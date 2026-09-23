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
    /// What that code reads and writes: the runtime services table, its
    /// variables, whatever it keeps between calls. Not executable, and
    /// never allocatable, but it has to stay mapped for `reboot` and
    /// `shutdown` to work once the lower half is otherwise empty
    /// (docs/adr/0013-fase3-physical-window.md).
    RuntimeData,
    Reserved,
}

use crate::addr::PhysAddr;

/// How a range may be cached. UEFI reports what each one supports; this is
/// what the kernel picks, and page tables have to reproduce it: mapping
/// MMIO write-back would corrupt whatever is behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Nothing said. Treated as write-back, which is what RAM wants.
    Unspecified,
    WriteBack,
    WriteThrough,
    WriteCombining,
    Uncacheable,
}

/// What UEFI says about a range, beyond what it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionAttributes {
    /// `EFI_MEMORY_RUNTIME`: the firmware needs this range mapped after
    /// `ExitBootServices`, **whatever its type**. Memory-mapped I/O the
    /// runtime services talk to carries it too, which is why keeping only
    /// types 5 and 6 is not enough (UEFI 2.10, "Memory Map").
    pub runtime: bool,
    pub cache: CachePolicy,
}

impl RegionAttributes {
    /// What a region carries when nobody said otherwise.
    pub const fn none() -> Self {
        Self {
            runtime: false,
            cache: CachePolicy::Unspecified,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub start_phys_addr: PhysAddr,
    pub page_count: u64,
    pub kind: MemoryRegionKind,
    pub attributes: RegionAttributes,
}

/// Fixed-capacity memory map: no heap exists yet when this is built (the
/// heap is built *using* this map, in Incremento 7), so a `Vec` is not an
/// option.
#[derive(Clone, Copy)]
pub struct MemoryMap {
    regions: [MemoryRegion; Self::CAPACITY],
    len: usize,
    /// Set the moment a region did not fit. A map with a hole in it
    /// cannot be used to decide what may be unmapped: the range that was
    /// dropped might be one the firmware still needs.
    truncated: bool,
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
            attributes: RegionAttributes::none(),
        };
        Self {
            regions: [EMPTY; Self::CAPACITY],
            len: 0,
            truncated: false,
        }
    }

