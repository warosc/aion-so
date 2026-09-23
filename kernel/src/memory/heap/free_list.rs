//! Address-ordered free-list heap: first fit, split on allocation,
//! coalescing on free.
//!
//! Every block and every hole starts on a `GRANULE` (16-byte) boundary and
//! is a whole number of granules long. A hole keeps its bookkeeping (`Hole`:
//! its size and the address of the next hole) in its own first 16 bytes,
//! so the free list needs no memory of its own and grows with
//! fragmentation. Because a granule is exactly the size of that header, any
//! padding cut off in front of or behind an allocation is itself a valid
//! hole: no byte is ever lost or left unaccounted for.
//!
//! Allocated blocks carry no header. `deallocate` recomputes a block's
//! extent from the `Layout` passed back, which the `GlobalAlloc` contract
//! guarantees is the one used to allocate it.
//!
//! Invariants (verified by `check`): holes are in address order, inside the
//! heap, at least one granule long, never overlapping and never touching
//! (touching holes are always merged), and their sizes add up to `free`.
//! Every hole access is bounds-checked against the heap, so a corrupted
//! header panics instead of sending a write anywhere else.

use core::alloc::Layout;
use core::ptr::{self, NonNull};

pub const GRANULE: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
struct Hole {
    size: usize,
    /// Address of the next hole, 0 for none.
    next: usize,
}

