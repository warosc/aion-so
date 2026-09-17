#![no_std]

use aion_hal::CpuControl;

/// Grepped by `cargo xtask boot-test` in the QEMU debugcon capture.
/// Keep in sync with tools/xtask's default `--marker` value.
pub const BOOT_OK_MARKER: &str = "AION-PHASE0-BOOT-OK";

/// Placeholder boot handoff payload. Fase 1 replaces this with the real
/// contract (memory map, framebuffer, RSDP...) — that shape change is the
/// "sustitución del boot path" ADR trigger per ARCHITECTURE.md.
#[derive(Default)]
pub struct BootInfo {
    _private: (),
}

pub fn kmain(_boot_info: &BootInfo) -> ! {
    log::info!("{}", BOOT_OK_MARKER);

    #[cfg(target_arch = "x86_64")]
    {
        aion_arch_x86_64::Cpu.halt_loop()
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        loop {}
    }
}