    /// Returns `false` (without adding the region) once the map is at
    /// capacity, so the caller can log a warning and keep going rather
    /// than panic or silently corrupt an array index.
    #[must_use]
    pub fn push(&mut self, region: MemoryRegion) -> bool {
        if self.len >= Self::CAPACITY {
            self.truncated = true;
            return false;
        }
        self.regions[self.len] = region;
        self.len += 1;
        true
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether this is the whole map the firmware reported. `false` means
    /// at least one region was dropped, and then nothing may be unmapped
    /// on the strength of what is here.
    pub fn is_complete(&self) -> bool {
        !self.truncated
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
/// The attribute bits of a UEFI memory descriptor, by value (UEFI 2.10,
/// "Memory Map").
mod raw_attribute {
    pub const UNCACHEABLE: u64 = 0x1;
    pub const WRITE_COMBINE: u64 = 0x2;
    pub const WRITE_THROUGH: u64 = 0x4;
    pub const WRITE_BACK: u64 = 0x8;
    /// The one that matters most: the firmware needs this range mapped
    /// after `ExitBootServices`, whatever the range is for.
    pub const RUNTIME: u64 = 0x8000_0000_0000_0000;
}

/// Turns a descriptor's raw attribute field into what the kernel maps.
///
/// The field lists what a range *supports*, so several cache bits can be
/// set at once. Taking the most permissive one the firmware offers gives
/// RAM write-back and leaves memory-mapped I/O — which usually advertises
/// nothing but uncacheable — where it has to be.
pub fn classify_attributes(raw: u64) -> RegionAttributes {
    use raw_attribute::*;
    let cache = if raw & WRITE_BACK != 0 {
        CachePolicy::WriteBack
    } else if raw & WRITE_THROUGH != 0 {
        CachePolicy::WriteThrough
    } else if raw & WRITE_COMBINE != 0 {
        CachePolicy::WriteCombining
    } else if raw & UNCACHEABLE != 0 {
        CachePolicy::Uncacheable
    } else {
        CachePolicy::Unspecified
    };
    RegionAttributes {
        runtime: raw & RUNTIME != 0,
        cache,
    }
}

mod raw_memory_type {
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
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
        RUNTIME_SERVICES_DATA => MemoryRegionKind::RuntimeData,
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

    /// The two halves of what the firmware keeps alive after the exit are
    /// told apart: its code runs, its data does not, and both have to stay
    /// mapped once the lower half is otherwise empty (ADR 0013).
    #[test]
    fn runtime_services_code_and_data_are_their_own_kinds() {
        assert_eq!(
            classify_memory_type(raw_memory_type::RUNTIME_SERVICES_CODE),
            MemoryRegionKind::RuntimeCode
        );
        assert_eq!(
            classify_memory_type(raw_memory_type::RUNTIME_SERVICES_DATA),
            MemoryRegionKind::RuntimeData
        );
        assert_ne!(
            classify_memory_type(raw_memory_type::RUNTIME_SERVICES_DATA),
            MemoryRegionKind::Reserved,
            "runtime data used to be lumped in with the rest and unmapped"
        );
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
            attributes: RegionAttributes::none(),
        };
        for _ in 0..MemoryMap::CAPACITY {
            assert!(map.push(region));
        }
        assert!(!map.push(region), "push past capacity must return false");
        assert_eq!(map.len(), MemoryMap::CAPACITY);
    }

    /// A map that did not fit says so, and keeps saying it.
    /// The attribute the whole increment turns on: a range carrying
    /// `EFI_MEMORY_RUNTIME` has to stay mapped whatever its type, and
    /// with the caching it was reported with.
    #[test]
    fn the_runtime_attribute_and_the_caching_survive_the_crossing() {
        use raw_attribute::*;

        let mmio = classify_attributes(UNCACHEABLE | RUNTIME);
        assert_eq!(
            mmio,
            RegionAttributes {
                runtime: true,
                cache: CachePolicy::Uncacheable
            },
            "memory-mapped I/O the firmware talks to"
        );

        // RAM advertises everything it supports; write-back is the pick.
        let ram = classify_attributes(UNCACHEABLE | WRITE_COMBINE | WRITE_THROUGH | WRITE_BACK);
        assert_eq!(
            ram,
            RegionAttributes {
                runtime: false,
                cache: CachePolicy::WriteBack
            }
        );
        assert!(classify_attributes(WRITE_BACK | RUNTIME).runtime);
        assert!(!classify_attributes(WRITE_BACK).runtime);

        assert_eq!(
            classify_attributes(WRITE_THROUGH).cache,
            CachePolicy::WriteThrough
        );
        assert_eq!(
            classify_attributes(WRITE_COMBINE).cache,
            CachePolicy::WriteCombining
        );
        assert_eq!(classify_attributes(0).cache, CachePolicy::Unspecified);
    }

    #[test]
    fn a_truncated_map_never_claims_to_be_complete() {
        let mut map = MemoryMap::new();
        assert!(map.is_complete(), "an empty map is a complete one");
        let region = MemoryRegion {
            start_phys_addr: PhysAddr::new(0),
            page_count: 1,
            kind: MemoryRegionKind::Usable,
            attributes: RegionAttributes::none(),
        };
        for _ in 0..MemoryMap::CAPACITY {
            assert!(map.push(region));
            assert!(map.is_complete());
        }
        assert!(!map.push(region));
        assert!(!map.is_complete(), "one region was dropped");
    }

    #[test]
    fn total_usable_pages_sums_only_usable_regions() {
        let mut map = MemoryMap::new();
        assert!(map.push(MemoryRegion {
            start_phys_addr: PhysAddr::new(0),
            page_count: 10,
            kind: MemoryRegionKind::Usable,
            attributes: RegionAttributes::none(),
        }));
        assert!(map.push(MemoryRegion {
            start_phys_addr: PhysAddr::new(0x1000),
            page_count: 5,
            kind: MemoryRegionKind::Reserved,
            attributes: RegionAttributes::none(),
        }));
        assert!(map.push(MemoryRegion {
            start_phys_addr: PhysAddr::new(0x2000),
            page_count: 20,
            kind: MemoryRegionKind::Usable,
            attributes: RegionAttributes::none(),
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
                attributes: RegionAttributes::none(),
            }));
        }
        assert_eq!(map.total_pages(MemoryRegionKind::Usable), 4);
        assert_eq!(map.total_pages(MemoryRegionKind::BootServices), 10);
        assert_eq!(map.total_pages(MemoryRegionKind::Reserved), 2);
    }
}
