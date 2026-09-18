#![cfg_attr(not(test), no_std)]

mod shell;

use aion_hal::{Console, PowerControl};

/// Grepped by `cargo xtask boot-test` in the QEMU debugcon capture.
/// Keep in sync with tools/xtask's default `--marker` value.
pub const BOOT_OK_MARKER: &str = "AION-PHASE0-BOOT-OK";

#[cfg(target_arch = "x86_64")]
pub const ARCH_NAME: &str = "x86_64";
#[cfg(not(target_arch = "x86_64"))]
pub const ARCH_NAME: &str = "unknown";

/// Placeholder boot handoff payload. Fase 1 still doesn't call
/// `ExitBootServices` or load a separate kernel image, so this stays
/// empty; see docs/adr/0001-fase0-boot-path.md and docs/fase1-notes.md.
#[derive(Default)]
pub struct BootInfo {
    _private: (),
}

pub fn kmain(_boot_info: &BootInfo, console: &mut dyn Console, power: &dyn PowerControl) -> ! {
    // SAFETY: called exactly once, as the first thing kmain does, before
    // any other arch-specific state is touched.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        aion_arch_x86_64::interrupts::init();
    }

    log::info!("{BOOT_OK_MARKER}");
    log::info!("AION: architecture = {ARCH_NAME}");
    log::info!("AION: GDT/IDT installed, breakpoint self-test OK");

    console.write_str(concat!("AION OS v", env!("CARGO_PKG_VERSION"), "\n"));
    console.write_str("Boot............ UEFI OK\n");
    console.write_str("Architecture.... ");
    console.write_str(ARCH_NAME);
    console.write_str("\n");
    console.write_str("Kernel.......... READY\n\n");

    shell::run_shell(console, power)
}
