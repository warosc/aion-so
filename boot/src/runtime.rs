//! The one UEFI call the kernel cannot make for itself.
//!
//! `SetVirtualAddressMap` tells the firmware where its runtime services
//! will answer from now on, so that it can relocate its own pointers. Only
//! this crate speaks UEFI, and only the kernel knows where things are
//! going to live, so the kernel passes the base and this makes the call
//! (docs/adr/0016-fase3-set-virtual-address-map.md).
//!
//! Once per boot, and only while the firmware's old mappings are still
//! there.

use harlan_hal::addr::VirtAddr;
use harlan_hal::warn;
use uefi::mem::memory_map::{MemoryMap as _, MemoryMapMut, MemoryMapOwned};
use uefi_raw::table::boot::{MemoryAttribute, MemoryDescriptor};

/// How many runtime descriptors this can hand to the firmware. OVMF
/// reports six; going over is refused rather than truncated, because a
/// range left out is one the firmware would lose.
const MAX_RUNTIME: usize = 64;

/// The map `ExitBootServices` returned. Kept alive because the call needs
/// it, and because the kernel decides *when* to make it.
static mut MAP: Option<MemoryMapOwned> = None;

/// Keeps the map for later. Called once, right after the exit.
///
/// # Safety
///
/// Single-threaded boot path, before the kernel starts.
pub unsafe fn remember(map: MemoryMapOwned) {
    // SAFETY: forwarded from this function's contract.
    unsafe { MAP = Some(map) };
}

/// Tells the firmware its services now answer at `base + physical`, and
/// answers how many descriptors it moved.
///
/// # Safety
///
/// Every range carrying `EFI_MEMORY_RUNTIME` must already be mapped at
/// `base + its physical address`, with its own caching and execute
/// permission, and the firmware's old mappings must still be in place.
/// Called at most once per boot.
pub unsafe fn relocate(base: VirtAddr) -> Option<u64> {
    // SAFETY: single core, and the kernel calls this once.
    let map = unsafe { (&raw mut MAP).as_mut()?.as_mut()? };

    // The firmware is handed the runtime descriptors, with the virtual
    // address each one will answer at.
    let mut runtime = [MemoryDescriptor::default(); MAX_RUNTIME];
    let mut len = 0;
    for index in 0..map.len() {
        let descriptor = map.get_mut(index)?;
        if !descriptor.att.contains(MemoryAttribute::RUNTIME) {
            continue;
        }
        if len == MAX_RUNTIME {
            warn!("HARLAN: more runtime descriptors than this can carry; not moving any");
            return None;
        }
        descriptor.virt_start = base.as_u64() + descriptor.phys_start;
        runtime[len] = *descriptor;
        len += 1;
    }
    if len == 0 {
        warn!("HARLAN: the firmware claims no runtime memory; nothing to move");
        return None;
    }

    // Where the system table itself lands. The crate updates its own
    // pointer to it when the call succeeds, which is what keeps `reboot`
    // and `shutdown` working afterwards.
    let system_table = uefi::table::system_table_raw()?;
    let moved_table = (base.as_u64() + system_table.as_ptr() as u64) as *const _;

    // SAFETY: the descriptors are this map's own, the addresses in them
    // are mapped (the caller's contract) and this is the only call.
    match unsafe { uefi::runtime::set_virtual_address_map(&mut runtime[..len], moved_table) } {
        Ok(()) => Some(len as u64),
        Err(err) => {
            warn!("HARLAN: the firmware refused to relocate its services ({err:?})");
            None
        }
    }
}
