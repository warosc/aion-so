#![no_std]
#![no_main]

mod console;
mod panic;
mod power;

use console::UefiConsole;
use power::UefiPower;
use uefi::boot::{self, MemoryType, PAGE_SIZE};
use uefi::mem::memory_map::MemoryMap;
use uefi::prelude::*;

#[entry]
fn efi_main() -> Status {
    uefi::helpers::init().unwrap();

    log::info!("AION OS - Fase 1 boot");

    match boot::memory_map(MemoryType::LOADER_DATA) {
        Ok(map) => {
            let descriptor_count = map.len();
            let total_pages: u64 = map.entries().map(|d| d.page_count).sum();
            let total_bytes = total_pages * PAGE_SIZE as u64;
            log::info!(
                "AION: memory map = {descriptor_count} descriptors, {total_pages} pages ({total_bytes} bytes)"
            );
        }
        Err(e) => log::warn!("AION: failed to read UEFI memory map: {e:?}"),
    }

    let boot_info = aion_kernel::BootInfo::default();
    let mut console = UefiConsole;
    let power = UefiPower;
    aion_kernel::kmain(&boot_info, &mut console, &power)
}
