//! Firmware-agnostic memory map. `boot` (which depends on the `uefi`
//! crate) builds one of these from the real UEFI memory map at the
//! `ExitBootServices` transition; `kernel` consumes it without ever
//! depending on `uefi` itself.

/// Coarse classification of a memory region: only as fine as some consumer
/// needs (today, the kernel's frame allocator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRegionKind {
    /// Free RAM (UEFI conventional memory): nothing lives there.
    Usable,
    /// UEFI boot-services code and data. The firmware no longer needs it
    /// after `ExitBootServices`, but the kernel still does: on OVMF the
    /// stack it runs on and every page table in the live `CR3` hierarchy
    /// sit in boot-services data (observed, see docs/fase2-notes.md,
    /// Incremento 5). Kept apart from `Usable` so it is not handed out
    /// before the kernel owns its own stack and page tables.
    BootServices,
    /// Firmware code that keeps running after `ExitBootServices` (the UEFI
    /// runtime services). Never allocatable, and the only firmware memory
    /// the kernel still executes, so its pages must stay executable when
    /// the kernel builds its own page tables.
    RuntimeCode,
    Reserved,
}

use crate::addr::PhysAddr;

#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub start_phys_addr: PhysAddr,
    pub page_count: u64,
    pub kind: MemoryRegionKind,
}

/// Fixed-capacity memory map: no heap exists yet when this is built (the
/// heap is built *using* this map, in Incremento 7), so a `Vec` is not an
/// option.
#[derive(Clone, Copy)]
pub struct MemoryMap {
    regions: [MemoryRegion; Self::CAPACITY],
    len: usize,
}

impl MemoryMap {
    /// Sized with headroom over what this project has actually observed
    /// on its QEMU/OVMF target (104 descriptors as of Fase 1 — see
    /// docs/fase2-notes.md). A real descriptor count above this capacity
    /// is handled by truncating with a warning (`push` returns `false`),
    /// never by panicking.
    pub const CAPACITY: usize = 256;

    pub const fn new() -> Self {
        const EMPTY: MemoryRegion = MemoryRegion {
            start_phys_addr: PhysAddr::new(0),
            page_count: 0,
            kind: MemoryRegionKind::Reserved,
        };
        Self {
            regions: [EMPTY; Self::CAPACITY],
            len: 0,
        }
    }

