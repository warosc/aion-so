//! Moving the kernel out of the address the firmware loaded it at.
//!
//! The kernel runs from its image's physical address, reached through the
//! identity map. Fase 3 needs the whole lower half for user processes, so
//! the image gets a second mapping in kernel space and the kernel goes on
//! running there (docs/adr/0012-fase3-higher-half-kernel.md).
//!
//! Two steps, in this order and no other:
//!
//! 1. **Map the alias.** Same frames, kernel-space addresses, and the same
//!    permissions the identity map gives them: code executable and
//!    read-only, everything else no-execute (ADR 0011).
//! 2. **Apply the image's relocations** for the difference between the two
//!    addresses. From that moment every absolute pointer in the kernel's
//!    data names the alias — which is why the alias has to exist first.
//!
//! The jump itself belongs to the caller: it is the one that knows which
//! function to continue in.

use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::frame::{PhysFrame, PhysRange};
use harlan_hal::paging::{PAGE_SIZE, Page, PageFlags, PageMapper};
use harlan_hal::pe::{self, CodeRanges, PeError};

use super::frame_allocator::FramePurpose;
use super::zeroed_frames::KernelFrames;

/// What the move needs to know, and what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// Where the image can now be reached from.
    pub base: VirtAddr,
    /// What to add to an address in the old image to get the new one.
    pub delta: u64,
    /// Pages of the image mapped at `base`.
    pub pages: u64,
    /// Of those, the ones mapped read-only because they hold only code.
    pub read_only: u64,
    /// Absolute addresses fixed up.
    pub relocations: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    /// The image does not start on a page boundary, so it cannot be mapped
    /// anywhere else.
    Unaligned,
    /// The image or its alias would run past the end of the address space.
    OutOfRange,
    /// The alias could not be mapped (out of frames, something already
    /// there).
    Mapping(harlan_hal::paging::MapError),
    /// The image's headers do not say where its absolute addresses are.
    Image(PeError),
}

/// Maps `image` at `base` and relocates it to run from there.
///
/// # Safety
///
/// * `base` must be in kernel space and its pages unused.
/// * `image` must be where this very kernel is loaded and running, mapped
///   and **writable** at its own physical address — the relocations are
///   written through it. That is true until the identity map is rebuilt
///   with the permissions of ADR 0011, so this runs before that.
/// * Nothing may be relying on the kernel's absolute addresses staying as
///   they are, other than this kernel itself, which is what the move is
///   for. Interrupts should be off from here until the caller has jumped
///   and reinstalled the descriptor tables.
pub unsafe fn prepare(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    base: VirtAddr,
    image: PhysRange,
    code: Option<CodeRanges>,
) -> Result<Move, MoveError> {
    if !image.start.is_aligned_to(PAGE_SIZE) || !base.is_aligned_to(PAGE_SIZE) {
        return Err(MoveError::Unaligned);
    }
    if image.start.checked_add(image.len).is_none() || base.checked_add(image.len).is_none() {
        return Err(MoveError::OutOfRange);
    }
    let delta = base.as_u64().wrapping_sub(image.start.as_u64());
    // SAFETY: forwarded from this function's contract.
    let mut mapped = unsafe { map_alias(mapper, frames, base, image, code, delta)? };
    // SAFETY: as above: the image is mapped writable at its own address.
    mapped.relocations = unsafe { relocate(image, delta)? };
    Ok(mapped)
}

