#![no_std]
#![no_main]

use uefi::prelude::*;

#[entry]
fn efi_main() -> Status {
    uefi::helpers::init().unwrap();

    log::info!("AION OS - Fase 0 boot placeholder");

    let boot_info = aion_kernel::BootInfo::default();
    aion_kernel::kmain(&boot_info)
}
