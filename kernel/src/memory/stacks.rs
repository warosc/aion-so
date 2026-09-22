//! Kernel stacks, each with unmapped guard pages around it.
//!
//! A stack that grows past its end runs into a page that is mapped to
//! nothing, so the very first byte written beyond it faults instead of
//! quietly overwriting whatever sits below. Since the faulting core can no
//! longer push an exception frame either, the fault escalates to a double
//! fault, which runs on its own stack (also guarded) and stops the kernel
//! with a legible message.
//!
//! The layout inside the stacks area is, in order: guard page, kernel
//! stack, guard page, double-fault stack, guard page. Nothing else is ever
//! mapped in that part of kernel space.

use harlan_hal::paging::{MapError, PAGE_SIZE, Page, PageFlags, PageMapper};

use super::frame_allocator::FramePurpose;
use super::zeroed_frames::KernelFrames;

/// 64 KiB for the kernel's own stack.
pub const KERNEL_STACK_PAGES: u64 = 16;
/// 16 KiB for the double-fault handler, as much as the static it replaces.
pub const DOUBLE_FAULT_STACK_PAGES: u64 = 4;

/// A mapped stack: `bottom` is its lowest byte, `top` the first address
/// past it (where the stack pointer starts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stack {
    bottom: u64,
    top: u64,
}

impl Stack {
    pub fn top(self) -> u64 {
        self.top
    }

    pub fn bottom(self) -> u64 {
        self.bottom
    }

    pub fn size(self) -> u64 {
        self.top - self.bottom
    }
}

/// Maps `pages` writable, never-executable pages directly above `guard`,
/// which is left unmapped. The caller must leave the page above the stack
/// unmapped too (`map_kernel_stacks` does).
pub fn map_with_guard(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    guard: Page,
    pages: u64,
) -> Result<Stack, MapError> {
    let stack = PageFlags {
        writable: true,
        executable: false,
    };
    let bottom = guard.start_address() + PAGE_SIZE;
    for index in 0..pages {
        let page = Page::containing_address(bottom + index * PAGE_SIZE);
        let frame = frames
            .allocate_for(FramePurpose::Stack)
            .ok_or(MapError::OutOfFrames)?;
        // SAFETY: the frame is fresh from the allocator, so nothing else
        // uses it, and this area of kernel space belongs to the stacks
        // alone.
        unsafe { mapper.map(page, frame, stack, frames) }?;
    }
    Ok(Stack {
        bottom,
        top: bottom + pages * PAGE_SIZE,
    })
}