    /// Returns `false` (without adding the region) once the map is at
    /// capacity, so the caller can log a warning and keep going rather
    /// than panic or silently corrupt an array index.
    #[must_use]
    pub fn push(&mut self, region: MemoryRegion) -> bool {
        if self.len >= Self::CAPACITY {
            return false;
        }
        self.regions[self.len] = region;
        self.len += 1;
        true
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &MemoryRegion> {
        self.regions[..self.len].iter()
    }

    pub fn total_usable_pages(&self) -> u64 {
        self.total_pages(MemoryRegionKind::Usable)
    }

    pub fn total_pages(&self, kind: MemoryRegionKind) -> u64 {
        self.iter()
            .filter(|r| r.kind == kind)
            .map(|r| r.page_count)
            .sum()
    }
}

impl Default for MemoryMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Raw UEFI `MemoryType` ordinals this classifier cares about (see the
/// UEFI spec / the `uefi` crate's `MemoryType` for the full list). Named
/// here, rather than depending on the `uefi` crate's own enum, so `hal`
/// stays free of that dependency.
mod raw_memory_type {
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const CONVENTIONAL: u32 = 7;
}

/// Classifies a raw UEFI memory-type ordinal. Boot-services memory gets its
/// own kind rather than `Usable`: the UEFI spec lets an OS reuse it after
/// `ExitBootServices`, but only an OS that no longer runs on the firmware's
/// stack and page tables, which this kernel still does (see
/// `MemoryRegionKind::BootServices`). When to reclaim it is the kernel's
/// decision, not this classifier's.
pub fn classify_memory_type(raw_ordinal: u32) -> MemoryRegionKind {
    use raw_memory_type::*;
    match raw_ordinal {
        CONVENTIONAL => MemoryRegionKind::Usable,
        BOOT_SERVICES_CODE | BOOT_SERVICES_DATA => MemoryRegionKind::BootServices,
        RUNTIME_SERVICES_CODE => MemoryRegionKind::RuntimeCode,
        _ => MemoryRegionKind::Reserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_memory_is_usable() {
        assert_eq!(
            classify_memory_type(raw_memory_type::CONVENTIONAL),
            MemoryRegionKind::Usable
        );
    }

    #[test]
    fn boot_services_memory_is_its_own_kind_not_usable() {
        assert_eq!(
            classify_memory_type(raw_memory_type::BOOT_SERVICES_CODE),
            MemoryRegionKind::BootServices
        );
        assert_eq!(
            classify_memory_type(raw_memory_type::BOOT_SERVICES_DATA),
            MemoryRegionKind::BootServices
        );
    }

    #[test]
    fn runtime_services_code_is_its_own_kind() {
        assert_eq!(
            classify_memory_type(raw_memory_type::RUNTIME_SERVICES_CODE),
            MemoryRegionKind::RuntimeCode
        );
        // Runtime services *data* is not executed: plain reserved memory.
        assert_eq!(classify_memory_type(6), MemoryRegionKind::Reserved);
    }

    #[test]
    fn loader_and_reserved_memory_is_never_usable() {
        // LOADER_CODE=1, LOADER_DATA=2, RESERVED=0
        assert_eq!(classify_memory_type(0), MemoryRegionKind::Reserved);
        assert_eq!(classify_memory_type(1), MemoryRegionKind::Reserved);
        assert_eq!(classify_memory_type(2), MemoryRegionKind::Reserved);
    }

    #[test]
    fn unknown_ordinal_is_reserved() {
        assert_eq!(classify_memory_type(9999), MemoryRegionKind::Reserved);
    }

    #[test]
    fn push_reports_capacity_exhaustion_instead_of_panicking() {
        let mut map = MemoryMap::new();
        let region = MemoryRegion {
            start_phys_addr: PhysAddr::new(0),
            page_count: 1,
            kind: MemoryRegionKind::Usable,
        };
        for _ in 0..MemoryMap::CAPACITY {
            assert!(map.push(region));
        }
        assert!(!map.push(region), "push past capacity must return false");
        assert_eq!(map.len(), MemoryMap::CAPACITY);
    }

    #[test]
    fn total_usable_pages_sums_only_usable_regions() {
        let mut map = MemoryMap::new();
        assert!(map.push(MemoryRegion {
            start_phys_addr: PhysAddr::new(0),
            page_count: 10,
            kind: MemoryRegionKind::Usable,
        }));
        assert!(map.push(MemoryRegion {
            start_phys_addr: PhysAddr::new(0x1000),
            page_count: 5,
            kind: MemoryRegionKind::Reserved,
        }));
        assert!(map.push(MemoryRegion {
            start_phys_addr: PhysAddr::new(0x2000),
            page_count: 20,
            kind: MemoryRegionKind::Usable,
        }));
        assert_eq!(map.total_usable_pages(), 30);
    }

    #[test]
    fn total_pages_counts_each_kind_separately() {
        let mut map = MemoryMap::new();
        for (start, pages, kind) in [
            (0x0, 4, MemoryRegionKind::Usable),
            (0x4000, 7, MemoryRegionKind::BootServices),
            (0xB000, 2, MemoryRegionKind::Reserved),
            (0xD000, 3, MemoryRegionKind::BootServices),
        ] {
            assert!(map.push(MemoryRegion {
                start_phys_addr: PhysAddr::new(start),
                page_count: pages,
                kind,
            }));
        }
        assert_eq!(map.total_pages(MemoryRegionKind::Usable), 4);
        assert_eq!(map.total_pages(MemoryRegionKind::BootServices), 10);
        assert_eq!(map.total_pages(MemoryRegionKind::Reserved), 2);
    }
}