const _: () = assert!(size_of::<Hole>() == GRANULE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapCorruption {
    pub what: &'static str,
    pub at: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapStats {
    pub size: usize,
    pub free: usize,
    pub holes: usize,
    pub largest_hole: usize,
}

pub struct FreeListHeap {
    /// Pointer to the region given to `init`; every address the heap hands
    /// out or touches is derived from it, keeping its provenance. Null
    /// until `init`.
    base: *mut u8,
    start: usize,
    end: usize,
    /// Address of the first hole, 0 for none.
    head: usize,
    free: usize,
}

// SAFETY: the heap only refers to the region handed to `init`, which it
// owns exclusively; nothing about it is tied to the thread that created it.
unsafe impl Send for FreeListHeap {}

impl FreeListHeap {
    pub const fn empty() -> Self {
        Self {
            base: ptr::null_mut(),
            start: 0,
            end: 0,
            head: 0,
            free: 0,
        }
    }

    /// Hands `[start, start + size)` to the heap, trimmed inwards to
    /// granule boundaries.
    ///
    /// # Safety
    ///
    /// The region must be valid for reads and writes for as long as the
    /// heap is used, belong to the heap alone (nothing else reads or writes
    /// it), and `start` must carry provenance over all of it.
    ///
    /// Panics if the heap was already initialized.
    pub unsafe fn init(&mut self, start: *mut u8, size: usize) {
        assert!(self.base.is_null(), "heap initialized twice");
        let first = start
            .addr()
            .checked_next_multiple_of(GRANULE)
            .expect("heap region wraps around the address space");
        let end = start
            .addr()
            .checked_add(size)
            .expect("heap region wraps around the address space")
            / GRANULE
            * GRANULE;
        self.base = start;
        self.start = first;
        self.end = end.max(first);
        if self.end - self.start >= GRANULE {
            self.write_hole(
                first,
                Hole {
                    size: self.end - first,
                    next: 0,
                },
            );
            self.head = first;
            self.free = self.end - first;
        }
    }

    pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let size = block_size(layout)?;
        let align = layout.align().max(GRANULE);
        let mut prev = 0;
        let mut current = self.head;
        while current != 0 {
            let hole = self.read_hole(current);
            let hole_end = current + hole.size;
            if let Some(block) = current.checked_next_multiple_of(align)
                && let Some(block_end) = block.checked_add(size)
                && block_end <= hole_end
            {
                // What is left behind and in front of the block stays free,
                // each part a whole number of granules.
                let mut rest = hole.next;
                if block_end < hole_end {
                    self.write_hole(
                        block_end,
                        Hole {
                            size: hole_end - block_end,
                            next: rest,
                        },
                    );
                    rest = block_end;
                }
                if block > current {
                    self.write_hole(
                        current,
                        Hole {
                            size: block - current,
                            next: rest,
                        },
                    );
                    rest = current;
                }
                self.link(prev, rest);
                self.free -= size;
                return NonNull::new(self.base.with_addr(block));
            }
            prev = current;
            current = hole.next;
        }
        None
    }

    /// Returns a block to the heap, merging it with the holes it touches.
    ///
    /// # Safety
    ///
    /// `ptr` must have come from `allocate` on this heap with this same
    /// `layout` and not have been deallocated since, and nothing may use
    /// the block afterwards. The violations the heap can see (a block
    /// outside the heap, misaligned, or overlapping a hole, as a double
    /// free does) panic instead of corrupting the free list.
    pub unsafe fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let size = block_size(layout).expect("a layout this heap never allocates");
        let block = ptr.as_ptr().addr();
        let block_end = block
            .checked_add(size)
            .filter(|&end| block >= self.start && end <= self.end && block.is_multiple_of(GRANULE))
            .unwrap_or_else(|| {
                panic!("heap: freeing {block:#x}, which is not a block of this heap")
            });

        let mut prev = 0;
        let mut current = self.head;
        while current != 0 && current < block {
            prev = current;
            current = self.read_hole(current).next;
        }
        let prev_hole = (prev != 0).then(|| self.read_hole(prev));
        let overlaps_prev = prev_hole.is_some_and(|hole| prev + hole.size > block);
        let overlaps_next = current != 0 && block_end > current;
        assert!(
            !overlaps_prev && !overlaps_next,
            "heap: double free of {block:#x} (it overlaps free memory)"
        );

        let mut merged = Hole {
            size,
            next: current,
        };
        if current != 0 && block_end == current {
            let next_hole = self.read_hole(current);
            merged = Hole {
                size: size + next_hole.size,
                next: next_hole.next,
            };
        }
        match prev_hole {
            Some(hole) if prev + hole.size == block => self.write_hole(
                prev,
                Hole {
                    size: hole.size + merged.size,
                    next: merged.next,
                },
            ),
            _ => {
                self.write_hole(block, merged);
                self.link(prev, block);
            }
        }
        self.free += size;
    }

    /// Walks the free list and verifies every invariant, touching only
    /// addresses it has already checked are inside the heap.
    pub fn check(&self) -> Result<HeapStats, HeapCorruption> {
        let corrupt = |what, at| Err(HeapCorruption { what, at });
        let max_holes = (self.end - self.start) / GRANULE;
        let (mut free, mut holes, mut largest_hole) = (0, 0, 0);
        let mut previous_end = None;
        let mut current = self.head;
        while current != 0 {
            if holes >= max_holes {
                return corrupt("free list longer than the heap can hold", current);
            }
            if !current.is_multiple_of(GRANULE) || current < self.start || current >= self.end {
                return corrupt("hole outside the heap or misaligned", current);
            }
            if previous_end.is_some_and(|end| current <= end) {
                return corrupt("holes out of order, overlapping or not merged", current);
            }
            let hole = self.read_hole(current);
            if hole.size < GRANULE
                || !hole.size.is_multiple_of(GRANULE)
                || hole.size > self.end - current
            {
                return corrupt("hole with an impossible size", current);
            }
            free += hole.size;
            holes += 1;
            largest_hole = largest_hole.max(hole.size);
            previous_end = Some(current + hole.size);
            current = hole.next;
        }
        if free != self.free {
            return corrupt("free byte count does not match the free list", 0);
        }
        Ok(HeapStats {
            size: self.end - self.start,
            free,
            holes,
            largest_hole,
        })
    }

    fn link(&mut self, prev: usize, next: usize) {
        if prev == 0 {
            self.head = next;
        } else {
            let mut hole = self.read_hole(prev);
            hole.next = next;
            self.write_hole(prev, hole);
        }
    }

    /// Pointer to the hole header at `addr`, after checking that all 16
    /// bytes of it lie inside the heap. A hole header is only ever read or
    /// written through this.
    fn hole_ptr(&self, addr: usize) -> *mut Hole {
        assert!(
            addr.is_multiple_of(GRANULE)
                && addr >= self.start
                && addr.checked_add(GRANULE).is_some_and(|end| end <= self.end),
            "heap: free-list entry {addr:#x} outside the heap (corrupted free list?)"
        );
        self.base.with_addr(addr).cast()
    }

    fn read_hole(&self, addr: usize) -> Hole {
        // SAFETY: `hole_ptr` checked that the 16 aligned bytes are inside
        // the region `init`'s contract makes valid and exclusively ours.
        // The heap only reads headers of holes, which no block overlaps.
        unsafe { self.hole_ptr(addr).read() }
    }

    fn write_hole(&mut self, addr: usize, hole: Hole) {
        // SAFETY: as for `read_hole`; callers only write headers at the
        // start of memory that is (becoming) free, never inside a block
        // that has been handed out.
        unsafe { self.hole_ptr(addr).write(hole) }
    }
}

