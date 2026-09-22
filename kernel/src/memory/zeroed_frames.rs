//! Reaching physical memory, and handing out frames already zeroed.
//!
//! `PhysWindow` is the one place that turns a physical frame into a pointer
//! the kernel can write through; `ZeroedFrames` wraps a frame allocator so
//! that every frame it hands out is zeroed first. No consumer can forget:
//! a new page table can never start with junk entries, and no frame carries
//! what its previous owner left in it.
//!
//! The frame allocator itself stays pure — it only flips bits in a bitmap —
//! so all the memory access of this path lives here.

use harlan_hal::frame::{FRAME_SIZE, FrameAllocator, PhysFrame};

use super::frame_allocator::{BitmapFrameAllocator, DeallocError};

/// How the kernel reaches physical memory: every frame is readable and
/// writable at `base + its physical address`.
///
/// Today that window is the firmware's identity map (`base == 0`), which
/// the paging take-over checks before anything is written through it. When
/// the lower half becomes user space (Fase 3), only the base changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysWindow {
    base: u64,
}

impl PhysWindow {
    /// # Safety
    ///
    /// Every frame the kernel may allocate must be mapped, readable and
    /// writable, at `base + frame address`, for as long as this window is
    /// used, and nothing else may be using those frames through it.
    pub const unsafe fn new(base: u64) -> Self {
        Self { base }
    }

    /// The identity window: a frame's physical address is also the virtual
    /// address it can be reached at.
    ///
    /// # Safety
    ///
    /// As `new`.
    pub const unsafe fn identity() -> Self {
        // SAFETY: forwarded from this function's own contract.
        unsafe { Self::new(0) }
    }

    /// Pointer to the first byte of `frame`.
    pub fn frame_ptr(self, frame: PhysFrame) -> *mut u8 {
        let addr = self
            .base
            .checked_add(frame.start_address())
            .expect("physical window does not reach this frame");
        addr as usize as *mut u8
    }

    /// Overwrites `frame` with zeros.
    ///
    /// # Safety
    ///
    /// Nothing else may be reading or writing `frame`: the caller must own
    /// it (as it does for a frame just taken from the allocator).
    pub unsafe fn zero(self, frame: PhysFrame) {
        // SAFETY: the window's contract makes the whole frame writable at
        // this address, and the caller owns it.
        unsafe { self.frame_ptr(frame).write_bytes(0, FRAME_SIZE as usize) }
    }
}

/// A frame allocator whose frames arrive zeroed.
pub struct ZeroedFrames<A> {
    inner: A,
    window: PhysWindow,
    zeroed: u64,
}

impl<A: FrameAllocator> ZeroedFrames<A> {
    /// # Safety
    ///
    /// `window` must reach every frame `inner` can hand out (see
    /// `PhysWindow::new`).
    pub const unsafe fn new(inner: A, window: PhysWindow) -> Self {
        Self {
            inner,
            window,
            zeroed: 0,
        }
    }

    pub fn window(&self) -> PhysWindow {
        self.window
    }

    /// Frames zeroed so far, for the boot log.
    pub fn zeroed_frames(&self) -> u64 {
        self.zeroed
    }
}

impl<A: FrameAllocator> FrameAllocator for ZeroedFrames<A> {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.inner.allocate_frame()?;
        // SAFETY: the frame has just been handed to us, so nothing else is
        // using it, and the window reaches it (this type's contract).
        unsafe { self.window.zero(frame) };
        self.zeroed += 1;
        Some(frame)
    }
}

/// What the kernel passes around: the bitmap allocator, zeroing on the way
/// out. The bitmap's own operations stay available.
pub type KernelFrames<'a> = ZeroedFrames<BitmapFrameAllocator<'a>>;

impl KernelFrames<'_> {
    pub fn allocate(&mut self) -> Option<PhysFrame> {
        self.allocate_frame()
    }

    pub fn deallocate(&mut self, frame: PhysFrame) -> Result<(), DeallocError> {
        self.inner.deallocate(frame)
    }

    pub fn manages(&self, frame: PhysFrame) -> bool {
        self.inner.manages(frame)
    }

    pub fn free_frames(&self) -> u64 {
        self.inner.free_frames()
    }

    pub fn covered_frames(&self) -> u64 {
        self.inner.covered_frames()
    }

    pub fn uncovered_usable_frames(&self) -> u64 {
        self.inner.uncovered_usable_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Page-aligned test memory standing in for physical frames: the test
    /// process's own addresses, so the identity window reaches them.
    struct TestFrames {
        _buffer: Vec<u8>,
        frames: Vec<PhysFrame>,
        handed_out: usize,
    }

    impl TestFrames {
        fn new(count: usize) -> Self {
            let buffer = vec![0xAAu8; (count + 1) * FRAME_SIZE as usize];
            let first = buffer.as_ptr().addr().next_multiple_of(FRAME_SIZE as usize) as u64;
            let frames = (0..count)
                .map(|i| PhysFrame::containing_address(first + i as u64 * FRAME_SIZE))
                .collect();
            Self {
                _buffer: buffer,
                frames,
                handed_out: 0,
            }
        }

        fn bytes(&self, frame: PhysFrame) -> &[u8] {
            // SAFETY: `frame` is one of this test's own pages.
            unsafe {
                core::slice::from_raw_parts(
                    frame.start_address() as usize as *const u8,
                    FRAME_SIZE as usize,
                )
            }
        }
    }

    impl FrameAllocator for TestFrames {
        fn allocate_frame(&mut self) -> Option<PhysFrame> {
            let frame = *self.frames.get(self.handed_out)?;
            self.handed_out += 1;
            Some(frame)
        }
    }

    fn zeroed_over(frames: TestFrames) -> ZeroedFrames<TestFrames> {
        // SAFETY: the frames are the test process's own pages, so the
        // identity window reaches them, and only the test uses them.
        unsafe { ZeroedFrames::new(frames, PhysWindow::identity()) }
    }

    #[test]
    fn frames_arrive_zeroed_whatever_they_held() {
        let mut frames = zeroed_over(TestFrames::new(3));
        for _ in 0..3 {
            let frame = frames.allocate_frame().unwrap();
            assert!(
                frames.inner.bytes(frame).iter().all(|&b| b == 0),
                "frame {frame:?} still holds its previous contents"
            );
        }
        assert_eq!(frames.zeroed_frames(), 3);
        assert_eq!(frames.allocate_frame(), None);
    }

    #[test]
    fn only_the_frame_handed_out_is_zeroed() {
        let mut frames = zeroed_over(TestFrames::new(2));
        let first = frames.allocate_frame().unwrap();
        let untouched = frames.inner.frames[1];
        assert!(frames.inner.bytes(first).iter().all(|&b| b == 0));
        assert!(frames.inner.bytes(untouched).iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn the_window_places_a_frame_at_base_plus_its_address() {
        // SAFETY: never dereferenced in this test.
        let window = unsafe { PhysWindow::new(0xFFFF_8100_0000_0000) };
        let frame = PhysFrame::containing_address(0x2000);
        assert_eq!(window.frame_ptr(frame).addr() as u64, 0xFFFF_8100_0000_2000);
    }

    #[test]
    #[should_panic(expected = "does not reach this frame")]
    fn a_window_that_cannot_reach_a_frame_panics() {
        // SAFETY: never dereferenced in this test.
        let window = unsafe { PhysWindow::new(u64::MAX - 0xFFF) };
        let _ = window.frame_ptr(PhysFrame::containing_address(0x2000));
    }
}