/// Maps the kernel stack and the double-fault stack inside the stacks area
/// that starts at `base`, with a guard page below each one and above the
/// last. Returns them in that order.
pub fn map_kernel_stacks(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    base: u64,
) -> Result<(Stack, Stack), MapError> {
    let kernel = map_with_guard(
        mapper,
        frames,
        Page::containing_address(base),
        KERNEL_STACK_PAGES,
    )?;
    // The page above the kernel stack is this one's guard page.
    let double_fault = map_with_guard(
        mapper,
        frames,
        Page::containing_address(kernel.top()),
        DOUBLE_FAULT_STACK_PAGES,
    )?;
    Ok((kernel, double_fault))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::frame_allocator::BitmapFrameAllocator;
    use crate::memory::zeroed_frames::{PhysWindow, ZeroedFrames};
    use harlan_hal::frame::FrameAllocator;
    use harlan_hal::frame::PhysFrame;
    use harlan_hal::memory_map::{MemoryMap, MemoryRegion, MemoryRegionKind};
    use harlan_hal::paging::UnmapError;
    use std::collections::BTreeMap;

    /// Records what was mapped where, without touching any page table.
    #[derive(Default)]
    struct FakeMapper {
        mapped: BTreeMap<u64, (PhysFrame, PageFlags)>,
    }

    impl PageMapper for FakeMapper {
        unsafe fn map(
            &mut self,
            page: Page,
            frame: PhysFrame,
            flags: PageFlags,
            _frames: &mut dyn FrameAllocator,
        ) -> Result<(), MapError> {
            if self.mapped.contains_key(&page.start_address()) {
                return Err(MapError::AlreadyMapped);
            }
            self.mapped.insert(page.start_address(), (frame, flags));
            Ok(())
        }

        unsafe fn unmap(&mut self, page: Page) -> Result<PhysFrame, UnmapError> {
            self.mapped
                .remove(&page.start_address())
                .map(|(frame, _)| frame)
                .ok_or(UnmapError::NotMapped)
        }

        fn translate(&self, addr: u64) -> Option<u64> {
            let page = Page::containing_address(addr);
            self.mapped
                .get(&page.start_address())
                .map(|(frame, _)| frame.start_address() + (addr - page.start_address()))
        }
    }

    /// Real frames of the test process, so the zeroing on the way out
    /// writes somewhere valid.
    struct TestMemory {
        _buffer: Vec<u8>,
        bitmap: Vec<u64>,
        map: MemoryMap,
    }

    fn test_memory(frames: u64) -> TestMemory {
        let bytes = ((frames + 1) * PAGE_SIZE) as usize;
        let buffer = vec![0xAAu8; bytes];
        let first = buffer.as_ptr().addr().next_multiple_of(PAGE_SIZE as usize) as u64;
        let mut map = MemoryMap::new();
        assert!(map.push(MemoryRegion {
            start_phys_addr: first,
            page_count: frames,
            kind: MemoryRegionKind::Usable,
        }));
        TestMemory {
            _buffer: buffer,
            bitmap: vec![0; (first / PAGE_SIZE / 64 + frames / 64 + 2) as usize],
            map,
        }
    }

    const BASE: u64 = 0xFFFF_8100_0000_0000;

    #[test]
    fn stacks_sit_between_unmapped_guard_pages() {
        let mut memory = test_memory(64);
        let allocator = BitmapFrameAllocator::new(&mut memory.bitmap, &memory.map);
        // SAFETY: the "frames" are the test process's own pages, reachable
        // at their own addresses.
        let mut frames = unsafe { ZeroedFrames::new(allocator, PhysWindow::identity()) };
        let mut mapper = FakeMapper::default();

        let (kernel, double_fault) = map_kernel_stacks(&mut mapper, &mut frames, BASE).unwrap();

        assert_eq!(kernel.bottom(), BASE + PAGE_SIZE);
        assert_eq!(kernel.size(), KERNEL_STACK_PAGES * PAGE_SIZE);
        assert_eq!(double_fault.bottom(), kernel.top() + PAGE_SIZE);
        assert_eq!(double_fault.size(), DOUBLE_FAULT_STACK_PAGES * PAGE_SIZE);

        // The guard pages: below each stack, and above the last one.
        for guard in [BASE, kernel.top(), double_fault.top()] {
            assert_eq!(mapper.translate(guard), None, "{guard:#x} is not a guard");
        }
        // Every byte of both stacks is mapped, writable and not executable.
        for stack in [kernel, double_fault] {
            for addr in (stack.bottom()..stack.top()).step_by(PAGE_SIZE as usize) {
                assert!(mapper.translate(addr).is_some(), "{addr:#x} not mapped");
                let (_, flags) = mapper.mapped[&addr];
                assert!(flags.writable && !flags.executable);
            }
        }
    }

    #[test]
    fn stacks_never_share_a_frame() {
        let mut memory = test_memory(64);
        let allocator = BitmapFrameAllocator::new(&mut memory.bitmap, &memory.map);
        // SAFETY: as above.
        let mut frames = unsafe { ZeroedFrames::new(allocator, PhysWindow::identity()) };
        let mut mapper = FakeMapper::default();
        map_kernel_stacks(&mut mapper, &mut frames, BASE).unwrap();

        let used: Vec<PhysFrame> = mapper.mapped.values().map(|&(frame, _)| frame).collect();
        let mut distinct = used.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            used.len(),
            (KERNEL_STACK_PAGES + DOUBLE_FAULT_STACK_PAGES) as usize
        );
        assert_eq!(distinct.len(), used.len(), "a frame was mapped twice");
    }

    #[test]
    fn running_out_of_frames_is_reported() {
        let mut memory = test_memory(4);
        let allocator = BitmapFrameAllocator::new(&mut memory.bitmap, &memory.map);
        // SAFETY: as above.
        let mut frames = unsafe { ZeroedFrames::new(allocator, PhysWindow::identity()) };
        let mut mapper = FakeMapper::default();
        assert_eq!(
            map_kernel_stacks(&mut mapper, &mut frames, BASE),
            Err(MapError::OutOfFrames)
        );
    }
}
