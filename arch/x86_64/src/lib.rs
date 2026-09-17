#![no_std]

use aion_hal::CpuControl;

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
}
