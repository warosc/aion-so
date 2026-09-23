//! Thin, `uefi`-dependent glue that walks the real UEFI memory map
//! (obtained at the `ExitBootServices` transition) and classifies it into
//! `hal`'s firmware-agnostic `MemoryMap` — the boundary that keeps
//! `kernel` free of any dependency on the `uefi` crate.

use harlan_hal::addr::PhysAddr;
use harlan_hal::memory_map::{MemoryMap, MemoryRegion, classify_attributes, classify_memory_type};
use harlan_hal::warn;
use uefi::mem::memory_map::{MemoryMap as UefiMemoryMapTrait, MemoryMapOwned};

pub fn build_memory_map(uefi_map: &MemoryMapOwned) -> MemoryMap {
    let mut map = MemoryMap::new();
    let mut dropped = 0u32;
    for descriptor in uefi_map.entries() {
        let region = MemoryRegion {
            start_phys_addr: PhysAddr::new(descriptor.phys_start),
            page_count: descriptor.page_count,
            kind: classify_memory_type(descriptor.ty.0),
            attributes: classify_attributes(descriptor.att.bits()),
        };
        if !map.push(region) {
            dropped += 1;
        }
    }
    if dropped > 0 {
        // The map itself remembers this: the kernel refuses to unmap
        // anything on the strength of a map with a hole in it.
        warn!(
            "HARLAN: memory map truncated, {dropped} region(s) dropped (capacity {})",
            MemoryMap::CAPACITY
        );
    }
    map
}
