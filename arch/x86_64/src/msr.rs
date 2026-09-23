//! Model-specific registers, read and written in one place.

use core::arch::asm;

pub const IA32_EFER: u32 = 0xC000_0080;
pub const IA32_STAR: u32 = 0xC000_0081;
pub const IA32_LSTAR: u32 = 0xC000_0082;
pub const IA32_FMASK: u32 = 0xC000_0084;
/// What `swapgs` swaps in: the kernel's per-CPU pointer while user code
/// runs.
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// # Safety
///
/// `msr` must exist on this CPU. Reading one that does not raises `#GP`.
pub unsafe fn read(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: forwarded from this function's contract; `rdmsr` has no side
    // effects.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    u64::from(low) | (u64::from(high) << 32)
}

/// # Safety
///
/// `msr` must exist on this CPU and `value` must be one it accepts:
/// writing a reserved bit, or an address that is not canonical, raises
/// `#GP`. What each register does to the machine is the caller's problem.
pub unsafe fn write(msr: u32, value: u64) {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nomem, nostack, preserves_flags));
    }
}
