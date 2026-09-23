//! Whose turn it is.
//!
//! A process's state lives on its own kernel stack, so switching is
//! switching stacks: `arch::switch` saves the registers the ABI protects,
//! swaps `rsp`, points CR3 at the new tables and returns — as the other
//! process, out of the switch it made last time
//! (docs/adr/0018-fase3-context-switch.md).
//!
//! Round robin over a fixed list. With two processes and a 100 Hz timer,
//! anything else would be decoration.

use harlan_hal::addr::VirtAddr;
use harlan_hal::info;

use crate::process::Process;

/// How many processes there can be at once. Two is what Fase 3 needs; the
/// limit is here so that running out is an error and not a `Vec` growing
/// inside an interrupt handler.
pub const MAX_PROCESSES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Never run: its stack is prepared for a first switch.
    New,
    Runnable,
    /// Left through `exit`. Its memory is gone; its slot stays so that
    /// nothing reuses its id while the log still mentions it.
    Dead,
}

/// What the scheduler keeps about each process.
pub struct Slot {
    pub process: &'static mut Process,
    pub state: State,
    /// Where its kernel stack pointer is while it is not running.
    pub kernel_rsp: u64,
    /// The top of that stack, which is what the CPU needs to know when it
    /// enters the kernel from ring 3.
    pub kernel_stack_top: VirtAddr,
}

/// The whole scheduler: who exists, and who is running.
pub struct Scheduler {
    slots: [Option<Slot>; MAX_PROCESSES],
    current: usize,
    /// Set while a switch is in flight, so that a tick arriving in the
    /// middle is ignored rather than nesting.
    switching: bool,
    /// Whether a process has the CPU. A tick before the kernel has handed
    /// it over must do nothing: switching then would save the kernel's
    /// stack pointer into a process's slot and lose it.
    handed_over: bool,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; MAX_PROCESSES],
            current: 0,
            switching: false,
            handed_over: false,
        }
    }

    /// Adds a process, and answers which slot it took.
    pub fn add(&mut self, slot: Slot) -> Option<usize> {
        let free = self.slots.iter().position(Option::is_none)?;
        self.slots[free] = Some(slot);
        Some(free)
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn process(&self, index: usize) -> Option<&Process> {
        self.slots[index].as_ref().map(|slot| &*slot.process)
    }

    pub fn state(&self, index: usize) -> Option<State> {
        self.slots[index].as_ref().map(|slot| slot.state)
    }

    /// Marks the running process as gone.
    pub fn kill_current(&mut self) {
        if let Some(slot) = self.slots[self.current].as_mut() {
            slot.state = State::Dead;
        }
    }

    /// Who runs after `from`, skipping the dead. `None` when nobody else
    /// can.
    pub fn next_after(&self, from: usize) -> Option<usize> {
        next_alive(&self.states(), from)
    }

    /// How many processes are still alive.
    pub fn alive(&self) -> usize {
        self.states()
            .iter()
            .flatten()
            .filter(|state| **state != State::Dead)
            .count()
    }

    /// Whether a timer tick should hand the CPU on. Not while the kernel
    /// still has it —switching then would save the kernel's own stack
    /// pointer into a process's slot and lose it— and not in the middle
    /// of another switch.
    pub fn should_switch_on_tick(&self) -> bool {
        self.handed_over && !self.switching
    }

    fn states(&self) -> [Option<State>; MAX_PROCESSES] {
        let mut states = [None; MAX_PROCESSES];
        for (index, slot) in self.slots.iter().enumerate() {
            states[index] = slot.as_ref().map(|slot| slot.state);
        }
        states
    }
}

/// Who runs after `from`, skipping the dead and wrapping round.
///
/// Pure, so that the order — the whole of the policy — is testable
/// without processes, page tables or stacks.
pub fn next_alive(states: &[Option<State>], from: usize) -> Option<usize> {
    let len = states.len();
    (1..=len)
        .map(|step| (from + step) % len)
        .find(|&index| states[index].is_some_and(|state| state != State::Dead))
}

// ---------------------------------------------------------------------
// The one scheduler, and the switching
// ---------------------------------------------------------------------

/// The kernel has exactly one. Reached from the syscall handler and from
/// the timer, both of which run with interrupts off on a single core, and
/// from the kernel's own flow while no process is running.
static mut SCHEDULER: Scheduler = Scheduler::new();

/// Where the kernel's own stack pointer is kept while a process runs, so
/// that the last process to exit can hand control back.
static mut KERNEL_RSP: u64 = 0;
/// And its page tables, to go back to.
static mut KERNEL_CR3: u64 = 0;

/// # Safety
///
/// Single core, and only from somewhere no other user of the scheduler
/// can interrupt: the syscall handler, the timer handler, or the kernel
/// while no process runs.
unsafe fn the_scheduler() -> &'static mut Scheduler {
    let scheduler: *mut Scheduler = &raw mut SCHEDULER;
    // SAFETY: forwarded from this function's contract.
    unsafe { &mut *scheduler }
}

