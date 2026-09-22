//! Bitmap physical frame allocator.
//!
//! One bit per `FRAME_SIZE` frame, counting from physical address 0, kept
//! in caller-provided storage: set = not available (withheld or already
//! handed out), clear = free. A bitmap rather than a bump allocator (which
//! cannot free) or a buddy/slab one (nothing needs multi-frame or
//! sub-frame allocations yet).
//!
//! Which frames can ever be handed out is decided from the memory map by
//! rules that all err towards withholding a frame:
//!
//! * only `Usable` regions count, rounded *inwards* to whole frames;
//! * any frame a non-`Usable` region touches is withheld, rounded
//!   *outwards*, even where it overlaps a `Usable` region (firmware maps
//!   should not overlap; if one does, the withholding side wins). This
//!   includes `BootServices` memory, which still holds the kernel's own
//!   stack and page tables (see `MemoryRegionKind::BootServices`);
//! * frame 0 is never handed out: its address is the null pointer;
//! * frames beyond what the storage covers are ignored (and counted).
//!
//! `deallocate` checks a frame against the same rules, so freeing a frame
//! this allocator could never have handed out is an error rather than a
//! way to make withheld memory allocatable.
//!
//! No `unsafe`: this only flips bits in a slice. Nothing here reads or
//! writes the frames themselves.

use harlan_hal::frame::{FRAME_SIZE, PhysFrame};
use harlan_hal::memory_map::{MemoryMap, MemoryRegion, MemoryRegionKind};

const BITS_PER_WORD: u64 = u64::BITS as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeallocError {
    /// Not a frame this allocator can ever hand out: withheld by the memory
    /// map, frame 0, or beyond the covered range.
    NotManaged,
    /// A managed frame that is already free: a double free, or freeing a
    /// frame that was never allocated.
    NotAllocated,
}

pub struct BitmapFrameAllocator<'a> {
    bitmap: &'a mut [u64],
    map: &'a MemoryMap,
    free: u64,
    /// Word where the next search starts (next-fit): allocation does not
    /// rescan the already-full low words every time.
    next_word: usize,
    uncovered: u64,
}

impl<'a> BitmapFrameAllocator<'a> {
    /// Builds the allocator over `storage` (whatever it held is
    /// overwritten), covering `storage.len() * 64` frames from physical
    /// address 0.
    pub fn new(storage: &'a mut [u64], map: &'a MemoryMap) -> Self {
        storage.fill(u64::MAX);
        let mut allocator = Self {
            bitmap: storage,
            map,
            free: 0,
            next_word: 0,
            uncovered: 0,
        };
        let covered = allocator.covered_frames();

        for region in map.iter().filter(|r| r.kind == MemoryRegionKind::Usable) {
            let (first, end) = inward_frames(region);
            allocator.uncovered += end.saturating_sub(first.max(covered));
            for frame in first..end.min(covered) {
                allocator.set_bit(frame, false);
            }
        }
        // Second pass, so that withholding wins over any overlap.
        for region in map.iter().filter(|r| r.kind != MemoryRegionKind::Usable) {
            let (first, end) = outward_frames(region);
            for frame in first..end.min(covered) {
                allocator.set_bit(frame, true);
            }
        }
        if covered > 0 {
            allocator.set_bit(0, true);
        }

        allocator.free = allocator
            .bitmap
            .iter()
            .map(|word| u64::from(word.count_zeros()))
            .sum();
        allocator
    }

    pub fn allocate(&mut self) -> Option<PhysFrame> {
        if self.free == 0 {
            return None;
        }
        let words = self.bitmap.len();
        for offset in 0..words {
            let index = (self.next_word + offset) % words;
            let word = self.bitmap[index];
            if word != u64::MAX {
                let bit = u64::from((!word).trailing_zeros());
                self.bitmap[index] = word | (1 << bit);
                self.free -= 1;
                self.next_word = index;
                let frame = index as u64 * BITS_PER_WORD + bit;
                return Some(PhysFrame::containing_address(frame * FRAME_SIZE));
            }
        }
        // `free > 0` guarantees a clear bit; only a bug gets here, and
        // "nothing to hand out" is the safe answer to it.
        None
    }

