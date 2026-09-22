//! Virtual memory surface: what the kernel asks of the architecture's page
//! tables. `arch` implements it (x86_64: `KernelPageTable`); the kernel
//! consumes it without knowing the page-table format.

use crate::addr::{PhysAddr, VirtAddr};
use crate::frame::{FRAME_SIZE, FrameAllocator, PhysFrame};

pub const PAGE_SIZE: u64 = FRAME_SIZE;

/// A `PAGE_SIZE`-aligned virtual page, named by its start address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Page {
    start: VirtAddr,
}

impl Page {
    /// `None` if `addr` is not `PAGE_SIZE`-aligned.
    pub const fn from_start_address(addr: VirtAddr) -> Option<Self> {
        if addr.is_aligned_to(PAGE_SIZE) {
            Some(Self { start: addr })
        } else {
            None
        }
    }

    /// The page that contains `addr` (rounds down).
    pub const fn containing_address(addr: VirtAddr) -> Self {
        Self {
            start: addr.align_down(PAGE_SIZE),
        }
    }

    pub const fn start_address(self) -> VirtAddr {
        self.start
    }
}

/// Access rights of a mapping. There is deliberately no user-accessible
/// flag: nothing runs outside ring 0 until Fase 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFlags {
    pub writable: bool,
    pub executable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The page is outside the range the kernel maps into; the rest of the
    /// address space belongs to someone else (the firmware's identity map
    /// today, user space later).
    OutsideKernelSpace,
    AlreadyMapped,
    /// No frame left for a page table the mapping needs.
    OutOfFrames,
    /// The frame offered for a new page table cannot be written where the
    /// mapper would write it.
    TableFrameNotWritable,
    /// A large-page mapping covers the address; splitting it is not
    /// supported.
    HugePageInTheWay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmapError {
    OutsideKernelSpace,
    NotMapped,
    HugePageInTheWay,
}

pub trait PageMapper {
    /// Maps `page` to `frame`, taking any page tables the mapping needs
    /// from `frames`.
    ///
    /// # Safety
    ///
    /// While the mapping is in use, nothing else may read or write `frame`
    /// (through the identity map, another mapping, or the allocator handing
    /// it out again) unless the caller manages that aliasing itself.
    unsafe fn map(
        &mut self,
        page: Page,
        frame: PhysFrame,
        flags: PageFlags,
        frames: &mut dyn FrameAllocator,
    ) -> Result<(), MapError>;

    /// Removes the mapping of `page` and returns the frame it pointed to.
    /// The frame is not freed: the caller still owns it.
    ///
    /// # Safety
    ///
    /// Nothing may access `page` afterwards: any reference or pointer into
    /// it is dangling from this call on.
    unsafe fn unmap(&mut self, page: Page) -> Result<PhysFrame, UnmapError>;

    /// The physical address `addr` translates to, if it is mapped.
    fn translate(&self, addr: VirtAddr) -> Option<PhysAddr>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn virt(addr: u64) -> VirtAddr {
        VirtAddr::new(addr)
    }

    #[test]
    fn from_start_address_accepts_only_aligned_addresses() {
        assert_eq!(
            Page::from_start_address(virt(0xFFFF_8000_0000_0000)).map(Page::start_address),
            Some(virt(0xFFFF_8000_0000_0000))
        );
        assert_eq!(Page::from_start_address(virt(0xFFFF_8000_0000_0008)), None);
    }

    #[test]
    fn containing_address_rounds_down_to_the_page_start() {
        assert_eq!(
            Page::containing_address(virt(0x7FFF)).start_address(),
            virt(0x7000)
        );
        assert_eq!(
            Page::containing_address(virt(u64::MAX)).start_address(),
            virt(u64::MAX - 0xFFF)
        );
    }
}
