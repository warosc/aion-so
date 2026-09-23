//! Virtual memory surface: what the kernel asks of the architecture's page
//! tables. `arch` implements it (x86_64: `KernelPageTable`); the kernel
//! consumes it without knowing the page-table format.

use crate::addr::{PhysAddr, VirtAddr};
use crate::frame::{FRAME_SIZE, FrameAllocator, PhysFrame, PhysRange};
use crate::memory_map::CachePolicy;

/// A range that has to stay mapped, and how.
///
/// The kernel's own code is executable and never written: mapping it
/// read-only is the other half of write xor execute. The firmware's
/// runtime services code is executable **and** written — OVMF writes
/// inside it, and `shutdown` faults with `#PF ... error_code=0x3` if that
/// range is read-only (measured; see
/// docs/adr/0011-fase2-write-xor-execute-inside-the-image.md). And what
/// that code reads and writes is data: writable, never executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRange {
    pub range: PhysRange,
    pub writable: bool,
    pub executable: bool,
    /// How it may be cached. Mapping memory-mapped I/O write-back would
    /// corrupt whatever is behind it, so this travels with the range
    /// instead of being assumed.
    pub cache: CachePolicy,
}

impl MappedRange {
    /// Code the kernel controls: executable, never written, ordinary RAM.
    pub const fn read_only_code(range: PhysRange) -> Self {
        Self {
            range,
            writable: false,
            executable: true,
            cache: CachePolicy::WriteBack,
        }
    }

    /// Code someone else controls and writes into.
    pub const fn writable_code(range: PhysRange, cache: CachePolicy) -> Self {
        Self {
            range,
            writable: true,
            executable: true,
            cache,
        }
    }

    /// What that code reads and writes: its own data, or the registers of
    /// a device it talks to.
    pub const fn data(range: PhysRange, cache: CachePolicy) -> Self {
        Self {
            range,
            writable: true,
            executable: false,
            cache,
        }
    }
}

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
    /// Reachable from ring 3. Without it the page belongs to the kernel
    /// and a user access to it faults, which is what isolation is made of
    /// (docs/adr/0014-fase3-syscall-abi-v0.md).
    pub user: bool,
    /// How it may be cached. RAM wants write-back; the registers of a
    /// device do not (docs/adr/0016-fase3-set-virtual-address-map.md).
    pub cache: CachePolicy,
}

impl PageFlags {
    /// The kernel's own memory: never reachable from ring 3, ordinary
    /// RAM unless told otherwise.
    pub const fn kernel(writable: bool, executable: bool) -> Self {
        Self {
            writable,
            executable,
            user: false,
            cache: CachePolicy::WriteBack,
        }
    }

    /// A process's memory.
    pub const fn user(writable: bool, executable: bool) -> Self {
        Self {
            writable,
            executable,
            user: true,
            cache: CachePolicy::WriteBack,
        }
    }

    /// The same page, cached as `cache` says.
    pub const fn cached_as(self, cache: CachePolicy) -> Self {
        Self { cache, ..self }
    }
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