    pub fn deallocate(&mut self, frame: PhysFrame) -> Result<(), DeallocError> {
        if !self.manages(frame) {
            return Err(DeallocError::NotManaged);
        }
        if !self.bit(frame.number()) {
            return Err(DeallocError::NotAllocated);
        }
        self.set_bit(frame.number(), false);
        self.free += 1;
        Ok(())
    }

    /// Whether `frame` is one this allocator can ever hand out, free or
    /// not. Re-derived from the memory map, not read from the bitmap, so it
    /// also guards `deallocate` against bitmap state it should not trust.
    pub fn manages(&self, frame: PhysFrame) -> bool {
        let number = frame.number();
        let contains = |(first, end): (u64, u64)| first <= number && number < end;
        number != 0
            && number < self.covered_frames()
            && self
                .map
                .iter()
                .any(|r| r.kind == MemoryRegionKind::Usable && contains(inward_frames(r)))
            && !self
                .map
                .iter()
                .any(|r| r.kind != MemoryRegionKind::Usable && contains(outward_frames(r)))
    }

    pub fn free_frames(&self) -> u64 {
        self.free
    }

    pub fn covered_frames(&self) -> u64 {
        self.bitmap.len() as u64 * BITS_PER_WORD
    }

    /// Frames of `Usable` regions that lie beyond the covered range and
    /// were therefore ignored.
    pub fn uncovered_usable_frames(&self) -> u64 {
        self.uncovered
    }

    /// Callers only pass frame numbers below `covered_frames()`, which
    /// bounds the index into `bitmap`.
    fn bit(&self, frame: u64) -> bool {
        let word = self.bitmap[(frame / BITS_PER_WORD) as usize];
        word & (1 << (frame % BITS_PER_WORD)) != 0
    }

