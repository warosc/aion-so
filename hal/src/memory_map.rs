//! Firmware-agnostic memory map. `boot` (which depends on the `uefi`
//! crate) builds one of these from the real UEFI memory map at the
//! `ExitBootServices` transition; `kernel` consumes it without ever
//! depending on `uefi` itself.

/// Coarse classification of a memory region. Nothing downstream needs
/// finer granularity yet — the Incremento 5 frame allocator only cares
/// about usable-or-not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRegionKind {
    Usable,
    Reserved,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub start_phys_addr: u64,
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
            start_phys_addr: 0,
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
        self.iter()
            .filter(|r| r.kind == MemoryRegionKind::Usable)
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
    pub const CONVENTIONAL: u32 = 7;
}

/// Classifies a raw UEFI memory-type ordinal. `post_exit`: whether this
/// runs after `ExitBootServices` — `BOOT_SERVICES_CODE`/`_DATA` only
/// become safely reusable then (the UEFI spec's own documented
/// convention, matching what the Linux EFI stub does).
pub fn classify_memory_type(raw_ordinal: u32, post_exit: bool) -> MemoryRegionKind {
    use raw_memory_type::*;
    match raw_ordinal {
        CONVENTIONAL => MemoryRegionKind::Usable,
        BOOT_SERVICES_CODE | BOOT_SERVICES_DATA if post_exit => MemoryRegionKind::Usable,
        _ => MemoryRegionKind::Reserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_memory_is_usable() {
        assert_eq!(
            classify_memory_type(raw_memory_type::CONVENTIONAL, true),
            MemoryRegionKind::Usable
        );
        assert_eq!(
            classify_memory_type(raw_memory_type::CONVENTIONAL, false),
            MemoryRegionKind::Usable
        );
    }

    #[test]
    fn boot_services_memory_is_usable_only_post_exit() {
        assert_eq!(
            classify_memory_type(raw_memory_type::BOOT_SERVICES_CODE, true),
            MemoryRegionKind::Usable
        );
        assert_eq!(
            classify_memory_type(raw_memory_type::BOOT_SERVICES_CODE, false),
            MemoryRegionKind::Reserved
        );
        assert_eq!(
            classify_memory_type(raw_memory_type::BOOT_SERVICES_DATA, true),
            MemoryRegionKind::Usable
        );
    }

    #[test]
    fn loader_and_reserved_memory_is_never_usable() {
        // LOADER_CODE=1, LOADER_DATA=2, RESERVED=0
        assert_eq!(classify_memory_type(0, true), MemoryRegionKind::Reserved);
        assert_eq!(classify_memory_type(1, true), MemoryRegionKind::Reserved);
        assert_eq!(classify_memory_type(2, true), MemoryRegionKind::Reserved);
    }

    #[test]
    fn unknown_ordinal_is_reserved() {
        assert_eq!(classify_memory_type(9999, true), MemoryRegionKind::Reserved);
    }

    #[test]
    fn push_reports_capacity_exhaustion_instead_of_panicking() {
        let mut map = MemoryMap::new();
        let region = MemoryRegion {
            start_phys_addr: 0,
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
            start_phys_addr: 0,
            page_count: 10,
            kind: MemoryRegionKind::Usable,
        }));
        assert!(map.push(MemoryRegion {
            start_phys_addr: 0x1000,
            page_count: 5,
            kind: MemoryRegionKind::Reserved,
        }));
        assert!(map.push(MemoryRegion {
            start_phys_addr: 0x2000,
            page_count: 20,
            kind: MemoryRegionKind::Usable,
        }));
        assert_eq!(map.total_usable_pages(), 30);
    }
}
