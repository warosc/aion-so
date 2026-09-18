#![cfg_attr(not(test), no_std)]

mod gdt;
mod idt;
pub mod interrupts;

use aion_hal::{CpuControl, InterruptControl};

pub struct Cpu;

impl CpuControl for Cpu {
    fn halt_loop(&self) -> ! {
        loop {
            // SAFETY: `hlt` only pauses this core until the next interrupt;
            // `nomem, nostack` because it touches no memory/stack. No shared
            // state is mutated and this is the terminal state of the Fase 0
            // placeholder, so ordering/re-entrancy are not a concern. Safe to
            // execute with UEFI boot services still active: a firmware timer
            // interrupt simply wakes the core, which re-issues `hlt`.
            unsafe {
                core::arch::asm!("hlt", options(nomem, nostack));
            }
        }
    }

    fn halt_once(&self) {
        // SAFETY: same reasoning as `halt_loop` above, minus the loop: a
        // single `hlt` parks the core until the next interrupt (e.g. the
        // firmware timer) and then returns control here normally.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}

impl InterruptControl for Cpu {
    fn disable(&self) -> bool {
        let was_enabled = self.are_enabled();
        // SAFETY: `cli` only affects this core's interrupt flag; no memory
        // or stack side effects.
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
        was_enabled
    }

    fn enable(&self) {
        // SAFETY: `sti` only affects this core's interrupt flag; no memory
        // or stack side effects. The one-instruction delay before `sti`
        // takes effect (Intel SDM Vol. 3 §6.8.3) is not a correctness
        // concern for any caller in this codebase.
        unsafe {
            core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        }
    }

    fn are_enabled(&self) -> bool {
        let flags: u64;
        // SAFETY: `pushfq`/`pop` is a standard read-only read of RFLAGS;
        // it uses the stack (hence no `nostack`) but does not modify any
        // flag itself (`pop` does not affect flags), so `preserves_flags`
        // is accurate.
        unsafe {
            core::arch::asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
        }
        (flags & (1 << 9)) != 0 // IF is RFLAGS bit 9 (Intel SDM Vol. 1 §3.4.3)
    }
}
