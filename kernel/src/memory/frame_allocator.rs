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

use harlan_hal::addr::PhysAddr;
use harlan_hal::frame::{FRAME_SIZE, FrameAllocator, PhysFrame};
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
    /// Freed as something it is not: the frame is in use, but for another
    /// purpose than the caller thinks (a page table freed as a heap page,
    /// say). Nothing is freed.
    WrongPurpose {
        expected: FramePurpose,
        actual: Option<FramePurpose>,
    },
}

/// What a frame the kernel holds is being used for. Recorded per frame so
/// that a wrong assumption shows up as an error instead of silently freeing
/// something else's memory, and so the boot log can say where the memory
/// went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FramePurpose {
    /// Handed out before the kernel had anywhere to record purposes.
    Boot = 1,
    /// A page table.
    PageTable = 2,
    /// Backing the kernel heap.
    Heap = 3,
    /// A kernel stack.
    Stack = 4,
    /// Anything else the kernel keeps.
    Kernel = 5,
}

impl FramePurpose {
    pub const ALL: [FramePurpose; 5] = [
        FramePurpose::Boot,
        FramePurpose::PageTable,
        FramePurpose::Heap,
        FramePurpose::Stack,
        FramePurpose::Kernel,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            FramePurpose::Boot => "boot",
            FramePurpose::PageTable => "page tables",
            FramePurpose::Heap => "heap",
            FramePurpose::Stack => "stacks",
            FramePurpose::Kernel => "kernel",
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(FramePurpose::Boot),
            2 => Some(FramePurpose::PageTable),
            3 => Some(FramePurpose::Heap),
            4 => Some(FramePurpose::Stack),
            5 => Some(FramePurpose::Kernel),
            _ => None,
        }
    }
}