/// Adds a process, prepares its stack for a first run, and answers its
/// slot.
///
/// # Safety
///
/// `process` must be one `spawn` built, with a kernel stack of its own
/// that nothing else uses.
pub unsafe fn add(process: &'static mut Process) -> Option<usize> {
    let kernel_stack_top = process.kernel_stack.top();
    // SAFETY: the stack is the process's own and has room; `run_first`
    // never returns.
    let kernel_rsp = unsafe {
        harlan_arch_x86_64::switch::prepare_first_run(
            kernel_stack_top.as_u64(),
            run_first,
            core::ptr::null_mut(),
        )
    };
    // SAFETY: as this function's contract.
    let scheduler = unsafe { the_scheduler() };
    scheduler.add(Slot {
        process,
        state: State::New,
        kernel_rsp,
        kernel_stack_top,
    })
}

/// Runs processes until none is left, then returns.
///
/// # Safety
///
/// The kernel must be in its own space, on its own stack, with no process
/// running.
pub unsafe fn run_until_empty(kernel_cr3: u64) {
    // SAFETY: no process is running (the caller's contract).
    let scheduler = unsafe { the_scheduler() };
    let Some(first) = scheduler.next_after(MAX_PROCESSES - 1) else {
        info!("HARLAN: there is no process to run");
        return;
    };
    scheduler.current = first;
    let (rsp, cr3) = {
        let slot = scheduler.slots[first]
            .as_ref()
            .expect("the slot just found");
        (slot.kernel_rsp, slot.process.space.root())
    };
    // SAFETY: single core, and nothing else writes these while no process
    // is running.
    unsafe { KERNEL_CR3 = kernel_cr3 };
    prepare_cpu_for(scheduler, first);
    info!("HARLAN: handing the CPU to the process in slot {first}");
    scheduler.handed_over = true;
    // SAFETY: `rsp` is the stack `add` prepared, `cr3` is that process's
    // tables — whose higher half holds this code — and interrupts are off
    // around the swap.
    unsafe { harlan_arch_x86_64::switch::switch(&raw mut KERNEL_RSP, rsp, cr3) };
    // SAFETY: back in the kernel, with no process running.
    unsafe { the_scheduler() }.handed_over = false;
    info!("HARLAN: every process has exited; the kernel has the CPU back");
}

/// Tells the CPU where the process in `index` enters the kernel. The
/// interrupt path and the syscall path both need it, and forgetting one
/// means the next entry writes on somebody else's stack.
fn prepare_cpu_for(scheduler: &Scheduler, index: usize) {
    let Some(slot) = scheduler.slots[index].as_ref() else {
        return;
    };
    // SAFETY: the stack belongs to the process that is about to run, and
    // stays mapped for as long as it lives.
    unsafe {
        harlan_arch_x86_64::set_kernel_stack(slot.kernel_stack_top);
        harlan_arch_x86_64::syscall::set_kernel_stack(slot.kernel_stack_top.as_u64());
    }
}

/// Gives the CPU to whoever is next, and comes back when this process's
/// turn comes round again.
///
/// # Safety
///
/// From the syscall or timer handler, with interrupts off, while a
/// process is running.
pub unsafe fn switch_to_next() {
    // SAFETY: as this function's contract.
    let scheduler = unsafe { the_scheduler() };
    if scheduler.switching {
        return;
    }
    let current = scheduler.current;
    let Some(next) = scheduler.next_after(current) else {
        // SAFETY: as above.
        unsafe { leave_for_the_kernel(scheduler, current) };
        return;
    };
    if next == current {
        return;
    }
    scheduler.switching = true;
    scheduler.current = next;
    let (next_rsp, next_cr3) = {
        let slot = scheduler.slots[next].as_ref().expect("the slot just found");
        (slot.kernel_rsp, slot.process.space.root())
    };
    prepare_cpu_for(scheduler, next);
    let save_to = &raw mut scheduler.slots[current]
        .as_mut()
        .expect("the running process")
        .kernel_rsp;
    // SAFETY: both are kernel stacks of processes, `next_cr3` is the
    // incoming one's tables — whose higher half holds this code — and
    // interrupts are off.
    unsafe { harlan_arch_x86_64::switch::switch(save_to, next_rsp, next_cr3) };
    // Back here as `current`, whenever its turn comes round again. The
    // binding above is gone with the other stack, so this asks again.
    // SAFETY: as above.
    unsafe { the_scheduler() }.switching = false;
}

/// Marks the running process as gone and gives the CPU away for good.
///
/// # Safety
///
/// As `switch_to_next`.
pub unsafe fn exit_current(code: u64) {
    // SAFETY: as this function's contract.
    let scheduler = unsafe { the_scheduler() };
    let current = scheduler.current;
    scheduler.kill_current();
    info!("HARLAN: the process in slot {current} exited with {code}");
    match scheduler.next_after(current) {
        Some(next) if next != current => {
            scheduler.current = next;
            let (next_rsp, next_cr3) = {
                let slot = scheduler.slots[next].as_ref().expect("the slot just found");
                (slot.kernel_rsp, slot.process.space.root())
            };
            prepare_cpu_for(scheduler, next);
            let save_to = &raw mut scheduler.slots[current]
                .as_mut()
                .expect("the running process")
                .kernel_rsp;
            // SAFETY: as in `switch_to_next`. This one never returns: the
            // process is dead and nothing switches back into it.
            unsafe { harlan_arch_x86_64::switch::switch(save_to, next_rsp, next_cr3) };
        }
        _ => {
            // SAFETY: as above.
            unsafe { leave_for_the_kernel(scheduler, current) };
        }
    }
}

