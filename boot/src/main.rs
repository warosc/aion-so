#![no_std]
#![no_main]

mod console_vga;
mod memory;
mod panic;
mod power;

use console_vga::VgaConsole;
use power::UefiPower;
use uefi::boot;
use uefi::prelude::*;

#[entry]
fn efi_main() -> Status {
    uefi::helpers::init().unwrap();

    log::info!("AION OS - Fase 2 boot");

    // SAFETY: this is the only call site, on a single linear,
    // non-reentrant path. No Boot-Services-backed resource is held past
    // this point: the old `UefiConsole` (which wrapped
    // `system::with_stdin`/`with_stdout`, both documented to panic once
    // boot services exit) is gone entirely, replaced below by
    // `VgaConsole`, a direct hardware backend. `UefiPower` is unaffected —
    // `uefi::runtime` services are documented available both before and
    // after this call.
    let uefi_memory_map = unsafe { boot::exit_boot_services(None) };
    // The `log`/debugcon pipeline (port 0xE9) is unconditional in the
    // `uefi` crate's logger and keeps working unchanged across this
    // transition (verified against its source, not assumed) — so this
    // line proves the transition itself succeeded, independent of
    // anything that follows.
    log::info!("AION-PHASE2-POST-EXIT-OK");

    // SAFETY: writing 0xFF to the PIC's mask ports only reduces which IRQ
    // lines can reach the CPU; see `aion_arch_x86_64::pic::mask_all`'s own
    // SAFETY comment for the full justification. Called immediately after
    // exiting boot services and before anything else runs, so no
    // hardware IRQ (PIC-routed or otherwise) can land on the still-mostly
    // -empty IDT installed by Incremento 1 in between.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        aion_arch_x86_64::pic::mask_all();
    }

    let memory_map = memory::build_memory_map(&uefi_memory_map);
    let boot_info = aion_kernel::BootInfo { memory_map };
    let mut console = VgaConsole::new();
    let power = UefiPower;
    aion_kernel::kmain(&boot_info, &mut console, &power)
}
