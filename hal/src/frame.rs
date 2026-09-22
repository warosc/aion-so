//! Physical memory frames: the unit the kernel's frame allocator hands out
//! and, from Incremento 6 on, the unit page tables map. Firmware-agnostic
//! and architecture-agnostic plain data, like `memory_map`.

/// Size of one physical frame. Also the size of a UEFI page, which is
/// always 4 KiB regardless of architecture, so `MemoryRegion::page_count`
/// counts frames directly.
pub const FRAME_SIZE: u64 = 4096;

/// A range of physical addresses, as plain data: where the kernel image
/// was loaded, what a firmware region covers, and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysRange {
    pub start: u64,
    pub len: u64,
}

impl PhysRange {
    pub const fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }

    /// First address past the range, saturating (the range may come from
    /// firmware).
    pub const fn end(self) -> u64 {
        self.start.saturating_add(self.len)
    }

    pub const fn contains(self, addr: u64) -> bool {
        self.start <= addr && addr < self.end()
    }

    /// Whether any part of `[start, end)` is inside this range.
    pub const fn overlaps(self, start: u64, end: u64) -> bool {
        start < self.end() && self.start < end
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
    start: u64,
}

impl PhysFrame {
    /// `None` if `addr` is not `FRAME_SIZE`-aligned: an unaligned address
    /// passed where a frame is expected is a caller bug, not something to
    /// round silently.
    pub const fn from_start_address(addr: u64) -> Option<Self> {
        if addr.is_multiple_of(FRAME_SIZE) {
            Some(Self { start: addr })
        } else {
            None
        }
    }

    /// The frame that contains `addr` (rounds down).
    pub const fn containing_address(addr: u64) -> Self {
        Self {
            start: addr - addr % FRAME_SIZE,
        }
    }

    pub const fn start_address(self) -> u64 {
        self.start
    }

    /// Index of this frame counting from physical address 0.
    pub const fn number(self) -> u64 {
        self.start / FRAME_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_knows_what_it_covers() {
        let range = PhysRange::new(0x2000, 0x1000);
        assert_eq!(range.end(), 0x3000);
        assert!(range.contains(0x2000) && range.contains(0x2FFF));
        assert!(!range.contains(0x1FFF) && !range.contains(0x3000));
        assert!(range.overlaps(0x1000, 0x2001) && range.overlaps(0x2FFF, 0x9000));
        assert!(!range.overlaps(0x0, 0x2000) && !range.overlaps(0x3000, 0x9000));
    }

    #[test]
    fn a_range_that_would_wrap_around_saturates() {
        let range = PhysRange::new(u64::MAX - 0xF, u64::MAX);
        assert_eq!(range.end(), u64::MAX);
        assert!(range.contains(u64::MAX - 1));
    }

    #[test]
    fn from_start_address_accepts_only_aligned_addresses() {
        assert_eq!(
            PhysFrame::from_start_address(0x5000).map(PhysFrame::start_address),
            Some(0x5000)
        );
        assert_eq!(PhysFrame::from_start_address(0x5001), None);
        assert_eq!(PhysFrame::from_start_address(0x5FFF), None);
    }

    #[test]
    fn containing_address_rounds_down_to_the_frame_start() {
        assert_eq!(
            PhysFrame::containing_address(0x5FFF).start_address(),
            0x5000
        );
        assert_eq!(
            PhysFrame::containing_address(0x5000).start_address(),
            0x5000
        );
        assert_eq!(PhysFrame::containing_address(0).start_address(), 0);
    }

    #[test]
    fn containing_address_does_not_overflow_at_the_top_of_the_address_space() {
        let frame = PhysFrame::containing_address(u64::MAX);
        assert_eq!(frame.start_address(), u64::MAX - (FRAME_SIZE - 1));
    }

    #[test]
    fn number_counts_frames_from_zero() {
        assert_eq!(PhysFrame::containing_address(0).number(), 0);
        assert_eq!(PhysFrame::containing_address(0x3000).number(), 3);
    }
}
