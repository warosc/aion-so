//! Whose turn it is.
//!
//! A process's state lives on its own kernel stack, so switching is
//! switching stacks: `arch::switch` saves the registers the ABI protects,
//! swaps `rsp`, points CR3 at the new tables and returns — as the other
//! process, out of the switch it made last time
//! (docs/adr/0018-fase3-context-switch.md).
//!
//! Round robin over a fixed list. With a handful of processes and a 100 Hz
//! timer, anything else would be decoration.
//!
//! Since there are messages (docs/adr/0019-fase3-ipc-v0.md) a process can
//! be alive and still not be given the CPU: one waiting for a message
//! keeps its slot and its memory and gets no turns until somebody writes
//! to its mailbox. `State::can_run` is where that distinction lives.

use harlan_hal::addr::VirtAddr;
use harlan_hal::frame::PhysRange;
use harlan_hal::{error, info};

use crate::ipc::{self, Delivery, Mailbox, TakeError};
use crate::process::Process;

/// How many processes there can be at once. Four is what Fase 3 runs; the
/// limit is here so that running out is an error and not a `Vec` growing
/// inside an interrupt handler.
pub const MAX_PROCESSES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Never run: its stack is prepared for a first switch.
    New,
    Runnable,
    /// Waiting for a message (ADR 0019). It exists, it is alive, and it
    /// does not get turns until somebody writes to its mailbox.
    Blocked,
    /// Left through `exit`. Its memory is gone; its slot stays so that
    /// nothing reuses its id while the log still mentions it.
    Dead,
}

impl State {
    /// Whether a process in this state can be handed the CPU.
    pub fn can_run(self) -> bool {
        matches!(self, State::New | State::Runnable)
    }

    /// What waiting for a message turns this state into, or `None` when a
    /// process in it has no business waiting: the dead, and one that is
    /// already waiting.
    pub fn waiting(self) -> Option<State> {
        self.can_run().then_some(State::Blocked)
    }