/// Maps `image` at `base` with the permissions of ADR 0011.
///
/// # Safety
///
/// As `prepare`.
unsafe fn map_alias(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    base: VirtAddr,
    image: PhysRange,
    code: Option<CodeRanges>,
    delta: u64,
) -> Result<Move, MoveError> {
    let mut mapped = Move {
        base,
        delta,
        pages: 0,
        read_only: 0,
        relocations: 0,
    };
    for offset in (0..image.len).step_by(PAGE_SIZE as usize) {
        let frame = PhysFrame::containing_address(image.start + offset);
        let page = Page::containing_address(base + offset);
        // Same rule as the identity map: a page that holds nothing but
        // code is executable and read-only; anything else is writable and
        // no-execute.
        let (runs_code, only_code) = match code {
            Some(code) => {
                let start = frame.start_address();
                let end = start + PAGE_SIZE;
                (
                    code.iter().any(|range| range.overlaps(start, end)),
                    code.iter()
                        .any(|range| range.contains(start) && range.end().as_u64() >= end.as_u64()),
                )
            }
            // Without section information the whole image has to stay
            // executable and writable, as it was before ADR 0011.
            None => (true, false),
        };
        let flags = PageFlags::kernel(!only_code, runs_code);
        // SAFETY: the frames are the image's own, which the allocator has
        // always withheld, and this range of kernel space is used by
        // nothing else (the caller's contract).
        unsafe { mapper.map(page, frame, flags, frames) }.map_err(MoveError::Mapping)?;
        frames.label(frame, FramePurpose::Kernel);
        mapped.pages += 1;
        mapped.read_only += u64::from(only_code);
    }
    Ok(mapped)
}

/// Adds `delta` to every absolute address the image's relocation table
/// lists, and answers how many.
///
/// # Safety
///
/// As `prepare`: `image` must be mapped and writable at its own address.
unsafe fn relocate(image: PhysRange, delta: u64) -> Result<u64, MoveError> {
    // SAFETY: the image is mapped, readable and writable at its own
    // physical address (the caller's contract), and `image.len` bytes long
    // by definition. Read only, to find the addresses to fix.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            VirtAddr::new(image.start.as_u64()).as_ptr::<u8>(),
            image.len as usize,
        )
    };
    let relocations = pe::relocations(bytes).map_err(MoveError::Image)?;

    // `pe::relocations` settled that every one of these is inside the
    // image before handing out the first: relocating is all or nothing,
    // because an image half moved would then be run from the address it
    // no longer agrees with.
    let mut applied = 0;
    for rva in relocations.iter() {
        debug_assert!(rva + 8 <= image.len);
        let slot = VirtAddr::new(image.start.as_u64() + rva).as_ptr::<u64>();
        // SAFETY: `slot` is inside the image, which is mapped writable,
        // and the relocation table says it holds an absolute address. The
        // table is 8-byte aligned in practice; `read_unaligned` costs
        // nothing here and does not care either way.
        unsafe {
            let value = slot.read_unaligned();
            slot.write_unaligned(value.wrapping_add(delta));
        }
        applied += 1;
    }
    Ok(applied)
}

