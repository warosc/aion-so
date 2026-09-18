#![cfg_attr(not(test), no_std)]

mod shell;

use aion_hal::memory_map::MemoryMap;
use aion_hal::{Console, PowerControl};

/// Grepped by `cargo xtask boot-test` in the QEMU debugcon capture.
/// Keep in sync with tools/xtask's default `--marker` value.
pub const BOOT_OK_MARKER: &str = "AION-PHASE0-BOOT-OK";

#[cfg(target_arch = "x86_64")]
pub const ARCH_NAME: &str = "x86_64";
#[cfg(not(target_arch = "x86_64"))]
pub const ARCH_NAME: &str = "unknown";

/// Boot handoff payload. Since Fase 2 Incremento 2, built from the real
/// UEFI memory map at the `ExitBootServices` transition — see
/// docs/adr/0002-fase2-exit-boot-services.md. `kernel` still has zero
/// dependency on the `uefi` crate: `MemoryMap` is `hal`'s own,
/// firmware-agnostic type.
pub struct BootInfo {
    pub memory_map: MemoryMap,
}

pub fn kmain(boot_info: &BootInfo, console: &mut dyn Console, power: &dyn PowerControl) -> ! {
    // SAFETY: called exactly once, as the first thing kmain does, before
    // any other arch-specific state is touched.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        aion_arch_x86_64::interrupts::init();
    }
    // SAFETY: called exactly once, immediately after `init()` above (GDT/
    // IDT already installed) and before anything else runs.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        aion_arch_x86_64::interrupts::init_timer();
    }

    log::info!("{BOOT_OK_MARKER}");
    log::info!("AION: architecture = {ARCH_NAME}");
    log::info!("AION: GDT/IDT installed, breakpoint self-test OK");
    log::info!(
        "AION: memory map = {} region(s), {} usable pages",
        boot_info.memory_map.len(),
        boot_info.memory_map.total_usable_pages()
    );

    console.write_str(concat!("AION OS v", env!("CARGO_PKG_VERSION"), "\n"));
    console.write_str("Boot............ UEFI OK\n");
    console.write_str("Architecture.... ");
    console.write_str(ARCH_NAME);
    console.write_str("\n");
    console.write_str("Kernel.......... READY\n\n");

    shell::run_shell(console, power)
}
