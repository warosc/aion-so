//! Changing which process the CPU is running.
//!
//! The state of a process lives on its own kernel stack: whatever it had
//! in registers when it entered the kernel was pushed there, by the
//! interrupt trampoline or by the syscall stub. So switching is switching
//! stacks (docs/adr/0018-fase3-context-switch.md).
//!
//! `switch` saves the registers the ABI says a function must preserve,
//! writes `rsp` where the outgoing process keeps it, loads the incoming
//! one's, points CR3 at its tables and returns. What returns is the other
//! process, out of the `switch` it called last time.

use core::arch::naked_asm;

/// Swaps the running process.
///
/// # Safety
///
/// * `save_rsp_to` must be where the outgoing process keeps its kernel
///   stack pointer, and `resume_rsp` must be a stack this same function
///   left behind — or one `prepare_first_run` built.
/// * `cr3` must be the incoming process's page tables, whose higher half
///   maps this very code.
/// * Interrupts must be off: a tick in the middle would find a half-swapped
///   world.
/// * The caller must update `TSS.rsp0` and the per-CPU syscall stack to the
///   incoming process's before user code runs again.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(save_rsp_to: *mut u64, resume_rsp: u64, cr3: u64) {
    naked_asm!(
        // Microsoft x64: rcx = save_rsp_to, rdx = resume_rsp, r8 = cr3.
        // Everything the ABI says a callee keeps, so that the process
        // finds it where it left it.
        "push rbp",
        "push rbx",
        "push rdi",
        "push rsi",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rcx], rsp",
        "mov rsp, rdx",
        "mov cr3, r8",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rsi",
        "pop rdi",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// How many bytes a prepared stack holds: eight saved registers, the
/// address `switch` returns to, and the entry point that address reads.
pub const FIRST_RUN_FRAME: usize = 10 * 8;

/// Lays out a kernel stack so that the first `switch` into it ends up in
/// `entry`, with `argument` where the ABI passes the first parameter.
///
/// Returns the stack pointer to hand `switch` as `resume_rsp`.
///
/// The layout, from the top of the stack down, is what `switch` pops:
/// the entry point (read by the trampoline), the address it returns to
/// (the trampoline), and then `rbp`, `rbx`, `rdi`, `rsi`, `r12`..`r15`.
/// `argument` travels in the `rdi` slot because the trampoline is what
/// moves it to `rcx`.
///
/// # Safety
///
/// `stack_top` must be the top of a mapped, writable stack of the
/// kernel's own that nothing else uses, with at least `FIRST_RUN_FRAME`
/// bytes below it, and `entry` must never return.
pub unsafe fn prepare_first_run(
    stack_top: u64,
    entry: unsafe extern "C" fn(*mut u8) -> !,
    argument: *mut u8,
) -> u64 {
    let base = stack_top as *mut u64;
    // SAFETY: the caller vouches for the stack and its size.
    unsafe {
        base.sub(1).write(entry as *const () as u64); // the trampoline pops this
        base.sub(2).write(first_run_trampoline as *const () as u64); // `ret` jumps here
        base.sub(3).write(0); // rbp
        base.sub(4).write(0); // rbx
        base.sub(5).write(argument as u64); // rdi
        base.sub(6).write(0); // rsi
        base.sub(7).write(0); // r12
        base.sub(8).write(0); // r13
        base.sub(9).write(0); // r14
        base.sub(10).write(0); // r15
    }
    stack_top - FIRST_RUN_FRAME as u64
}

/// Moves the argument to where the ABI expects it and jumps to the entry
/// point, with the stack aligned the way a called function expects.
///
/// # Safety
///
/// Only reached by `switch` returning off a stack `prepare_first_run`
/// built.
#[unsafe(naked)]
unsafe extern "C" fn first_run_trampoline() -> ! {
    naked_asm!(
        // `rdi` came off the stack with the other saved registers; the
        // Microsoft ABI wants the first argument in `rcx`.
        "mov rcx, rdi",
        "pop rax",
        // A called function finds the stack 8 past 16-byte aligned, and
        // this one is jumped to. The address pushed can only fault, which
        // is right: the entry point never returns.
        "push 0",
        "jmp rax",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn never_called(_argument: *mut u8) -> ! {
        unreachable!("the test only ever looks at the address")
    }

    /// The prepared frame is read by ten instructions of assembly, so its
    /// layout is checked here rather than discovered in QEMU.
    #[test]
    fn a_prepared_stack_is_what_switch_expects_to_pop() {
        // A stand-in stack, aligned like a real one.
        let mut memory = std::vec![0u64; 64];
        let top = unsafe { memory.as_mut_ptr().add(memory.len()) };
        let argument = 0x1234_5678_9ABC_DEF0u64;

        // SAFETY: the buffer is this test's, big enough, and the entry is
        // never actually called.
        let rsp = unsafe { prepare_first_run(top as u64, never_called, argument as *mut u8) };

        // Pinned to where the last register sits, not to the constant:
        // otherwise a wrong constant would agree with itself.
        assert_eq!(
            rsp,
            &memory[memory.len() - 10] as *const u64 as u64,
            "the stack pointer points at the last register `switch` pops"
        );
        assert_eq!(top as u64 - rsp, FIRST_RUN_FRAME as u64);
        let slot = |from_top: usize| memory[memory.len() - from_top];
        assert_eq!(
            slot(1),
            never_called as *const () as u64,
            "the trampoline reads the entry point from the top slot"
        );
        assert_eq!(
            slot(2),
            first_run_trampoline as *const () as u64,
            "`ret` jumps to the trampoline"
        );
        assert_eq!(slot(3), 0, "rbp");
        assert_eq!(slot(4), 0, "rbx");
        assert_eq!(slot(5), argument, "rdi carries the argument");
        for from_top in 6..=10 {
            assert_eq!(slot(from_top), 0, "slot {from_top} is a cleared register");
        }
        // And the whole frame is accounted for: nothing below was touched.
        assert!(memory[..memory.len() - 10].iter().all(|word| *word == 0));
    }
}
