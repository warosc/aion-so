//! Leaving the kernel for ring 3.
//!
//! `sysretq` is the other half of `syscall`: it takes the address to
//! continue at from `rcx` and the flags from `r11`, and loads `CS` and
//! `SS` from `STAR` with RPL 3. It does not set the stack pointer, so this
//! does, and it does not clear any register, so this does that too:
//! whatever the kernel left lying around would otherwise be readable by
//! the program (docs/adr/0014-fase3-syscall-abi-v0.md).

use core::arch::naked_asm;

/// Interrupts on, and bit 1, which is always set.
const USER_RFLAGS: u64 = 0x202;

/// Runs `entry` in ring 3 on `stack_top`, and never comes back here: the
/// only ways out are a syscall or a fault.
///
/// # Safety
///
/// * `syscall::init` must have run, with a handler set.
/// * `entry` and `stack_top` must be mapped for ring 3 in the page tables
///   in use, executable and writable respectively, and must belong to the
///   program being started.
/// * `stack_top` must be 16-byte aligned and its stack must be the
///   program's alone.
#[unsafe(naked)]
pub unsafe extern "C" fn enter(entry: u64, stack_top: u64) -> ! {
    naked_asm!(
        // Microsoft x64: rcx = entry, rdx = stack_top. `sysretq` wants the
        // address in rcx, which is where it already is.
        "mov rsp, rdx",
        "mov r11, {rflags}",
        // Nothing of the kernel's travels down with us.
        "xor eax, eax",
        "xor ebx, ebx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor ebp, ebp",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r12d, r12d",
        "xor r13d, r13d",
        "xor r14d, r14d",
        "xor r15d, r15d",
        "sysretq",
        rflags = const USER_RFLAGS,
    )
}