/// Where a lower-half address ends up after the move.
///
/// Refuses anything that is not in the image being moved: a value that is
/// already in kernel space is not an address the relocation applies to,
/// and turning it into one would produce a jump into nowhere.
pub fn moved(address: u64, image: PhysRange, moved: &Move) -> Option<VirtAddr> {
    let inside = image.contains(PhysAddr::new(address));
    inside.then(|| VirtAddr::new(address.wrapping_add(moved.delta)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::frame_allocator::BitmapFrameAllocator;
    use crate::memory::zeroed_frames::{PhysWindow, ZeroedFrames};
    use harlan_hal::frame::FrameAllocator;
    use harlan_hal::memory_map::MemoryMap;
    use harlan_hal::paging::{MapError, UnmapError};
    use std::collections::BTreeMap;

    /// Records what was mapped where, without touching a page table or a
    /// byte of memory.
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
            self.mapped
                .get(&Page::containing_address(addr).start_address())
                .map(|(frame, _)| frame.start_address())
        }
    }

    /// An allocator over an empty map: `map_alias` never allocates and
    /// never writes, it only asks the mapper.
    fn no_frames<'a>(
        bitmap: &'a mut [u64],
        map: &'a MemoryMap,
    ) -> ZeroedFrames<BitmapFrameAllocator<'a>> {
        let allocator = BitmapFrameAllocator::new(bitmap, map);
        // SAFETY: nothing in these tests writes through the window.
        unsafe { ZeroedFrames::new(allocator, PhysWindow::identity()) }
    }

    const BASE: VirtAddr = VirtAddr::new(0xFFFF_8180_0000_0000);

    /// The image's code is executable and read-only in the new mapping;
    /// its data is writable and no-execute. Same rule as the identity map.
    #[test]
    fn the_alias_keeps_write_xor_execute() {
        let image = range(0x10_0000, 4 * PAGE_SIZE);
        // Two pages of code, then data; the code ends mid-page, so the
        // last page of it holds data too and cannot be read-only.
        let code =
            pe::CodeRanges::new(&[PhysRange::new(PhysAddr::new(0x10_0000), 2 * PAGE_SIZE + 8)])
                .unwrap();
        let (mut bitmap, map) = ([0u64; 1], MemoryMap::new());
        let mut frames = no_frames(&mut bitmap, &map);
        let mut mapper = FakeMapper::default();

        // SAFETY: the fake mapper touches no memory, and `code` covers
        // part of the range, so no relocation is attempted here.
        let plan = unsafe { map_alias(&mut mapper, &mut frames, BASE, image, Some(code), 7) }
            .expect("mapping the alias");
        assert_eq!((plan.pages, plan.read_only), (4, 2));
        assert_eq!(plan.base, BASE);

        let flags_at = |offset: u64| mapper.mapped[&(BASE + offset)].1;
        for offset in [0, PAGE_SIZE] {
            assert_eq!(
                flags_at(offset),
                PageFlags::kernel(false, true),
                "the page at {offset:#x} is nothing but code"
            );
        }
        assert_eq!(
            flags_at(2 * PAGE_SIZE),
            PageFlags::kernel(true, true),
            "code and data share this page, so it stays writable"
        );
        assert_eq!(
            flags_at(3 * PAGE_SIZE),
            PageFlags::kernel(true, false),
            "plain data"
        );
        // Every page of the image is mapped to its own frame, in order.
        for offset in (0..image.len).step_by(PAGE_SIZE as usize) {
            let (frame, _) = mapper.mapped[&(BASE + offset)];
            assert_eq!(frame.start_address(), image.start + offset);
        }
    }

    /// Without section information nothing can be marked no-execute: the
    /// whole image stays as it was before ADR 0011.
    #[test]
    fn an_image_whose_sections_are_unknown_stays_executable() {
        let image = range(0x10_0000, 2 * PAGE_SIZE);
        let (mut bitmap, map) = ([0u64; 1], MemoryMap::new());
        let mut frames = no_frames(&mut bitmap, &map);
        let mut mapper = FakeMapper::default();
        // SAFETY: as above.
        let plan =
            unsafe { map_alias(&mut mapper, &mut frames, BASE, image, None, 0) }.expect("mapping");
        assert_eq!((plan.pages, plan.read_only), (2, 0));
        for offset in [0, PAGE_SIZE] {
            assert_eq!(
                mapper.mapped[&(BASE + offset)].1,
                PageFlags::kernel(true, true)
            );
        }
    }

    #[test]
    fn an_image_that_is_not_page_aligned_is_refused() {
        let (mut bitmap, map) = ([0u64; 1], MemoryMap::new());
        let mut frames = no_frames(&mut bitmap, &map);
        let mut mapper = FakeMapper::default();
        // SAFETY: refused before anything is mapped or read.
        let moved = unsafe {
            prepare(
                &mut mapper,
                &mut frames,
                BASE,
                range(0x10_0800, PAGE_SIZE),
                None,
            )
        };
        assert_eq!(moved, Err(MoveError::Unaligned));
        assert!(mapper.mapped.is_empty());
    }

    fn range(start: u64, len: u64) -> PhysRange {
        PhysRange::new(PhysAddr::new(start), len)
    }

    #[test]
    fn an_address_inside_the_image_moves_by_the_delta() {
        let image = range(0x100_000, 0x2000);
        let plan = Move {
            base: VirtAddr::new(0xFFFF_8180_0000_0000),
            delta: 0xFFFF_8180_0000_0000u64.wrapping_sub(0x100_000),
            pages: 2,
            read_only: 1,
            relocations: 7,
        };
        assert_eq!(
            moved(0x100_123, image, &plan),
            Some(VirtAddr::new(0xFFFF_8180_0000_0123))
        );
        // Outside the image: not something the move can place.
        assert_eq!(moved(0x0FF_FFF, image, &plan), None);
        assert_eq!(moved(0x102_000, image, &plan), None);
        assert_eq!(moved(0xFFFF_8180_0000_0000, image, &plan), None);
    }
}