/// Size of the block that serves `layout`: at least one granule, rounded
/// up to whole granules. `None` only for sizes near `usize::MAX`.
fn block_size(layout: Layout) -> Option<usize> {
    layout.size().max(1).checked_next_multiple_of(GRANULE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap over a fresh, 16-byte-aligned buffer the test owns.
    struct TestHeap {
        heap: FreeListHeap,
        _buffer: Vec<u128>,
    }

    fn heap_of(bytes: usize) -> TestHeap {
        let mut buffer = vec![0u128; bytes / 16];
        let mut heap = FreeListHeap::empty();
        // SAFETY: the buffer outlives the heap (both live in `TestHeap`)
        // and nothing else touches it.
        unsafe { heap.init(buffer.as_mut_ptr().cast(), bytes) };
        TestHeap {
            heap,
            _buffer: buffer,
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    fn addr(ptr: NonNull<u8>) -> usize {
        ptr.as_ptr().addr()
    }

    #[test]
    fn an_uninitialized_heap_allocates_nothing() {
        let mut heap = FreeListHeap::empty();
        assert_eq!(heap.allocate(layout(8, 8)), None);
        assert_eq!(heap.check().unwrap().free, 0);
    }

    #[test]
    fn init_trims_the_region_to_whole_granules() {
        let mut buffer = vec![0u128; 64];
        let mut heap = FreeListHeap::empty();
        // SAFETY: the buffer outlives the heap and nothing else uses it.
        unsafe { heap.init(buffer.as_mut_ptr().cast::<u8>().wrapping_add(3), 1000) };
        let stats = heap.check().unwrap();
        // Buffer at B (16-aligned): [B + 3, B + 1003) trims to [B + 16, B + 992).
        assert_eq!(stats.size, 976);
        assert_eq!(stats.free, stats.size);
        assert!(heap.start.is_multiple_of(GRANULE) && heap.end.is_multiple_of(GRANULE));
    }

    #[test]
    #[should_panic(expected = "initialized twice")]
    fn init_twice_panics() {
        let mut test = heap_of(256);
        let mut other = vec![0u128; 16];
        // SAFETY: never reached past the assertion.
        unsafe { test.heap.init(other.as_mut_ptr().cast(), 256) };
    }

    #[test]
    fn blocks_are_aligned_disjoint_and_counted_until_exhaustion() {
        let mut test = heap_of(4096);
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        while let Some(ptr) = test.heap.allocate(layout(40, 8)) {
            let start = addr(ptr);
            assert!(start.is_multiple_of(GRANULE));
            for &(other, len) in &blocks {
                assert!(start + 48 <= other || other + len <= start, "overlap");
            }
            blocks.push((start, 48));
            test.heap.check().unwrap();
        }
        assert_eq!(blocks.len(), 4096 / 48);
        assert_eq!(test.heap.free, 4096 % 48);
    }

    #[test]
    fn large_alignments_leave_the_padding_free() {
        let mut test = heap_of(16 * 1024);
        let ptr = test.heap.allocate(layout(100, 4096)).unwrap();
        assert!(addr(ptr).is_multiple_of(4096));
        let stats = test.heap.check().unwrap();
        assert_eq!(stats.free, 16 * 1024 - 112);
        // SAFETY: allocated just above with this layout.
        unsafe { test.heap.deallocate(ptr, layout(100, 4096)) };
        assert_eq!(test.heap.check().unwrap().holes, 1);
    }

    #[test]
    fn zero_sized_layouts_take_one_granule() {
        let mut test = heap_of(256);
        let a = test.heap.allocate(layout(0, 1)).unwrap();
        let b = test.heap.allocate(layout(0, 1)).unwrap();
        assert_eq!(addr(b) - addr(a), GRANULE);
        assert_eq!(test.heap.free, 256 - 2 * GRANULE);
    }

    #[test]
    fn impossible_requests_fail_cleanly() {
        let mut test = heap_of(1024);
        assert_eq!(test.heap.allocate(layout(2048, 8)), None);
        assert_eq!(test.heap.allocate(layout(isize::MAX as usize - 8, 8)), None);
        assert_eq!(test.heap.allocate(layout(8, 1 << 40)), None);
        assert_eq!(test.heap.check().unwrap().free, 1024);
    }

    #[test]
    fn freed_neighbours_merge_into_one_hole() {
        let mut test = heap_of(1024);
        let l = layout(64, 16);
        let a = test.heap.allocate(l).unwrap();
        let b = test.heap.allocate(l).unwrap();
        let c = test.heap.allocate(l).unwrap();
        // SAFETY (all deallocations): each block was allocated above with
        // layout `l` and is freed once.
        unsafe { test.heap.deallocate(a, l) };
        unsafe { test.heap.deallocate(b, l) };
        // a and b merged: a 128-byte block fits exactly where a was.
        let ab = test.heap.allocate(layout(128, 16)).unwrap();
        assert_eq!(ab, a);
        unsafe { test.heap.deallocate(ab, layout(128, 16)) };
        unsafe { test.heap.deallocate(c, l) };
        let stats = test.heap.check().unwrap();
        assert_eq!((stats.holes, stats.free), (1, 1024));
    }

    #[test]
    fn every_free_order_ends_in_a_single_hole() {
        let l = layout(48, 16);
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            let mut test = heap_of(512);
            let blocks: Vec<_> = (0..4).map(|_| test.heap.allocate(l).unwrap()).collect();
            for i in order {
                // SAFETY: each block allocated above with `l`, freed once.
                unsafe { test.heap.deallocate(blocks[i], l) };
                test.heap.check().unwrap();
            }
            assert_eq!(test.heap.check().unwrap().holes, 1, "order {order:?}");
        }
    }

    #[test]
    #[should_panic(expected = "double free")]
    fn a_double_free_panics() {
        let mut test = heap_of(512);
        let l = layout(32, 16);
        let a = test.heap.allocate(l).unwrap();
        let _b = test.heap.allocate(l).unwrap();
        // SAFETY: the second call is the deliberate contract violation
        // under test; the heap detects it before writing anything.
        unsafe { test.heap.deallocate(a, l) };
        unsafe { test.heap.deallocate(a, l) };
    }

    #[test]
    #[should_panic(expected = "not a block of this heap")]
    fn freeing_foreign_memory_panics() {
        let mut test = heap_of(512);
        let mut elsewhere = [0u128; 4];
        let ptr = NonNull::new(elsewhere.as_mut_ptr().cast::<u8>()).unwrap();
        // SAFETY: deliberate contract violation, detected before any write.
        unsafe { test.heap.deallocate(ptr, layout(16, 16)) };
    }

    #[test]
    fn check_reports_a_corrupted_header() {
        let mut test = heap_of(512);
        let l = layout(32, 16);
        let a = test.heap.allocate(l).unwrap();
        let _b = test.heap.allocate(l).unwrap();
        // SAFETY: allocated above; freed once.
        unsafe { test.heap.deallocate(a, l) };
        // A buffer overrun from a neighbour scribbles over the hole's size.
        // SAFETY: `a` is now a hole inside the test's own buffer.
        unsafe { a.as_ptr().cast::<usize>().write(8) };
        assert_eq!(
            test.heap.check().map_err(|c| c.what),
            Err("hole with an impossible size")
        );
    }

    #[test]
    #[should_panic(expected = "outside the heap")]
    fn a_corrupted_next_pointer_panics_instead_of_writing_elsewhere() {
        let mut test = heap_of(512);
        let l = layout(32, 16);
        let a = test.heap.allocate(l).unwrap();
        let _b = test.heap.allocate(l).unwrap();
        // SAFETY: allocated above; freed once.
        unsafe { test.heap.deallocate(a, l) };
        // SAFETY: `a` is a hole in the test's buffer; its `next` field is
        // pointed outside the heap.
        unsafe { a.as_ptr().cast::<usize>().add(1).write(0x10) };
        let _ = test.heap.allocate(layout(512, 16));
    }

    /// Deterministic pseudo-random allocate/free sequence; every live block
    /// is filled with its own byte and verified before it is freed, so any
    /// overlap between blocks, or a header written into a live block, shows
    /// up as a mismatch.
    #[test]
    fn random_operations_never_overlap_and_leave_no_leak() {
        let mut test = heap_of(64 * 1024);
        let mut live: Vec<(NonNull<u8>, Layout, u8)> = Vec::new();
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        for step in 0..20_000 {
            let r = next();
            if r % 3 != 0 && live.len() < 64 {
                let l = layout(1 + (r >> 8) as usize % 700, 1 << ((r >> 20) % 9));
                if let Some(ptr) = test.heap.allocate(l) {
                    assert!(addr(ptr).is_multiple_of(l.align()));
                    let fill = (r >> 24) as u8;
                    // SAFETY: a fresh block of at least `l.size()` bytes.
                    unsafe { ptr.as_ptr().write_bytes(fill, l.size()) };
                    live.push((ptr, l, fill));
                }
            } else if !live.is_empty() {
                let (ptr, l, fill) = live.swap_remove(next() as usize % live.len());
                // SAFETY: a live block of `l.size()` bytes, filled above.
                let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), l.size()) };
                assert!(
                    bytes.iter().all(|&b| b == fill),
                    "block overwritten at step {step}"
                );
                // SAFETY: allocated with `l`, freed once.
                unsafe { test.heap.deallocate(ptr, l) };
            }
            test.heap.check().unwrap();
        }
        for (ptr, l, _) in live.drain(..) {
            // SAFETY: allocated with `l`, freed once.
            unsafe { test.heap.deallocate(ptr, l) };
        }
        let stats = test.heap.check().unwrap();
        assert_eq!((stats.holes, stats.free), (1, 64 * 1024));
    }
}