    fn set_bit(&mut self, frame: u64, value: bool) {
        let word = &mut self.bitmap[(frame / BITS_PER_WORD) as usize];
        let mask = 1 << (frame % BITS_PER_WORD);
        if value {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }
}

/// End address of a region, saturating: the map is firmware data and a
/// corrupt `page_count` must not wrap around to a small address.
fn region_end(region: &MemoryRegion) -> u64 {
    region
        .start_phys_addr
        .saturating_add(region.page_count.saturating_mul(FRAME_SIZE))
}

/// `[first, end)` frame numbers lying wholly inside `region`.
fn inward_frames(region: &MemoryRegion) -> (u64, u64) {
    (
        region.start_phys_addr.div_ceil(FRAME_SIZE),
        region_end(region) / FRAME_SIZE,
    )
}

/// `[first, end)` frame numbers `region` touches at all.
fn outward_frames(region: &MemoryRegion) -> (u64, u64) {
    (
        region.start_phys_addr / FRAME_SIZE,
        region_end(region).div_ceil(FRAME_SIZE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harlan_hal::memory_map::MemoryRegionKind::{BootServices, Reserved, Usable};
    use std::collections::BTreeSet;

    fn map_of(regions: &[(u64, u64, MemoryRegionKind)]) -> MemoryMap {
        let mut map = MemoryMap::new();
        for &(start_phys_addr, page_count, kind) in regions {
            assert!(map.push(MemoryRegion {
                start_phys_addr,
                page_count,
                kind,
            }));
        }
        map
    }

    fn frame(addr: u64) -> PhysFrame {
        PhysFrame::from_start_address(addr).unwrap()
    }

    fn drain(allocator: &mut BitmapFrameAllocator<'_>) -> Vec<u64> {
        core::iter::from_fn(|| allocator.allocate())
            .map(PhysFrame::start_address)
            .collect()
    }

    #[test]
    fn empty_map_hands_out_nothing() {
        let map = MemoryMap::new();
        let mut storage = [0; 4];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(allocator.free_frames(), 0);
        assert_eq!(allocator.allocate(), None);
    }

    #[test]
    fn frame_zero_is_never_handed_out() {
        let map = map_of(&[(0, 4, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert!(!allocator.manages(frame(0)));
        assert_eq!(drain(&mut allocator), [0x1000, 0x2000, 0x3000]);
    }

    #[test]
    fn only_usable_regions_are_handed_out() {
        let map = map_of(&[
            (0x10000, 4, Usable),
            (0x20000, 4, BootServices),
            (0x30000, 4, Reserved),
        ]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(allocator.free_frames(), 4);
        assert_eq!(drain(&mut allocator), [0x10000, 0x11000, 0x12000, 0x13000]);
    }

    #[test]
    fn a_withheld_region_overlapping_a_usable_one_wins() {
        let map = map_of(&[(0x10000, 8, Usable), (0x12000, 2, Reserved)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let handed_out = drain(&mut allocator);
        assert_eq!(handed_out.len(), 6);
        assert!(!handed_out.contains(&0x12000) && !handed_out.contains(&0x13000));
    }

    #[test]
    fn usable_rounds_inwards_and_withheld_rounds_outwards() {
        let map = map_of(&[
            // Unaligned usable region 0x10800..0x13800: only 0x11000 and
            // 0x12000 lie wholly inside it.
            (0x10800, 3, Usable),
            (0x20000, 4, Usable),
            // Unaligned reserved region 0x20800..0x21800 touches both
            // 0x20000 and 0x21000.
            (0x20800, 1, Reserved),
        ]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(drain(&mut allocator), [0x11000, 0x12000, 0x22000, 0x23000]);
    }

    #[test]
    fn allocations_are_distinct_and_counted_until_exhaustion() {
        let map = map_of(&[(0x1000, 100, Usable)]);
        let mut storage = [0; 2];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let mut seen = BTreeSet::new();
        for remaining in (0..100).rev() {
            let got = allocator.allocate().unwrap();
            assert!(seen.insert(got), "{got:?} handed out twice");
            assert_eq!(allocator.free_frames(), remaining);
        }
        assert_eq!(allocator.allocate(), None);
    }

    #[test]
    fn a_freed_frame_is_handed_out_again() {
        let map = map_of(&[(0x1000, 3, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let frames = drain(&mut allocator);
        assert_eq!(allocator.deallocate(frame(frames[1])), Ok(()));
        assert_eq!(allocator.free_frames(), 1);
        assert_eq!(allocator.allocate(), Some(frame(frames[1])));
        assert_eq!(allocator.allocate(), None);
    }

    #[test]
    fn allocation_wraps_around_to_frames_freed_behind_the_search_point() {
        let map = map_of(&[(0x1000, 3 * 64 - 1, Usable)]);
        let mut storage = [0; 3];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        drain(&mut allocator);
        assert_eq!(allocator.deallocate(frame(0x1000)), Ok(()));
        assert_eq!(allocator.allocate(), Some(frame(0x1000)));
    }

    #[test]
    fn double_free_is_rejected() {
        let map = map_of(&[(0x1000, 2, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let got = allocator.allocate().unwrap();
        assert_eq!(allocator.deallocate(got), Ok(()));
        assert_eq!(allocator.deallocate(got), Err(DeallocError::NotAllocated));
        assert_eq!(allocator.free_frames(), 2);
    }

    #[test]
    fn freeing_a_never_allocated_frame_is_rejected() {
        let map = map_of(&[(0x1000, 2, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(
            allocator.deallocate(frame(0x1000)),
            Err(DeallocError::NotAllocated)
        );
        assert_eq!(allocator.free_frames(), 2);
    }

    #[test]
    fn freeing_a_withheld_frame_is_rejected_and_does_not_make_it_allocatable() {
        let map = map_of(&[
            (0x0, 8, Usable),
            (0x10000, 2, BootServices),
            (0x20000, 2, Reserved),
        ]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let free_before = allocator.free_frames();
        for addr in [0x0, 0x10000, 0x20000, 0x30000, 64 * FRAME_SIZE] {
            assert_eq!(
                allocator.deallocate(PhysFrame::containing_address(addr)),
                Err(DeallocError::NotManaged),
                "{addr:#x}"
            );
        }
        assert_eq!(allocator.free_frames(), free_before);
        let handed_out = drain(&mut allocator);
        assert!(
            handed_out.iter().all(|&addr| addr < 0x8000),
            "{handed_out:x?}"
        );
    }

    #[test]
    fn frames_beyond_the_covered_range_are_ignored_and_counted() {
        // 1 word covers frames 0..64 (0..0x40000); this region spans
        // frames 48..80.
        let map = map_of(&[(0x30000, 32, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(allocator.covered_frames(), 64);
        assert_eq!(allocator.free_frames(), 16);
        assert_eq!(allocator.uncovered_usable_frames(), 16);
        assert!(drain(&mut allocator).iter().all(|&addr| addr < 0x40000));
    }

    #[test]
    fn corrupt_regions_near_the_top_of_the_address_space_do_not_overflow() {
        let map = map_of(&[
            (0x1000, 1, Usable),
            (u64::MAX - 0xFFF, u64::MAX, Usable),
            (u64::MAX - 0x1FFF, u64::MAX, Reserved),
        ]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(drain(&mut allocator), [0x1000]);
        assert!(!allocator.manages(PhysFrame::containing_address(u64::MAX)));
    }

    #[test]
    fn bitmap_agrees_with_manages_for_every_covered_frame() {
        let map = map_of(&[
            (0x0, 20, Usable),
            (0x5000, 2, Reserved),
            (0x14800, 10, Usable),
            (0x18000, 1, BootServices),
            (0x30000, 64, Usable),
            (0x40000, 3, Reserved),
        ]);
        let mut storage = [0; 2];
        let allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let mut managed = 0;
        for number in 0..allocator.covered_frames() {
            let frame = PhysFrame::containing_address(number * FRAME_SIZE);
            assert_eq!(
                !allocator.bit(number),
                allocator.manages(frame),
                "frame {number}"
            );
            managed += u64::from(allocator.manages(frame));
        }
        assert_eq!(allocator.free_frames(), managed);
    }

    /// First descriptors of the real map OVMF reported under
    /// `cargo xtask boot-test` (captured 2026-09-21; UEFI types in
    /// parentheses).
    #[test]
    fn real_ovmf_map_excerpt() {
        let map = map_of(&[
            (0x0, 135, Usable),            // Conventional
            (0x87000, 1, BootServices),    // BootServicesData
            (0x88000, 24, Usable),         // Conventional
            (0x100000, 1792, Usable),      // Conventional
            (0x800000, 8, Reserved),       // ACPI NVS
            (0x808000, 3, Usable),         // Conventional
            (0xfe6b000, 32, BootServices), // BootServicesData: the live stack
        ]);
        let mut storage = [0; 1024];
        let allocator = BitmapFrameAllocator::new(&mut storage, &map);
        // Frame 0 withheld; boot-services memory withheld.
        assert_eq!(allocator.free_frames(), 134 + 24 + 1792 + 3);
        assert!(!allocator.manages(PhysFrame::containing_address(0xfe86f70)));
        assert!(!allocator.manages(frame(0x87000)));
    }

    /// Deterministic pseudo-random allocate/free sequence checked against
    /// a plain set of the frames currently handed out.
    #[test]
    fn random_operations_match_a_reference_model() {
        let map = map_of(&[
            (0x0, 50, Usable),
            (0x9000, 3, Reserved),
            (0x40000, 90, Usable),
            (0x70000, 5, BootServices),
        ]);
        let mut storage = [0; 4];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let managed = allocator.free_frames();
        let mut held: Vec<PhysFrame> = Vec::new();
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };

        for _ in 0..10_000 {
            if next() % 3 != 0 {
                match allocator.allocate() {
                    Some(got) => {
                        assert!(allocator.manages(got), "{got:?} is not a managed frame");
                        assert!(!held.contains(&got), "{got:?} handed out twice");
                        held.push(got);
                    }
                    None => assert_eq!(held.len() as u64, managed),
                }
            } else if !held.is_empty() {
                let victim = held.swap_remove(next() as usize % held.len());
                assert_eq!(allocator.deallocate(victim), Ok(()));
                assert_eq!(
                    allocator.deallocate(victim),
                    Err(DeallocError::NotAllocated)
                );
            }
            assert_eq!(allocator.free_frames(), managed - held.len() as u64);
        }
    }
}