pub struct BitmapFrameAllocator<'a> {
    bitmap: &'a mut [u64],
    map: &'a MemoryMap,
    /// Once the kernel owns its stack and page tables, boot-services
    /// memory becomes allocatable too (see `reclaim_boot_services`).
    boot_services_reclaimed: bool,
    /// One byte per frame: 0 for a frame nobody holds, otherwise the
    /// purpose it was handed out for. `None` until the kernel has a heap to
    /// put the table in (`with_purposes`).
    purposes: Option<&'a mut [u8]>,
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
            boot_services_reclaimed: false,
            purposes: None,
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

    /// Takes over `storage`, a copy of another allocator's bitmap over the
    /// same `map`, keeping every frame that was already handed out handed
    /// out. This is how the kernel moves its bookkeeping (from the
    /// firmware's memory to its own heap) without losing track of anything.
    pub fn adopt(
        storage: &'a mut [u64],
        map: &'a MemoryMap,
        boot_services_reclaimed: bool,
    ) -> Self {
        let free = storage
            .iter()
            .map(|word| u64::from(word.count_zeros()))
            .sum();
        Self {
            bitmap: storage,
            map,
            boot_services_reclaimed,
            purposes: None,
            free,
            next_word: 0,
            uncovered: 0,
        }
    }

    /// Starts recording what each frame is used for, in `table` (one byte
    /// per frame, as long as the bitmap covers). Frames already handed out
    /// are recorded as `Boot`: they were taken before there was anywhere to
    /// write this down.
    pub fn with_purposes(mut self, table: &'a mut [u8]) -> Self {
        let covered = self.covered_frames() as usize;
        assert!(
            table.len() >= covered,
            "the purpose table must cover every frame the bitmap does"
        );
        for number in 0..covered as u64 {
            let frame = PhysFrame::containing_address(PhysAddr::new(number * FRAME_SIZE));
            let held = self.manages(frame) && self.bit(number);
            table[number as usize] = if held { FramePurpose::Boot as u8 } else { 0 };
        }
        self.purposes = Some(table);
        self
    }

    /// What `frame` is being used for, if the kernel holds it and purposes
    /// are being recorded.
    pub fn purpose_of(&self, frame: PhysFrame) -> Option<FramePurpose> {
        let table = self.purposes.as_ref()?;
        let number = frame.number() as usize;
        FramePurpose::from_byte(*table.get(number)?)
    }

    /// How many frames are held for `purpose`.
    pub fn frames_for(&self, purpose: FramePurpose) -> u64 {
        self.purposes.as_ref().map_or(0, |table| {
            table.iter().filter(|&&byte| byte == purpose as u8).count() as u64
        })
    }

    /// Records what a frame the kernel already holds is for, for frames
    /// taken before there was anywhere to write it down. Returns whether
    /// anything was recorded: a frame nobody holds keeps no purpose.
    pub fn label(&mut self, frame: PhysFrame, purpose: FramePurpose) -> bool {
        let held = self.manages(frame) && self.bit(frame.number());
        if held {
            self.set_purpose(frame, Some(purpose));
        }
        held
    }

    /// Hands out a frame and records what it is for.
    pub fn allocate_for(&mut self, purpose: FramePurpose) -> Option<PhysFrame> {
        let frame = self.allocate()?;
        self.set_purpose(frame, Some(purpose));
        Some(frame)
    }

    /// Gives a frame back, checking it is what the caller thinks it is.
    /// Nothing is freed if it is not.
    pub fn deallocate_as(
        &mut self,
        frame: PhysFrame,
        purpose: FramePurpose,
    ) -> Result<(), DeallocError> {
        let actual = self.purpose_of(frame);
        if self.purposes.is_some() && actual != Some(purpose) {
            return Err(DeallocError::WrongPurpose {
                expected: purpose,
                actual,
            });
        }
        self.deallocate(frame)
    }

    fn set_purpose(&mut self, frame: PhysFrame, purpose: Option<FramePurpose>) {
        let number = frame.number() as usize;
        if let Some(table) = self.purposes.as_mut()
            && let Some(slot) = table.get_mut(number)
        {
            *slot = purpose.map_or(0, |purpose| purpose as u8);
        }
    }

    /// Adds the map's boot-services memory to the pool, under the same
    /// rules as everything else: never a frame a reserved region touches,
    /// never frame 0, never one already handed out. Returns how many frames
    /// it added; calling it again adds nothing.
    ///
    /// Only correct once the kernel no longer depends on anything the
    /// firmware left there — its own stack and page tables, above all (see
    /// docs/adr/0007-fase2-own-memory.md).
    pub fn reclaim_boot_services(&mut self) -> u64 {
        if self.boot_services_reclaimed {
            return 0;
        }
        self.boot_services_reclaimed = true;
        let covered = self.covered_frames();
        let mut added = 0;
        // A copy of the shared reference, so walking the map does not
        // borrow `self` while the bits are being cleared.
        let map = self.map;
        for region in map
            .iter()
            .filter(|r| r.kind == MemoryRegionKind::BootServices)
        {
            let (first, end) = inward_frames(region);
            for number in first..end.min(covered) {
                let frame = PhysFrame::containing_address(PhysAddr::new(number * FRAME_SIZE));
                if self.manages(frame) && self.bit(number) {
                    self.set_bit(number, false);
                    added += 1;
                }
            }
        }
        self.free += added;
        added
    }

    pub fn boot_services_reclaimed(&self) -> bool {
        self.boot_services_reclaimed
    }

    /// The raw bitmap, to copy it somewhere the kernel owns (see `adopt`).
    pub fn bitmap(&self) -> &[u64] {
        self.bitmap
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
                return Some(PhysFrame::containing_address(PhysAddr::new(
                    frame * FRAME_SIZE,
                )));
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
        self.set_purpose(frame, None);
        self.free += 1;
        Ok(())
    }

    /// Whether `frame` is one this allocator can ever hand out, free or
    /// not. Re-derived from the memory map, not read from the bitmap, so it
    /// also guards `deallocate` against bitmap state it should not trust.
    pub fn manages(&self, frame: PhysFrame) -> bool {
        let number = frame.number();
        let contains = |(first, end): (u64, u64)| first <= number && number < end;
        let allocatable = |kind| {
            kind == MemoryRegionKind::Usable
                || (self.boot_services_reclaimed && kind == MemoryRegionKind::BootServices)
        };
        number != 0
            && number < self.covered_frames()
            && self
                .map
                .iter()
                .any(|r| allocatable(r.kind) && contains(inward_frames(r)))
            && !self
                .map
                .iter()
                .any(|r| !allocatable(r.kind) && contains(outward_frames(r)))
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

/// How architecture code draws frames from this allocator without
/// depending on the `kernel` crate. The only such consumer is page-table
/// creation, so frames taken this way are recorded as page tables; anything
/// else uses `allocate_for`.
impl FrameAllocator for BitmapFrameAllocator<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        self.allocate_for(FramePurpose::PageTable)
    }
}

