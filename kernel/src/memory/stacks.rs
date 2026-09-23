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

#[cfg(test)]
use harlan_hal::addr::PhysAddr;
use harlan_hal::addr::VirtAddr;
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
    bottom: VirtAddr,
    top: VirtAddr,
}

impl Stack {
    pub fn top(self) -> VirtAddr {
        self.top
    }

    pub fn bottom(self) -> VirtAddr {
        self.bottom
    }

    pub fn size(self) -> u64 {
        self.top.saturating_sub(self.bottom)
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
    let stack = PageFlags::kernel(true, false);
    let bottom = guard.start_address() + PAGE_SIZE;
    for index in 0..pages {
        let page = Page::containing_address(bottom + index * PAGE_SIZE);
        let Some(frame) = frames.allocate_for(FramePurpose::Stack) else {
            rollback_stack(mapper, frames, bottom, index);
            return Err(MapError::OutOfFrames);
        };
        // SAFETY: the frame is fresh from the allocator, so nothing else
        // uses it, and this area of kernel space belongs to the stacks
        // alone.
        if let Err(err) = unsafe { mapper.map(page, frame, stack, frames) } {
            let returned = frames.deallocate_as(frame, FramePurpose::Stack);
            debug_assert!(returned.is_ok());
            rollback_stack(mapper, frames, bottom, index);
            return Err(err);
        }
    }
    Ok(Stack {
        bottom,
        top: bottom + pages * PAGE_SIZE,
    })
}

fn rollback_stack(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    bottom: VirtAddr,
    mapped_pages: u64,
) {
    for index in 0..mapped_pages {
        let page = Page::containing_address(bottom + index * PAGE_SIZE);
        // SAFETY: only pages successfully mapped by `map_with_guard` in
        // this attempt are visited, and no caller can observe them yet.
        let frame = unsafe { mapper.unmap(page) };
        debug_assert!(frame.is_ok());
        if let Ok(frame) = frame {
            let returned = frames.deallocate_as(frame, FramePurpose::Stack);
            debug_assert!(returned.is_ok());
        }
    }
}

fn unmap_stack(mapper: &mut dyn PageMapper, frames: &mut KernelFrames<'_>, stack: Stack) {
    rollback_stack(mapper, frames, stack.bottom(), stack.size() / PAGE_SIZE);
}

/// How many pages the stack a syscall lands on gets. Same reasoning as
/// the double-fault stack: small, but with a guard page at each end.
pub const SYSCALL_STACK_PAGES: u64 = 4;

/// Maps the kernel stack, the double-fault stack and the syscall stack
/// inside the stacks area that starts at `base`, with a guard page below
/// each one and above the last. Returns them in that order.
///
/// The syscall stack is a third one on purpose: `syscall` does not change
/// the stack pointer, so the entry stub switches to this one, and it must
/// not be the stack the kernel was already using — that one has frames of
/// its own below the top (docs/adr/0014-fase3-syscall-abi-v0.md).
pub fn map_kernel_stacks(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    base: VirtAddr,
) -> Result<(Stack, Stack, Stack), MapError> {
    let kernel = map_with_guard(
        mapper,
        frames,
        Page::containing_address(base),
        KERNEL_STACK_PAGES,
    )?;
    // The page above the kernel stack is this one's guard page.
    let double_fault = match map_with_guard(
        mapper,
        frames,
        Page::containing_address(kernel.top()),
        DOUBLE_FAULT_STACK_PAGES,
    ) {
        Ok(stack) => stack,
        Err(err) => {
            unmap_stack(mapper, frames, kernel);
            return Err(err);
        }
    };
    let syscall = match map_with_guard(
        mapper,
        frames,
        Page::containing_address(double_fault.top()),
        SYSCALL_STACK_PAGES,
    ) {
        Ok(stack) => stack,
        Err(err) => {
            unmap_stack(mapper, frames, double_fault);
            unmap_stack(mapper, frames, kernel);
            return Err(err);
        }
    };
    Ok((kernel, double_fault, syscall))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::frame_allocator::BitmapFrameAllocator;
    use crate::memory::zeroed_frames::{PhysWindow, ZeroedFrames};
    use harlan_hal::frame::FrameAllocator;
    use harlan_hal::frame::PhysFrame;
    use harlan_hal::memory_map::{MemoryMap, MemoryRegion, MemoryRegionKind, RegionAttributes};
    use harlan_hal::paging::UnmapError;
    use std::collections::BTreeMap;

    /// Records what was mapped where, without touching any page table.
    #[derive(Default)]
    struct FakeMapper {
        mapped: BTreeMap<VirtAddr, (PhysFrame, PageFlags)>,
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

        fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
            let page = Page::containing_address(addr);
            self.mapped
                .get(&page.start_address())
                .map(|(frame, _)| frame.start_address() + addr.saturating_sub(page.start_address()))
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
            start_phys_addr: PhysAddr::new(first),
            page_count: frames,
            kind: MemoryRegionKind::Usable,
            attributes: RegionAttributes::none(),
        }));
        TestMemory {
            _buffer: buffer,
            bitmap: vec![0; (first / PAGE_SIZE / 64 + frames / 64 + 2) as usize],
            map,
        }
    }

    const BASE: VirtAddr = VirtAddr::new(0xFFFF_8100_0000_0000);

    #[test]
    fn stacks_sit_between_unmapped_guard_pages() {
        let mut memory = test_memory(64);
        let allocator = BitmapFrameAllocator::new(&mut memory.bitmap, &memory.map);
        // SAFETY: the "frames" are the test process's own pages, reachable
        // at their own addresses.
        let mut frames = unsafe { ZeroedFrames::new(allocator, PhysWindow::identity()) };
        let mut mapper = FakeMapper::default();

        let (kernel, double_fault, syscall) =
            map_kernel_stacks(&mut mapper, &mut frames, BASE).unwrap();

        assert_eq!(kernel.bottom(), BASE + PAGE_SIZE);
        assert_eq!(kernel.size(), KERNEL_STACK_PAGES * PAGE_SIZE);
        assert_eq!(double_fault.bottom(), kernel.top() + PAGE_SIZE);
        assert_eq!(double_fault.size(), DOUBLE_FAULT_STACK_PAGES * PAGE_SIZE);
        assert_eq!(syscall.bottom(), double_fault.top() + PAGE_SIZE);
        assert_eq!(syscall.size(), SYSCALL_STACK_PAGES * PAGE_SIZE);

        // The guard pages: below each stack, and above the last one.
        for guard in [BASE, kernel.top(), double_fault.top(), syscall.top()] {
            assert_eq!(mapper.translate(guard), None, "{guard:#x} is not a guard");
        }
        // Every byte of every stack is mapped, writable and not executable.
        for stack in [kernel, double_fault, syscall] {
            for page in 0..stack.size() / PAGE_SIZE {
                let addr = stack.bottom() + page * PAGE_SIZE;
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
            (KERNEL_STACK_PAGES + DOUBLE_FAULT_STACK_PAGES + SYSCALL_STACK_PAGES) as usize
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
        assert!(mapper.mapped.is_empty(), "partial stack mappings leaked");
        assert_eq!(frames.free_frames(), 4, "partial stack frames leaked");
    }
}