/// Hands the CPU back to the kernel's own flow, waiting inside
/// `run_until_empty`.
///
/// # Safety
///
/// As `switch_to_next`.
unsafe fn leave_for_the_kernel(scheduler: &mut Scheduler, current: usize) {
    let save_to = match scheduler.slots[current].as_mut() {
        Some(slot) => &raw mut slot.kernel_rsp,
        None => return,
    };
    // SAFETY: `KERNEL_RSP` is where `run_until_empty` left the kernel's
    // own stack, and `KERNEL_CR3` its tables.
    unsafe { harlan_arch_x86_64::switch::switch(save_to, KERNEL_RSP, KERNEL_CR3) };
}

/// What the timer calls.
///
/// # Safety
///
/// From the timer handler, with interrupts off.
pub unsafe fn on_tick() {
    // SAFETY: as this function's contract.
    let scheduler = unsafe { the_scheduler() };
    if scheduler.should_switch_on_tick() {
        // SAFETY: as above.
        unsafe { switch_to_next() };
    }
}

/// The processes that have exited, so that the kernel can give their
/// memory back once it has the CPU.
///
/// # Safety
///
/// No process may be running.
pub unsafe fn dead_processes() -> impl Iterator<Item = &'static Process> {
    // SAFETY: as this function's contract.
    let scheduler = unsafe { the_scheduler() };
    scheduler
        .slots
        .iter()
        .flatten()
        .filter(|slot| slot.state == State::Dead)
        .map(|slot| {
            let process: *const Process = slot.process;
            // SAFETY: the process is dead, so nothing runs on it and
            // nothing else holds it.
            unsafe { &*process }
        })
}

/// Where a process runs for the first time: straight into ring 3.
///
/// # Safety
///
/// Only `switch` reaches this, off a stack `add` prepared.
unsafe extern "C" fn run_first(_argument: *mut u8) -> ! {
    // SAFETY: this process is the one running, interrupts are off, and
    // this is the scheduler's own state.
    let scheduler = unsafe { the_scheduler() };
    scheduler.switching = false;
    let current = scheduler.current;
    let slot = scheduler.slots[current]
        .as_mut()
        .expect("the process just switched into");
    slot.state = State::Runnable;
    let process: *const Process = slot.process;
    // SAFETY: the slot owns the process for as long as it lives, and
    // nothing else holds it while it runs.
    let process: &'static Process = unsafe { &*process };
    // SAFETY: the space is this process's, its pages are mapped for ring
    // 3, and the syscall path was pointed at its stack before the switch.
    unsafe { crate::user::enter(process) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The turn goes round, skips the dead, and comes back.
    #[test]
    fn the_turn_goes_round_and_skips_the_dead() {
        let mut states = [None; MAX_PROCESSES];
        states[0] = Some(State::Runnable);
        states[2] = Some(State::New);
        states[5] = Some(State::Dead);

        assert_eq!(next_alive(&states, 0), Some(2));
        assert_eq!(next_alive(&states, 2), Some(0), "round, not forward only");
        assert_eq!(next_alive(&states, 5), Some(0), "the dead do not run");
        assert_eq!(next_alive(&states, 7), Some(0));

        // A process still gets its own turn when it is the last one.
        let mut alone = [None; MAX_PROCESSES];
        alone[3] = Some(State::Runnable);
        assert_eq!(next_alive(&alone, 3), Some(3));

        // And when there is nobody, there is nobody.
        assert_eq!(next_alive(&[None; MAX_PROCESSES], 0), None);
        let all_dead = [Some(State::Dead); MAX_PROCESSES];
        assert_eq!(next_alive(&all_dead, 0), None);
    }

    /// A tick that arrives before the kernel has handed the CPU over, or
    /// in the middle of a switch, must do nothing.
    #[test]
    fn a_tick_only_switches_while_a_process_has_the_cpu() {
        let mut scheduler = Scheduler::new();
        assert!(!scheduler.should_switch_on_tick(), "the kernel has it");

        scheduler.handed_over = true;
        assert!(scheduler.should_switch_on_tick());

        scheduler.switching = true;
        assert!(!scheduler.should_switch_on_tick(), "mid-switch");

        scheduler.switching = false;
        scheduler.handed_over = false;
        assert!(!scheduler.should_switch_on_tick(), "back in the kernel");
    }

    #[test]
    fn a_new_scheduler_has_nobody() {
        let scheduler = Scheduler::new();
        assert_eq!(scheduler.next_after(0), None);
        assert_eq!(scheduler.alive(), 0);
        assert_eq!(scheduler.state(0), None);
    }
}
