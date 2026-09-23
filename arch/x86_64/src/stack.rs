//! Switching the core onto a stack of the kernel's own.
//!
//! Until this runs, the kernel is still on the stack the firmware handed
//! it: 128 KiB of boot-services memory with nothing below it but more
//! firmware data, so an overflow would corrupt whatever sits there. The
//! kernel maps its own stack with unmapped guard pages around it
//! (`kernel::memory::stacks`) and jumps onto it here.

use core::arch::naked_asm;

/// Moves the core onto `top` and jumps to `entry(arg)`, which never
/// returns. The stack in use when this is called is left behind
/// untouched.
///
/// # Safety
///
/// - `top` must be the top (exclusive, 16-byte aligned) of a mapped,
///   writable stack that nothing else uses, with room for everything
///   `entry` will do.
/// - `arg` must stay valid for as long as `entry` uses it: it cannot point
///   into the stack frame being left behind unless that memory stays
///   mapped and untouched (as the firmware's stack does — the kernel never
///   reuses it).
/// - `entry` must never return.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_to(
    top: u64,
    entry: extern "C" fn(*mut u8) -> !,
    arg: *mut u8,
) -> ! {
    // Microsoft x64 (what the UEFI target uses, like the rest of this
    // crate's asm): top = RCX, entry = RDX, arg = R8.
    //
    // The stack is set up as if `entry` had been reached by a `call`: 32
    // bytes of shadow space for its own register arguments, then a fake
    // return address, leaving RSP ≡ 8 (mod 16) as the ABI expects at entry.
    // That fake address is 0, so an `entry` that returned anyway would
    // fault at once instead of running off into whatever was on the stack.
    naked_asm!(
        "mov rsp, rcx",
        "and rsp, -16",
        "sub rsp, 40",
        "mov qword ptr [rsp], 0",
        "mov rcx, r8",
        "jmp rdx",
    )
}