    /// And what a message arriving turns it into. Only a process that was
    /// waiting is woken: a message for one that is running changes
    /// nothing about whose turn it is.
    pub fn woken(self) -> Option<State> {
        (self == State::Blocked).then_some(State::Runnable)
    }
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
    /// Where a message sent to it waits. In the kernel's memory, so that
    /// it is reachable whichever space is active
    /// (docs/adr/0019-fase3-ipc-v0.md).
    pub mailbox: Mailbox,
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
        self.kill(self.current);
    }

    /// Marks one process as gone, wherever it is.
    pub fn kill(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index).and_then(Option::as_mut) {
            slot.state = State::Dead;
        }
    }

    /// Stops giving turns to the process in `index`, which is waiting for
    /// a message. Answers whether it was there to be stopped.
    pub fn block(&mut self, index: usize) -> bool {
        self.move_to(index, State::waiting)
    }

    /// Gives it turns again, a message having arrived.
    pub fn unblock(&mut self, index: usize) -> bool {
        self.move_to(index, State::woken)
    }

    /// Moves the process in `index` to wherever `transition` says, and
    /// answers whether there was one and it went.
    fn move_to(&mut self, index: usize, transition: fn(State) -> Option<State>) -> bool {
        match self.slots.get_mut(index).and_then(Option::as_mut) {
            Some(slot) => match transition(slot.state) {
                Some(state) => {
                    slot.state = state;
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// How many are waiting for a message.
    pub fn blocked(&self) -> usize {
        self.states()
            .iter()
            .flatten()
            .filter(|state| **state == State::Blocked)
            .count()
    }

    /// Who runs after `from`, skipping those that cannot. `None` when
    /// nobody else can.
    pub fn next_after(&self, from: usize) -> Option<usize> {
        next_runnable(&self.states(), from)
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

/// Who runs after `from`, skipping whoever cannot run — the dead and
/// whoever is waiting for a message — and wrapping round.
///
/// Pure, so that the order — the whole of the policy — is testable
/// without processes, page tables or stacks.
pub fn next_runnable(states: &[Option<State>], from: usize) -> Option<usize> {
    let len = states.len();
    (1..=len)
        .map(|step| (from + step) % len)
        .find(|&index| states[index].is_some_and(State::can_run))
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
        mailbox: Mailbox::new(),
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
    let scheduler = unsafe { the_scheduler() };
    scheduler.handed_over = false;
    // Nobody can run. Whoever is still waiting for a message would wait
    // for ever, so the kernel says so and gives it up (ADR 0019,
    // point 7); its memory goes back with the rest.
    for index in 0..MAX_PROCESSES {
        if scheduler.state(index) == Some(State::Blocked) {
            error!(
                "HARLAN: the process in slot {index} is still waiting for a message that will not come"
            );
            scheduler.kill(index);
        }
    }
    info!("HARLAN: every process is gone; the kernel has the CPU back");
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

/// The switch itself, from the process in `from` to the one in `to`.
/// `yield`, `exit` and waiting for a message all end here, so that there
/// is one place where a stack is left and another taken up.
///
/// # Safety
///
/// Both slots must hold a process, `to` must be able to run, and this must
/// be called with interrupts off from somewhere that can be resumed later:
/// a syscall handler or the timer handler.
unsafe fn switch_to(scheduler: &mut Scheduler, from: usize, to: usize) {
    scheduler.switching = true;
    scheduler.current = to;
    let (next_rsp, next_cr3) = {
        let slot = scheduler.slots[to].as_ref().expect("the slot just found");
        (slot.kernel_rsp, slot.process.space.root())
    };
    prepare_cpu_for(scheduler, to);
    let save_to = &raw mut scheduler.slots[from]
        .as_mut()
        .expect("the process leaving")
        .kernel_rsp;
    // SAFETY: both are kernel stacks of processes, `next_cr3` is the
    // incoming one's tables — whose higher half holds this code — and
    // interrupts are off.
    unsafe { harlan_arch_x86_64::switch::switch(save_to, next_rsp, next_cr3) };
    // Back here as `from`, whenever its turn comes round again. The
    // borrow above went with the other stack, so this asks again.
    // SAFETY: as above.
    unsafe { the_scheduler() }.switching = false;
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
    // SAFETY: as this function's contract; `next` can run and is not
    // `current`.
    unsafe { switch_to(scheduler, current, next) };
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
            // SAFETY: as in `switch_to_next`. This one never comes back:
            // the process is dead and nothing switches into it again.
            unsafe { switch_to(scheduler, current, next) };
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

// ---------------------------------------------------------------------
// Messages between processes (docs/adr/0019-fase3-ipc-v0.md)
// ---------------------------------------------------------------------

/// What the syscall handler needs to know about the process it is
/// serving: which slot it is, and what memory it owns. Copied out rather
/// than borrowed, so that the handler can go on to touch the scheduler —
/// and so that the checks it makes are testable without a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running {
    pub slot: usize,
    pub ranges: [PhysRange; 2],
}

impl Running {
    /// Whether `[ptr, ptr + len)` is memory this process owns (ADR 0014,
    /// point 9).
    pub fn owns(&self, ptr: u64, len: u64) -> bool {
        crate::process::owned_by(&self.ranges, ptr, len)
    }
}

/// Who has the CPU, if anyone. The one answer to that question: a second
/// copy of it somewhere else is a copy that goes stale on the next switch.
///
/// # Safety
///
/// As `the_scheduler`.
pub unsafe fn running() -> Option<Running> {
    // SAFETY: forwarded from this function's contract.
    let scheduler = unsafe { the_scheduler() };
    if !scheduler.handed_over {
        return None;
    }
    let slot = scheduler.current;
    let held = scheduler.slots[slot].as_ref()?;
    if held.state == State::Dead {
        return None;
    }
    Some(Running {
        slot,
        ranges: [held.process.code, held.process.stack],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// Nothing in that slot, or what was there has exited.
    NoSuchProcess,
    /// Its mailbox already holds a message nobody has read.
    Busy,
    /// Not a message a mailbox can hold.
    Rejected(ipc::DeliverError),
}

/// Puts `message` in the mailbox of the process in `to`, and gives it
/// turns again if it was waiting for one.
///
/// # Safety
///
/// As `the_scheduler`. `message` must be readable: the sender's own
/// memory, checked by the caller, in the space that is active.
pub unsafe fn deliver(to: usize, from: usize, message: &[u8]) -> Result<Delivery, SendError> {
    // SAFETY: forwarded from this function's contract.
    let scheduler = unsafe { the_scheduler() };
    let slot = scheduler
        .slots
        .get_mut(to)
        .and_then(Option::as_mut)
        .ok_or(SendError::NoSuchProcess)?;
    if slot.state == State::Dead {
        return Err(SendError::NoSuchProcess);
    }
    let delivery = slot
        .mailbox
        .deliver(from, message)
        .map_err(|err| match err {
            ipc::DeliverError::Busy => SendError::Busy,
            other => SendError::Rejected(other),
        })?;
    // One way to wake a process, wherever the waking comes from.
    scheduler.unblock(to);
    Ok(delivery)
}

/// Takes the message waiting for whoever is running into `into`.
///
/// # Safety
///
/// As `the_scheduler`. `into` must be writable memory of the running
/// process, checked by the caller, in the space that is active.
pub unsafe fn take_message(into: &mut [u8]) -> Result<Delivery, TakeError> {
    // SAFETY: forwarded from this function's contract.
    let scheduler = unsafe { the_scheduler() };
    let current = scheduler.current;
    match scheduler.slots[current].as_mut() {
        Some(slot) => slot.mailbox.take_into(into),
        None => Err(TakeError::Nothing),
    }
}

/// Parks whoever is running until somebody writes to its mailbox, and
/// comes back once somebody has.
///
/// Answers `false`, having parked nobody, when nobody else could ever
/// write it: a wait with no end is worse than an error (ADR 0019,
/// point 7).
///
/// # Safety
///
/// From the syscall handler, with interrupts off, while a process is
/// running.
pub unsafe fn wait_for_message() -> bool {
    // SAFETY: as this function's contract.
    let scheduler = unsafe { the_scheduler() };
    let current = scheduler.current;
    match scheduler.next_after(current) {
        Some(next) if next != current => {
            if !scheduler.block(current) {
                return false;
            }
            // SAFETY: as this function's contract; `next` can run, and it
            // is not the process being parked.
            unsafe { switch_to(scheduler, current, next) };
            true
        }
        // Nobody else can run, so nobody could ever deliver.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harlan_hal::addr::PhysAddr;
    use harlan_hal::paging::PAGE_SIZE;

    /// The turn goes round, skips whoever cannot run, and comes back.
    #[test]
    fn the_turn_goes_round_and_skips_whoever_cannot_run() {
        let mut states = [None; MAX_PROCESSES];
        states[0] = Some(State::Runnable);
        states[2] = Some(State::New);
        states[5] = Some(State::Dead);

        assert_eq!(next_runnable(&states, 0), Some(2));
        assert_eq!(
            next_runnable(&states, 2),
            Some(0),
            "round, not forward only"
        );
        assert_eq!(next_runnable(&states, 5), Some(0), "the dead do not run");
        assert_eq!(next_runnable(&states, 7), Some(0));

        // A process still gets its own turn when it is the last one.
        let mut alone = [None; MAX_PROCESSES];
        alone[3] = Some(State::Runnable);
        assert_eq!(next_runnable(&alone, 3), Some(3));

        // And when there is nobody, there is nobody.
        assert_eq!(next_runnable(&[None; MAX_PROCESSES], 0), None);
        let all_dead = [Some(State::Dead); MAX_PROCESSES];
        assert_eq!(next_runnable(&all_dead, 0), None);
    }

    /// Waiting for a message is being alive and not being runnable, which
    /// is the distinction the whole of IPC rests on: a blocked process
    /// keeps its memory and its slot, and gets no turns.
    #[test]
    fn whoever_waits_for_a_message_gets_no_turns() {
        let mut states = [None; MAX_PROCESSES];
        states[0] = Some(State::Runnable);
        states[1] = Some(State::Blocked);
        states[2] = Some(State::Runnable);

        assert_eq!(next_runnable(&states, 0), Some(2), "1 is waiting");
        assert_eq!(next_runnable(&states, 2), Some(0));
        // Even asked directly, and even as the only one left.
        assert_eq!(next_runnable(&states, 1), Some(2));
        let mut only_waiting = [None; MAX_PROCESSES];
        only_waiting[4] = Some(State::Blocked);
        assert_eq!(
            next_runnable(&only_waiting, 4),
            None,
            "nobody can run, which is what makes it a deadlock to report"
        );

        assert!(State::New.can_run());
        assert!(State::Runnable.can_run());
        assert!(!State::Blocked.can_run());
        assert!(!State::Dead.can_run());
    }

    /// Who may start waiting, and who may be woken. A process that needs
    /// a slot to exist cannot be tested here, so the rule itself is kept
    /// where it can be.
    #[test]
    fn only_some_states_can_start_or_stop_waiting() {
        assert_eq!(State::New.waiting(), Some(State::Blocked));
        assert_eq!(State::Runnable.waiting(), Some(State::Blocked));
        assert_eq!(
            State::Blocked.waiting(),
            None,
            "already waiting: waiting twice would park it for ever"
        );
        assert_eq!(State::Dead.waiting(), None, "the dead do not wait");

        assert_eq!(State::Blocked.woken(), Some(State::Runnable));
        assert_eq!(State::New.woken(), None, "it had not started waiting");
        assert_eq!(State::Runnable.woken(), None);
        assert_eq!(
            State::Dead.woken(),
            None,
            "a message does not bring anyone back"
        );
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
        assert_eq!(scheduler.blocked(), 0);
        assert_eq!(scheduler.state(0), None);
    }

    /// Blocking, unblocking and killing a slot that is not there must
    /// answer no rather than reaching past the array.
    #[test]
    fn a_slot_that_is_not_there_cannot_be_blocked_or_woken() {
        let mut scheduler = Scheduler::new();
        assert!(!scheduler.block(0));
        assert!(!scheduler.unblock(0));
        assert!(!scheduler.block(MAX_PROCESSES));
        assert!(!scheduler.unblock(MAX_PROCESSES + 100));
        scheduler.kill(MAX_PROCESSES);
        assert_eq!(scheduler.state(0), None);
    }

    /// What the syscall handler checks every pointer against, without a
    /// process — which would need page tables.
    #[test]
    fn the_handler_only_trusts_the_running_process_s_own_memory() {
        let running = Running {
            slot: 1,
            ranges: [
                PhysRange::new(PhysAddr::new(0x0040_0000), PAGE_SIZE),
                PhysRange::new(PhysAddr::new(0x0050_0000), PAGE_SIZE),
            ],
        };
        assert!(running.owns(0x0040_0000, 8), "its code");
        assert!(running.owns(0x0050_0000, PAGE_SIZE), "its whole stack");
        assert!(!running.owns(0x0040_0000, PAGE_SIZE + 1), "past the page");
        assert!(!running.owns(0x0045_0000, 8), "the gap between the two");
        assert!(!running.owns(0xFFFF_8000_0000_0000, 8), "the kernel's half");
        assert!(!running.owns(0x0040_0000, 0), "nothing at all");
    }
}