/// End address of a region, saturating: the map is firmware data and a
/// corrupt `page_count` must not wrap around to a small address.
fn region_end(region: &MemoryRegion) -> u64 {
    region
        .start_phys_addr
        .as_u64()
        .saturating_add(region.page_count.saturating_mul(FRAME_SIZE))
}

/// `[first, end)` frame numbers lying wholly inside `region`.
fn inward_frames(region: &MemoryRegion) -> (u64, u64) {
    (
        region.start_phys_addr.as_u64().div_ceil(FRAME_SIZE),
        region_end(region) / FRAME_SIZE,
    )
}

/// `[first, end)` frame numbers `region` touches at all.
fn outward_frames(region: &MemoryRegion) -> (u64, u64) {
    (
        region.start_phys_addr.as_u64() / FRAME_SIZE,
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
                start_phys_addr: PhysAddr::new(start_phys_addr),
                page_count,
                kind,
            }));
        }
        map
    }

    fn frame(addr: u64) -> PhysFrame {
        PhysFrame::from_start_address(PhysAddr::new(addr)).unwrap()
    }

    fn drain(allocator: &mut BitmapFrameAllocator<'_>) -> Vec<u64> {
        core::iter::from_fn(|| allocator.allocate())
            .map(|frame| frame.start_address().as_u64())
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
                allocator.deallocate(PhysFrame::containing_address(PhysAddr::new(addr))),
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
        assert!(!allocator.manages(PhysFrame::containing_address(PhysAddr::new(u64::MAX))));
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
            let frame = PhysFrame::containing_address(PhysAddr::new(number * FRAME_SIZE));
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
        assert!(!allocator.manages(PhysFrame::containing_address(PhysAddr::new(0xfe86f70))));
        assert!(!allocator.manages(frame(0x87000)));
    }

    #[test]
    fn an_adopted_bitmap_keeps_every_frame_that_was_handed_out() {
        let map = map_of(&[(0x1000, 10, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let taken = [
            allocator.allocate().unwrap(),
            allocator.allocate().unwrap(),
            allocator.allocate().unwrap(),
        ];
        let free_before = allocator.free_frames();

        let mut copy = [0; 1];
        copy.copy_from_slice(allocator.bitmap());
        let mut moved = BitmapFrameAllocator::adopt(&mut copy, &map, false);

        assert_eq!(moved.free_frames(), free_before);
        let handed_out = drain(&mut moved);
        for frame in taken {
            assert!(
                !handed_out.contains(&frame.start_address().as_u64()),
                "{frame:?} was handed out twice"
            );
            assert_eq!(moved.deallocate(frame), Ok(()));
        }
    }

    #[test]
    fn reclaiming_boot_services_adds_exactly_its_frames() {
        let map = map_of(&[
            (0x0, 4, BootServices),
            (0x4000, 4, Usable),
            (0x8000, 4, BootServices),
        ]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let taken = allocator.allocate().unwrap();
        assert!(!allocator.manages(frame(0x8000)));

        // Frame 0 is in the first boot-services region and stays out.
        assert_eq!(allocator.reclaim_boot_services(), 3 + 4);
        assert_eq!(allocator.reclaim_boot_services(), 0, "not twice");
        assert!(allocator.manages(frame(0x8000)) && !allocator.manages(frame(0x0)));

        // The frame handed out before is still handed out.
        let handed_out = drain(&mut allocator);
        assert!(!handed_out.contains(&taken.start_address().as_u64()));
        assert_eq!(handed_out.len(), 3 + 4 + 3);
    }

    #[test]
    fn reclaiming_again_never_frees_what_was_handed_out() {
        let map = map_of(&[(0x1000, 8, BootServices), (0x9000, 1, Usable)]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(allocator.reclaim_boot_services(), 8);
        let taken = allocator.allocate().unwrap();
        let free_before = allocator.free_frames();

        assert_eq!(allocator.reclaim_boot_services(), 0);

        assert_eq!(
            allocator.free_frames(),
            free_before,
            "a live frame was freed"
        );
        // Still allocated: freeing it now must succeed exactly once.
        assert_eq!(allocator.deallocate(taken), Ok(()));
        assert_eq!(allocator.deallocate(taken), Err(DeallocError::NotAllocated));
    }

    #[test]
    fn reclaiming_never_touches_frames_a_reserved_region_covers() {
        let map = map_of(&[
            (0x1000, 8, BootServices),
            (0x3000, 1, Reserved),
            (0x9000, 1, Usable),
        ]);
        let mut storage = [0; 1];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        assert_eq!(allocator.reclaim_boot_services(), 7);
        assert!(!allocator.manages(frame(0x3000)));
        assert!(!drain(&mut allocator).contains(&0x3000));
    }

    #[test]
    fn frames_carry_what_they_are_used_for() {
        let map = map_of(&[(0x1000, 10, Usable)]);
        let mut storage = [0; 1];
        let mut table = [0u8; 64];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map).with_purposes(&mut table);

        let heap = allocator.allocate_for(FramePurpose::Heap).unwrap();
        let stack = allocator.allocate_for(FramePurpose::Stack).unwrap();
        assert_eq!(allocator.purpose_of(heap), Some(FramePurpose::Heap));
        assert_eq!(allocator.purpose_of(stack), Some(FramePurpose::Stack));
        assert_eq!(allocator.frames_for(FramePurpose::Heap), 1);
        assert_eq!(allocator.frames_for(FramePurpose::PageTable), 0);

        // A frame nobody holds carries no purpose.
        assert_eq!(allocator.purpose_of(frame(0x9000)), None);
        assert_eq!(allocator.deallocate(heap), Ok(()));
        assert_eq!(allocator.purpose_of(heap), None);
        assert_eq!(allocator.frames_for(FramePurpose::Heap), 0);
    }

    #[test]
    fn frames_taken_through_the_trait_are_page_tables() {
        let map = map_of(&[(0x1000, 4, Usable)]);
        let mut storage = [0; 1];
        let mut table = [0u8; 64];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map).with_purposes(&mut table);
        // What `arch` page-table code gets, through the trait.
        let frame = FrameAllocator::allocate_frame(&mut allocator).unwrap();
        assert_eq!(allocator.purpose_of(frame), Some(FramePurpose::PageTable));
    }

    #[test]
    fn labelling_only_touches_frames_the_kernel_holds() {
        let map = map_of(&[(0x1000, 4, Usable)]);
        let mut storage = [0; 1];
        let mut table = [0u8; 64];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map).with_purposes(&mut table);
        let held = allocator.allocate().unwrap();

        assert!(allocator.label(held, FramePurpose::Heap));
        assert_eq!(allocator.purpose_of(held), Some(FramePurpose::Heap));
        // Free and withheld frames keep no purpose.
        assert!(!allocator.label(frame(0x2000), FramePurpose::Heap));
        assert!(!allocator.label(frame(0), FramePurpose::Heap));
        assert_eq!(allocator.frames_for(FramePurpose::Heap), 1);
    }

    #[test]
    fn freeing_a_frame_as_the_wrong_thing_frees_nothing() {
        let map = map_of(&[(0x1000, 4, Usable)]);
        let mut storage = [0; 1];
        let mut table = [0u8; 64];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map).with_purposes(&mut table);
        let table_frame = allocator.allocate_for(FramePurpose::PageTable).unwrap();
        let free_before = allocator.free_frames();

        assert_eq!(
            allocator.deallocate_as(table_frame, FramePurpose::Heap),
            Err(DeallocError::WrongPurpose {
                expected: FramePurpose::Heap,
                actual: Some(FramePurpose::PageTable),
            })
        );
        assert_eq!(allocator.free_frames(), free_before, "nothing was freed");
        assert_eq!(
            allocator.purpose_of(table_frame),
            Some(FramePurpose::PageTable)
        );
        assert_eq!(
            allocator.deallocate_as(table_frame, FramePurpose::PageTable),
            Ok(())
        );
    }

    #[test]
    fn frames_handed_out_before_the_table_existed_are_recorded_as_boot() {
        let map = map_of(&[(0x1000, 6, Usable)]);
        let mut storage = [0; 1];
        let mut table = [0xEEu8; 64];
        let mut allocator = BitmapFrameAllocator::new(&mut storage, &map);
        let early = allocator.allocate().unwrap();
        let mut allocator = allocator.with_purposes(&mut table);

        assert_eq!(allocator.purpose_of(early), Some(FramePurpose::Boot));
        assert_eq!(allocator.frames_for(FramePurpose::Boot), 1);
        // Withheld frames and free ones carry nothing, whatever the table
        // held before.
        assert_eq!(allocator.purpose_of(frame(0)), None);
        assert_eq!(allocator.purpose_of(frame(0x2000)), None);
        assert_eq!(allocator.deallocate(early), Ok(()));
        assert_eq!(allocator.frames_for(FramePurpose::Boot), 0);
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
