//! Physical memory frames: the unit the kernel's frame allocator hands out
//! and, from Incremento 6 on, the unit page tables map. Firmware-agnostic
//! and architecture-agnostic plain data, like `memory_map`.

use crate::addr::PhysAddr;

/// Size of one physical frame. Also the size of a UEFI page, which is
/// always 4 KiB regardless of architecture, so `MemoryRegion::page_count`
/// counts frames directly.
pub const FRAME_SIZE: u64 = 4096;

/// A range of physical addresses, as plain data: where the kernel image
/// was loaded, what a firmware region covers, and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysRange {
    pub start: PhysAddr,
    pub len: u64,
}

impl PhysRange {
    pub const fn new(start: PhysAddr, len: u64) -> Self {
        Self { start, len }
    }

    /// First address past the range, saturating (the range may come from
    /// firmware).
    pub const fn end(self) -> PhysAddr {
        match self.start.checked_add(self.len) {
            Some(end) => end,
            None => PhysAddr::new(u64::MAX),
        }
    }

    pub const fn contains(self, addr: PhysAddr) -> bool {
        self.start.as_u64() <= addr.as_u64() && addr.as_u64() < self.end().as_u64()
    }

    /// Whether any part of `[start, end)` is inside this range.
    pub const fn overlaps(self, start: PhysAddr, end: PhysAddr) -> bool {
        start.as_u64() < self.end().as_u64() && self.start.as_u64() < end.as_u64()
    }
}

/// Source of free physical frames. The kernel's frame allocator implements
/// it; architecture code that builds page tables consumes it, since `arch`
/// cannot depend on the `kernel` crate.
pub trait FrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame>;
}

/// A `FRAME_SIZE`-aligned block of physical memory, named by its start
/// address. Constructing one says nothing about who owns the memory — that
/// is the frame allocator's job — only that the address is aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysFrame {
    start: PhysAddr,
}

impl PhysFrame {
    /// `None` if `addr` is not `FRAME_SIZE`-aligned: an unaligned address
    /// passed where a frame is expected is a caller bug, not something to
    /// round silently.
    pub const fn from_start_address(addr: PhysAddr) -> Option<Self> {
        if addr.is_aligned_to(FRAME_SIZE) {
            Some(Self { start: addr })
        } else {
            None
        }
    }

    /// The frame that contains `addr` (rounds down).
    pub const fn containing_address(addr: PhysAddr) -> Self {
        Self {
            start: addr.align_down(FRAME_SIZE),
        }
    }

    pub const fn start_address(self) -> PhysAddr {
        self.start
    }

    /// Index of this frame counting from physical address 0.
    pub const fn number(self) -> u64 {
        self.start.as_u64() / FRAME_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phys(addr: u64) -> PhysAddr {
        PhysAddr::new(addr)
    }

    #[test]
    fn a_range_knows_what_it_covers() {
        let range = PhysRange::new(phys(0x2000), 0x1000);
        assert_eq!(range.end(), phys(0x3000));
        assert!(range.contains(phys(0x2000)) && range.contains(phys(0x2FFF)));
        assert!(!range.contains(phys(0x1FFF)) && !range.contains(phys(0x3000)));
        assert!(
            range.overlaps(phys(0x1000), phys(0x2001))
                && range.overlaps(phys(0x2FFF), phys(0x9000))
        );
        assert!(
            !range.overlaps(phys(0x0), phys(0x2000)) && !range.overlaps(phys(0x3000), phys(0x9000))
        );
    }

    #[test]
    fn a_range_that_would_wrap_around_saturates() {
        let range = PhysRange::new(phys(u64::MAX - 0xF), u64::MAX);
        assert_eq!(range.end(), phys(u64::MAX));
        assert!(range.contains(phys(u64::MAX - 1)));
    }

    #[test]
    fn from_start_address_accepts_only_aligned_addresses() {
        assert_eq!(
            PhysFrame::from_start_address(phys(0x5000)).map(PhysFrame::start_address),
            Some(phys(0x5000))
        );
        assert_eq!(PhysFrame::from_start_address(phys(0x5001)), None);
        assert_eq!(PhysFrame::from_start_address(phys(0x5FFF)), None);
    }

    #[test]
    fn containing_address_rounds_down_to_the_frame_start() {
        assert_eq!(
            PhysFrame::containing_address(phys(0x5FFF)).start_address(),
            phys(0x5000)
        );
        assert_eq!(
            PhysFrame::containing_address(phys(0x5000)).start_address(),
            phys(0x5000)
        );
        assert_eq!(
            PhysFrame::containing_address(phys(0)).start_address(),
            phys(0)
        );
    }

    #[test]
    fn containing_address_does_not_overflow_at_the_top_of_the_address_space() {
        let frame = PhysFrame::containing_address(phys(u64::MAX));
        assert_eq!(frame.start_address(), phys(u64::MAX - (FRAME_SIZE - 1)));
    }

    #[test]
    fn number_counts_frames_from_zero() {
        assert_eq!(PhysFrame::containing_address(phys(0)).number(), 0);
        assert_eq!(PhysFrame::containing_address(phys(0x3000)).number(), 3);
    }
}
