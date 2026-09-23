//! Moving the firmware's runtime services out of the lower half.
//!
//! UEFI keeps a handful of ranges alive after `ExitBootServices`, and by
//! default they are where the firmware put them: the lower half, which is
//! about to be user space. `SetVirtualAddressMap` is the way out — the
//! firmware relocates its own pointers to addresses we choose — and it can
//! be called **once per boot**, only while the old mappings are still
//! there (docs/adr/0016-fase3-set-virtual-address-map.md).
//!
//! The order is the whole trick: map the new addresses, call, and only
//! then take the old ones away.

use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::frame::{PhysFrame, PhysRange};
use harlan_hal::memory_map::{MemoryMap, MemoryRegionKind};
use harlan_hal::paging::{PAGE_SIZE, Page, PageFlags, PageMapper};
use harlan_hal::{error, info};

use super::zeroed_frames::KernelFrames;

/// What the bootloader lends the kernel: the one call that can move the
/// firmware, given the base the kernel has mapped it at. Answers how many
/// descriptors were moved, or `None` if the firmware refused.
///
/// A function and not data, because only the bootloader speaks UEFI and
/// only the kernel decides the layout.
pub type Relocate = unsafe fn(VirtAddr) -> Option<u64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveError {
    /// A region's page count overflows, so the map cannot be trusted.
    BadRegion,
    /// The new addresses could not be mapped.
    Mapping(harlan_hal::paging::MapError),
    /// The firmware would not relocate itself.
    Refused,
}

/// Maps every runtime range at `base + its physical address` and then asks
/// the firmware to move there.
///
/// Returns how many pages were mapped, so the caller can say what it cost.
///
/// # Safety
///
/// * The kernel must own its page tables and reach frames through its own
///   window: this maps pages and the firmware will read them.
/// * `relocate` must be the bootloader's, called at most once per boot,
///   with the firmware's old mappings still in place.
/// * Nothing else may be using `base`'s slot of kernel space.
pub unsafe fn move_to_kernel_space(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    map: &MemoryMap,
    base: VirtAddr,
    relocate: Relocate,
) -> Result<u64, MoveError> {
    let mut pages = 0;
    for region in map.iter().filter(|region| region.attributes.runtime) {
        let Some(len) = region.page_count.checked_mul(PAGE_SIZE) else {
            return Err(MoveError::BadRegion);
        };
        let range = PhysRange::new(region.start_phys_addr, len);
        // The same permissions and caching the range has below, because
        // this is the same memory seen from somewhere else.
        let flags = PageFlags::kernel(true, region.kind == MemoryRegionKind::RuntimeCode)
            .cached_as(region.attributes.cache);
        for offset in (0..len).step_by(PAGE_SIZE as usize) {
            let frame = PhysFrame::containing_address(range.start + offset);
            let page = Page::containing_address(moved(base, range.start + offset));
            // SAFETY: this is the firmware's own memory, which the frame
            // allocator has always withheld, and this slot of kernel space
            // is used by nothing else (the caller's contract).
            unsafe { mapper.map(page, frame, flags, frames) }.map_err(MoveError::Mapping)?;
            pages += 1;
        }
        info!(
            "HARLAN: the firmware's {}..{} will answer at {:#x}",
            range.start,
            range.end(),
            moved(base, range.start)
        );
    }

    // SAFETY: the new addresses are mapped, the old ones are still there,
    // and this is the only call (the caller's contract).
    match unsafe { relocate(base) } {
        Some(moved) => {
            info!(
                "HARLAN: the firmware moved {moved} descriptor(s) into kernel space at {base:#x}; {pages} page(s) mapped"
            );
            Ok(pages)
        }
        None => {
            error!("HARLAN: the firmware refused to relocate its runtime services");
            Err(MoveError::Refused)
        }
    }
}

