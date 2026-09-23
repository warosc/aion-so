#![no_std]
#![no_main]

mod console_hw;
mod framebuffer;
mod image;
mod memory;
mod panic;
mod power;

use console_hw::HardwareConsole;
use harlan_hal::info;
use power::UefiPower;
use uefi::boot;
use uefi::prelude::*;

#[entry]
fn efi_main() -> Status {
    // The kernel's log sink, from the first line: the `log` crate's
    // logger can only be registered once, and both halves of that pointer
    // would name this image, which the kernel later stops mapping
    // (docs/adr/0013-fase3-physical-window.md).
    harlan_kernel::klog::install();
    uefi::helpers::init().unwrap();

    info!("HARLAN OS - Fase 2 boot");

    // The GOP protocol is a Boot Services object: it can only be queried
    // now. What it describes (the framebuffer memory) outlives the exit.
    let framebuffer = framebuffer::query();
    let framebuffer_range =
        framebuffer.map(|info| harlan_hal::frame::PhysRange::new(info.base_addr, info.size_bytes));
    // Also a Boot Services question: the kernel marks this range executable
    // and everything else no-execute when it builds its own page tables.
    let kernel_image = image::query_with_code();

    // SAFETY: this is the only call site, on a single linear,
    // non-reentrant path. No Boot-Services-backed resource is held past
    // this point: the old `UefiConsole` (which wrapped
    // `system::with_stdin`/`with_stdout`, both documented to panic once
    // boot services exit) is gone entirely, replaced below by
    // `HardwareConsole`, a direct hardware backend, and the GOP protocol
    // handle `framebuffer::query` opened is already closed. `UefiPower` is
    // unaffected — `uefi::runtime` services are documented available both
    // before and after this call.
    let uefi_memory_map = unsafe { boot::exit_boot_services(None) };
    // The `log`/debugcon pipeline (port 0xE9) is unconditional in the
    // `uefi` crate's logger and keeps working unchanged across this
    // transition (verified against its source, not assumed) — so this
    // line proves the transition itself succeeded, independent of
    // anything that follows.
    info!("HARLAN-PHASE2-POST-EXIT-OK");

    // SAFETY: writing 0xFF to the PIC's mask ports only reduces which IRQ
    // lines can reach the CPU; see `harlan_arch_x86_64::pic::mask_all`'s own
    // SAFETY comment for the full justification. Called immediately after
    // exiting boot services and before anything else runs, so no
    // hardware IRQ (PIC-routed or otherwise) can land on the still-mostly
    // -empty IDT installed by Incremento 1 in between.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        harlan_arch_x86_64::pic::mask_all();
    }

    let memory_map = memory::build_memory_map(&uefi_memory_map);
    let boot_info = harlan_kernel::BootInfo {
        memory_map,
        kernel_image: kernel_image.as_ref().map(|image| image.range),
        kernel_code: kernel_image.as_ref().and_then(|image| image.code),
        framebuffer: framebuffer_range,
    };
    // SAFETY: `framebuffer` came from the firmware's GOP for the mode that
    // was current when it was queried, and nothing changes the display
    // mode after that (boot services are gone). Firmware's own console
    // stopped drawing at the exit, and this is the only writer from here
    // on. The memory stays mapped: UEFI's identity mapping is still what's
    // live (no page tables are touched by exiting boot services).
    let console = unsafe { HardwareConsole::new(framebuffer) };
    // The kernel takes ownership of all three: it moves them into its own
    // heap as soon as it has one, so that nothing of its own is left in the
    // firmware's memory (docs/adr/0007-fase2-own-memory.md).
    harlan_kernel::kmain(boot_info, console, UefiPower)
}