/// Where a physical address of the firmware's answers once it has moved.
pub fn moved(base: VirtAddr, physical: PhysAddr) -> VirtAddr {
    VirtAddr::new(base.as_u64() + physical.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::frame_allocator::BitmapFrameAllocator;
    use crate::memory::zeroed_frames::{PhysWindow, ZeroedFrames};
    use core::sync::atomic::{AtomicU64, Ordering};
    use harlan_hal::frame::FrameAllocator;
    use harlan_hal::memory_map::{CachePolicy, MemoryRegion, RegionAttributes};
    use harlan_hal::paging::{MapError, UnmapError};
    use std::collections::BTreeMap;

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

    /// Where the stub was told the firmware would answer.
    static ASKED_FOR: AtomicU64 = AtomicU64::new(0);

    unsafe fn accepts(base: VirtAddr) -> Option<u64> {
        ASKED_FOR.store(base.as_u64(), Ordering::Release);
        Some(6)
    }

    unsafe fn refuses(_base: VirtAddr) -> Option<u64> {
        None
    }

    fn region(
        start: u64,
        pages: u64,
        kind: MemoryRegionKind,
        runtime: bool,
        cache: CachePolicy,
    ) -> MemoryRegion {
        MemoryRegion {
            start_phys_addr: PhysAddr::new(start),
            page_count: pages,
            kind,
            attributes: RegionAttributes { runtime, cache },
        }
    }

    fn firmware_map() -> MemoryMap {
        let mut map = MemoryMap::new();
        // Code, data it writes, the registers of a device it talks to,
        // and one range that is none of its business.
        assert!(map.push(region(
            0x1000,
            1,
            MemoryRegionKind::RuntimeCode,
            true,
            CachePolicy::WriteBack
        )));
        assert!(map.push(region(
            0x2000,
            1,
            MemoryRegionKind::RuntimeData,
            true,
            CachePolicy::WriteBack
        )));
        assert!(map.push(region(
            0x3000,
            1,
            MemoryRegionKind::Reserved,
            true,
            CachePolicy::Uncacheable
        )));
        assert!(map.push(region(
            0x4000,
            1,
            MemoryRegionKind::Usable,
            false,
            CachePolicy::WriteBack
        )));
        map
    }

    const BASE: VirtAddr = VirtAddr::new(0xFFFF_8280_0000_0000);

    fn no_frames<'a>(
        bitmap: &'a mut [u64],
        map: &'a MemoryMap,
    ) -> ZeroedFrames<BitmapFrameAllocator<'a>> {
        let allocator = BitmapFrameAllocator::new(bitmap, map);
        // SAFETY: the fake mapper never writes through the window.
        unsafe { ZeroedFrames::new(allocator, PhysWindow::identity()) }
    }

    /// Each range keeps what it is: code runs, data does not, and the
    /// caching the firmware reported travels with it.
    #[test]
    fn the_firmware_is_mapped_where_it_was_told_with_what_it_had() {
        let map = firmware_map();
        let (mut bitmap, empty) = ([0u64; 1], MemoryMap::new());
        let mut frames = no_frames(&mut bitmap, &empty);
        let mut mapper = FakeMapper::default();

        // SAFETY: the fake mapper touches no memory and the stub is not
        // the firmware.
        let pages = unsafe { move_to_kernel_space(&mut mapper, &mut frames, &map, BASE, accepts) }
            .expect("the stub accepts");
        assert_eq!(pages, 3, "the three runtime ranges, one page each");
        assert_eq!(ASKED_FOR.load(Ordering::Acquire), BASE.as_u64());

        let flags_at = |physical: u64| mapper.mapped[&moved(BASE, PhysAddr::new(physical))].1;
        assert!(flags_at(0x1000).executable, "runtime code has to run");
        assert!(!flags_at(0x2000).executable, "its data does not");
        assert!(!flags_at(0x3000).executable);
        assert!(!flags_at(0x1000).user, "and none of it is user space");

        assert_eq!(flags_at(0x2000).cache, CachePolicy::WriteBack);
        assert_eq!(
            flags_at(0x3000).cache,
            CachePolicy::Uncacheable,
            "the device's registers must not be cached"
        );

        // What is not the firmware's stays out of its new home.
        assert!(
            !mapper
                .mapped
                .contains_key(&moved(BASE, PhysAddr::new(0x4000)))
        );
        assert_eq!(mapper.mapped.len(), 3);
    }

    /// A firmware that will not move leaves nothing half done for the
    /// caller to guess at.
    #[test]
    fn a_firmware_that_refuses_is_reported() {
        let map = firmware_map();
        let (mut bitmap, empty) = ([0u64; 1], MemoryMap::new());
        let mut frames = no_frames(&mut bitmap, &empty);
        let mut mapper = FakeMapper::default();
        // SAFETY: as above.
        let moved = unsafe { move_to_kernel_space(&mut mapper, &mut frames, &map, BASE, refuses) };
        assert_eq!(moved, Err(MoveError::Refused));
    }

    #[test]
    fn a_firmware_address_becomes_the_base_plus_itself() {
        let base = VirtAddr::new(0xFFFF_8280_0000_0000);
        assert_eq!(
            moved(base, PhysAddr::new(0x0F5E_D000)),
            VirtAddr::new(0xFFFF_8280_0F5E_D000)
        );
        assert_eq!(moved(base, PhysAddr::new(0)), base);
    }
}
