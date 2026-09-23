//! x86_64 4-level paging: the kernel's page mapper.
//!
//! The kernel boots on the firmware's page tables: an identity map of the
//! first 1 TiB (PML4 slots 0-1, 2 MiB pages) whose own table pages are
//! mapped read-only, with CR0.WP set (measured on OVMF, see
//! docs/adr/0005-fase2-kernel-page-tables.md). Writing any of those tables
//! would fault, so the kernel never does. `KernelPageTable::take_over`
//! copies the firmware's root table into a frame of the kernel's own and
//! loads it into CR3: every translation stays the same, the lower-level
//! firmware tables are shared read-only, and from then on each table the
//! kernel writes is its own.
//!
//! The mapper only maps and unmaps in kernel space (the higher half, PML4
//! slots 256-511), which the firmware leaves empty. The lower half keeps
//! the firmware's identity map and is reserved for user space later.
//!
//! The walking logic (`PageTables`) is generic over how table memory is
//! reached (`TableAccess`), so it is host-tested against simulated
//! physical memory. Only `PhysAccess` and the register accessors touch
//! the hardware.

use core::arch::asm;

use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::frame::{FrameAllocator, PhysFrame};
use harlan_hal::memory_map::CachePolicy;
use harlan_hal::paging::{MapError, MappedRange, Page, PageFlags, PageMapper, UnmapError};

/// First address of kernel space: PML4 slot 256, the start of the
/// canonical higher half. The bare number is what the walker below does
/// its arithmetic with; callers outside get the typed address.
const KERNEL_SPACE_BASE: u64 = 0xFFFF_8000_0000_0000;

/// First address of kernel space (see `KERNEL_SPACE_BASE`).
pub const KERNEL_SPACE_START: VirtAddr = VirtAddr::new(KERNEL_SPACE_BASE);

/// Where the kernel heap starts: PML4 slot 257, a slot of its own (see
/// docs/adr/0006-fase2-kernel-heap.md).
pub const KERNEL_HEAP_START: VirtAddr = VirtAddr::new(KERNEL_SPACE_BASE + (1 << 39));

/// Where the kernel's stacks live: PML4 slot 258, again a slot of its own,
/// so a stack that runs off its guard page can only ever land on an
/// unmapped page (see docs/fase2-notes.md, Incremento 10).
pub const KERNEL_STACKS_START: VirtAddr = VirtAddr::new(KERNEL_SPACE_BASE + 2 * (1 << 39));

/// Where all of physical memory is readable and writable, so the kernel
/// can reach a frame without the identity map of the lower half, which
/// becomes user space: PML4 slot 260
/// (docs/adr/0013-fase3-physical-window.md).
pub const KERNEL_PHYSMAP_START: VirtAddr = VirtAddr::new(KERNEL_SPACE_BASE + 4 * (1 << 39));

/// Where the firmware's runtime services are mapped once they have been
/// told to move there: PML4 slot 261. A physical address `p` of theirs
/// becomes `KERNEL_RUNTIME_START + p`, so filling in the map UEFI asks for
/// is one addition (docs/adr/0016-fase3-set-virtual-address-map.md).
pub const KERNEL_RUNTIME_START: VirtAddr = VirtAddr::new(KERNEL_SPACE_BASE + 5 * (1 << 39));

/// Where the kernel's own image is mapped so that it can stop running from
/// wherever the firmware put it: PML4 slot 259
/// (docs/adr/0012-fase3-higher-half-kernel.md).
pub const KERNEL_IMAGE_START: VirtAddr = VirtAddr::new(KERNEL_SPACE_BASE + 3 * (1 << 39));

const ENTRIES: usize = 512;
const PAGE: u64 = 4096;
const LARGE_PAGE: u64 = 2 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// How much physical address space `rebuild_identity_map` covers by
/// default: enough for the RAM the frame allocator can manage, the
/// framebuffer and legacy MMIO, on every machine this kernel has run on.
pub const DEFAULT_IDENTITY_LIMIT: u64 = 4 * GIB;

const PRESENT: u64 = 1 << 0;
/// Reachable from ring 3. Every level of the walk has to have it.
const USER: u64 = 1 << 2;
const WRITABLE: u64 = 1 << 1;
/// Page-level write-through (PWT) and cache disable (PCD). With the
/// default PAT, neither set means write-back, PWT alone write-through and
/// both together uncacheable.
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
/// In a PDPT or PD entry: this entry maps a 1 GiB or 2 MiB page itself.
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
/// Bits 12-51: the physical address of the next table or of the page.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const CR3_FLAGS: u64 = 0x18; // PWT | PCD
const CR4_PCIDE: u64 = 1 << 17;
const CR4_LA57: u64 = 1 << 12;
const IA32_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;

/// PML4, PDPT, PD and PT indices of `va`, in walk order.
fn table_indices(va: u64) -> [usize; 4] {
    [39, 30, 21, 12].map(|shift| ((va >> shift) & 0x1FF) as usize)
}

/// Bits 48-63 must repeat bit 47 (4-level paging).
fn is_canonical(va: u64) -> bool {
    matches!(va >> 47, 0 | 0x1_FFFF)
}

/// Why `take_over` refused. The kernel keeps running on the firmware's
/// tables, without a page mapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingError {
    FiveLevelPaging,
    PcidEnabled,
    /// EFER.NXE is off: the no-execute bit would be a reserved-bit fault.
    NoExecuteDisabled,
    /// A translation the walker relies on is not the identity.
    NotIdentityMapped,
    /// The firmware already maps something in kernel space.
    KernelSpaceInUse,
    OutOfFrames,
    TableFrameNotWritable,
    /// The identity limit must be a whole number of GiB, at most 512.
    BadIdentityLimit,
    /// A range that must stay executable falls outside the new map, which
    /// would leave the kernel without the code it is about to run.
    RequiredRangeOutsideIdentityMap,
    /// The physical window has to start at a PML4 slot of kernel space.
    WindowOutsideKernelSpace,
    /// Something is already mapped where the physical window would go.
    WindowSlotTaken,
}

/// What `rebuild_identity_map` built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityMapStats {
    /// Page tables the new map took (all of them the kernel's own).
    pub tables: u64,
    /// Physical address space it covers, from 0.
    pub limit: u64,
    /// 4 KiB pages left executable; everything else in the map is
    /// no-execute.
    pub executable_pages: u64,
    /// Of those, the ones that hold nothing but code and are therefore
    /// mapped read-only. A page that mixes code and data cannot be, so
    /// the two numbers differ if any section is not page-aligned.
    pub read_only_pages: u64,
}

/// The page-table bits that reproduce a cache policy.
///
/// Write-combining needs the PAT reprogrammed, which this kernel does not
/// do; until it does, a range that asked for it is mapped uncacheable —
/// slower, never wrong.
const fn cache_bits(policy: CachePolicy) -> u64 {
    match policy {
        CachePolicy::WriteBack | CachePolicy::Unspecified => 0,
        CachePolicy::WriteThrough => WRITE_THROUGH,
        CachePolicy::WriteCombining | CachePolicy::Uncacheable => CACHE_DISABLE | WRITE_THROUGH,
    }
}

/// How the walker reaches table memory, given a table's physical address.
trait TableAccess {
    /// What is added to a table's physical address to reach it. Zero for
    /// anything that can read physical memory directly, which is what the
    /// tests do.
    fn window(&self) -> u64 {
        0
    }

    fn read(&self, table: u64, index: usize) -> u64;
    fn write(&mut self, table: u64, index: usize, value: u64);
    /// Drops any cached translation of `va`.
    fn flush(&mut self, va: u64);
    /// Drops every cached translation (after replacing whole subtrees).
    fn flush_all(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Translation {
    phys: u64,
    writable: bool,
    /// Size of the page that resolved the walk (4 KiB, 2 MiB or 1 GiB).
    /// Whoever asks about a whole range can skip to that page's end
    /// instead of walking every frame inside it.
    page_size: u64,
}

struct PageTables<A: TableAccess> {
    root: u64,
    access: A,
}

impl<A: TableAccess> PageTables<A> {
    /// Page tables whose root is a fresh copy of `old_root`: the same
    /// translations, with the lower-level tables shared. Only the new root
    /// is written.
    fn adopt(
        access: A,
        old_root: u64,
        frames: &mut dyn FrameAllocator,
    ) -> Result<Self, PagingError> {
        let mut tables = Self {
            root: old_root,
            access,
        };
        let kernel_slots = table_indices(KERNEL_SPACE_BASE)[0]..ENTRIES;
        if kernel_slots
            .into_iter()
            .any(|slot| tables.access.read(old_root, slot) & PRESENT != 0)
        {
            return Err(PagingError::KernelSpaceInUse);
        }
        let new_root = tables.new_table(frames).map_err(|err| match err {
            MapError::OutOfFrames => PagingError::OutOfFrames,
            _ => PagingError::TableFrameNotWritable,
        })?;
        for slot in 0..ENTRIES {
            let entry = tables.access.read(old_root, slot);
            tables.access.write(new_root, slot, entry);
        }
        tables.root = new_root;
        Ok(tables)
    }

    fn translate(&self, va: u64) -> Option<Translation> {
        if !is_canonical(va) {
            return None;
        }
        let mut table = self.root;
        let mut writable = true;
        for (level, index) in table_indices(va).into_iter().enumerate() {
            let entry = self.access.read(table, index);
            if entry & PRESENT == 0 || (level == 0 && entry & HUGE != 0) {
                return None;
            }
            writable &= entry & WRITABLE != 0;
            if level == 3 || entry & HUGE != 0 {
                // 4 KiB, 2 MiB or 1 GiB. Masking with the page size also
                // drops the PAT bit (bit 12) of large-page entries.
                let size = 1u64 << (39 - 9 * level);
                let base = entry & ADDRESS_MASK & !(size - 1);
                return Some(Translation {
                    phys: base + (va & (size - 1)),
                    writable,
                    page_size: size,
                });
            }
            table = entry & ADDRESS_MASK;
        }
        None
    }

    fn map(
        &mut self,
        page: u64,
        frame: PhysFrame,
        flags: PageFlags,
        frames: &mut dyn FrameAllocator,
    ) -> Result<(), MapError> {
        // The lower half is user space: only a page that says so out loud
        // may be mapped there, and the kernel's own pages never may.
        if flags.user != (page < KERNEL_SPACE_BASE) {
            return Err(MapError::OutsideKernelSpace);
        }
        let indices = table_indices(page);
        let mut table = self.root;
        let mut upgraded = false;
        for &index in &indices[..3] {
            let entry = self.access.read(table, index);
            // Every level of the walk has to allow it, so a user page
            // carries the bit all the way up.
            let reachable = if flags.user {
                PRESENT | WRITABLE | USER
            } else {
                PRESENT | WRITABLE
            };
            table = if entry & PRESENT == 0 {
                let new = self.new_table(frames)?;
                // Linked only once fully zeroed: the hardware walker never
                // sees a half-built table.
                self.access.write(table, index, new | reachable);
                new
            } else if entry & HUGE != 0 {
                return Err(MapError::HugePageInTheWay);
            } else {
                // A table built for the kernel and now on a program's path
                // has to allow ring 3 too: the CPU ands the bit down the
                // walk.
                if flags.user && entry & USER == 0 {
                    self.access.write(table, index, entry | USER);
                    upgraded = true;
                }
                entry & ADDRESS_MASK
            };
        }
        if self.access.read(table, indices[3]) & PRESENT != 0 {
            return Err(MapError::AlreadyMapped);
        }
        let mut leaf = frame.start_address().as_u64() | PRESENT | cache_bits(flags.cache);
        if flags.writable {
            leaf |= WRITABLE;
        }
        if !flags.executable {
            leaf |= NO_EXECUTE;
        }
        if flags.user {
            leaf |= USER;
        }
        self.access.write(table, indices[3], leaf);
        self.access.flush(page);
        if upgraded {
            // Pages under those tables were cached with the old, stricter
            // permissions.
            self.access.flush_all();
        }
        Ok(())
    }

    fn unmap(&mut self, page: u64) -> Result<PhysFrame, UnmapError> {
        if page < KERNEL_SPACE_BASE {
            return Err(UnmapError::OutsideKernelSpace);
        }
        let indices = table_indices(page);
        let mut table = self.root;
        for &index in &indices[..3] {
            let entry = self.access.read(table, index);
            if entry & PRESENT == 0 {
                return Err(UnmapError::NotMapped);
            }
            if entry & HUGE != 0 {
                return Err(UnmapError::HugePageInTheWay);
            }
            table = entry & ADDRESS_MASK;
        }
        let leaf = self.access.read(table, indices[3]);
        if leaf & PRESENT == 0 {
            return Err(UnmapError::NotMapped);
        }
        self.access.write(table, indices[3], 0);
        self.access.flush(page);
        Ok(PhysFrame::containing_address(PhysAddr::new(
            leaf & ADDRESS_MASK,
        )))
    }

    /// Builds an identity map for `0..limit` in fresh frames and installs
    /// it as the whole lower half, leaving the null page unmapped. The
    /// tables that were there before are simply dropped: nothing points at
    /// them any more.
    fn rebuild_identity(
        &mut self,
        frames: &mut dyn FrameAllocator,
        limit: u64,
        executable: &[MappedRange],
    ) -> Result<IdentityMapStats, PagingError> {
        if limit == 0 || !limit.is_multiple_of(GIB) || limit > 512 * GIB {
            return Err(PagingError::BadIdentityLimit);
        }
        // Marking a range executable it cannot reach would build a map
        // without the code the caller says it still runs.
        if executable.iter().map(|code| code.range).any(|range| {
            range.len == 0
                || range.start.as_u64() >= limit
                || range.start.checked_add(range.len).is_none()
                || range.end().as_u64() > limit
        }) {
            return Err(PagingError::RequiredRangeOutsideIdentityMap);
        }
        let to_paging = |err| match err {
            MapError::OutOfFrames => PagingError::OutOfFrames,
            _ => PagingError::TableFrameNotWritable,
        };

        // Everything is built first and only linked into the root at the
        // end, so the map in use never loses a translation it is running
        // on.
        let pdpt = self.new_table(frames).map_err(to_paging)?;
        let (mut tables, mut executable_pages, mut read_only_pages) = (1, 0, 0);
        for slot in 0..(limit / GIB) as usize {
            let directory = self.new_table(frames).map_err(to_paging)?;
            tables += 1;
            for entry in 0..ENTRIES {
                let base = slot as u64 * GIB + entry as u64 * LARGE_PAGE;
                // A 2 MiB page will do wherever the whole range is data.
                // The first 2 MiB (the null page has to be left out) and
                // anything holding code need 4 KiB granularity.
                let fine_grained = base == 0
                    || executable.iter().any(|code| {
                        code.range
                            .overlaps(PhysAddr::new(base), PhysAddr::new(base + LARGE_PAGE))
                    });
                if !fine_grained {
                    self.access.write(
                        directory,
                        entry,
                        base | PRESENT | WRITABLE | HUGE | NO_EXECUTE,
                    );
                    continue;
                }
                let table = self.new_table(frames).map_err(to_paging)?;
                tables += 1;
                for index in 0..ENTRIES {
                    let page = base + index as u64 * PAGE;
                    // The null page stays out, so a null dereference faults
                    // instead of reading or writing real memory.
                    if page == 0 {
                        continue;
                    }
                    let runs_code = executable.iter().any(|code| {
                        code.executable
                            && code
                                .range
                                .overlaps(PhysAddr::new(page), PhysAddr::new(page + PAGE))
                    });
                    // A page that is code and nothing but code is never
                    // written, so it is mapped read-only: the other half of
                    // write xor execute. One that only overlaps a code
                    // range holds data too and stays writable.
                    let only_code = executable.iter().any(|code| {
                        code.range.contains(PhysAddr::new(page))
                            && code.range.end().as_u64() >= page + PAGE
                    }) && !executable.iter().any(|code| {
                        code.writable
                            && code
                                .range
                                .overlaps(PhysAddr::new(page), PhysAddr::new(page + PAGE))
                    });
                    executable_pages += u64::from(runs_code);
                    read_only_pages += u64::from(only_code);
                    let mut flags = PRESENT;
                    if !only_code {
                        flags |= WRITABLE;
                    }
                    if !runs_code {
                        flags |= NO_EXECUTE;
                    }
                    self.access.write(table, index, page | flags);
                }
                self.access
                    .write(directory, entry, table | PRESENT | WRITABLE);
            }
            self.access
                .write(pdpt, slot, directory | PRESENT | WRITABLE);
        }

        self.access.write(self.root, 0, pdpt | PRESENT | WRITABLE);
        for slot in 1..table_indices(KERNEL_SPACE_BASE)[0] {
            self.access.write(self.root, slot, 0);
        }
        self.access.flush_all();
        Ok(IdentityMapStats {
            tables,
            limit,
            executable_pages,
            read_only_pages,
        })
    }

    /// Maps `[0, limit)` of physical memory at `base`, writable and
    /// no-execute, in 2 MiB pages. Nothing is executed through this
    /// window and nothing but the kernel can reach it.
    fn map_physical_window(
        &mut self,
        frames: &mut dyn FrameAllocator,
        base: u64,
        limit: u64,
    ) -> Result<u64, PagingError> {
        if limit == 0 || !limit.is_multiple_of(GIB) || limit > 512 * GIB {
            return Err(PagingError::BadIdentityLimit);
        }
        if base < KERNEL_SPACE_BASE || !base.is_multiple_of(512 * GIB) {
            return Err(PagingError::WindowOutsideKernelSpace);
        }
        // Asked before a single frame is spent, so a window that cannot
        // be placed costs nothing.
        let root_slot = table_indices(base)[0];
        if self.access.read(self.root, root_slot) & PRESENT != 0 {
            return Err(PagingError::WindowSlotTaken);
        }
        let to_paging = |err| match err {
            MapError::OutOfFrames => PagingError::OutOfFrames,
            _ => PagingError::TableFrameNotWritable,
        };

        // Built whole and linked at the end, like the identity map: the
        // walker never sees a half-built window.
        let pdpt = self.new_table(frames).map_err(to_paging)?;
        let mut tables = 1;
        for slot in 0..(limit / GIB) as usize {
            let directory = self.new_table(frames).map_err(to_paging)?;
            tables += 1;
            for entry in 0..ENTRIES {
                let physical = slot as u64 * GIB + entry as u64 * LARGE_PAGE;
                self.access.write(
                    directory,
                    entry,
                    physical | PRESENT | WRITABLE | HUGE | NO_EXECUTE,
                );
            }
            self.access
                .write(pdpt, slot, directory | PRESENT | WRITABLE);
        }
        self.access
            .write(self.root, root_slot, pdpt | PRESENT | WRITABLE);
        self.access.flush_all();
        Ok(tables)
    }

    /// Leaves the lower half with `keep` mapped and nothing else.
    ///
    /// Everything the kernel needs is in kernel space by now; what is left
    /// down here is code that belongs to somebody else — the firmware's
    /// runtime services — and it has to stay reachable at the address it
    /// was compiled for.
    fn keep_only(
        &mut self,
        frames: &mut dyn FrameAllocator,
        keep: &[MappedRange],
    ) -> Result<u64, PagingError> {
        let to_paging = |err| match err {
            MapError::OutOfFrames => PagingError::OutOfFrames,
            _ => PagingError::TableFrameNotWritable,
        };
        if keep.iter().any(|code| {
            code.range.len == 0
                || code.range.start.as_u64() >= 512 * GIB
                || code.range.end().as_u64() > 512 * GIB
        }) {
            return Err(PagingError::RequiredRangeOutsideIdentityMap);
        }

        // Built whole while the old map is still usable — every table
        // frame has to be reachable to be zeroed — and linked at the end.
        // The other way round, emptying the lower half first, takes away
        // the very addresses the next table would be written through.
        let pdpt = self.new_table(frames).map_err(to_paging)?;
        let mut tables = 1;
        for code in keep {
            let first = code.range.start.as_u64() / PAGE * PAGE;
            let last = (code.range.end().as_u64() - 1) / PAGE * PAGE;
            for page in (first..=last).step_by(PAGE as usize) {
                // Everything here is below 512 GiB, so the walk starts at
                // the new PDPT, one level down from the root.
                let indices = table_indices(page);
                let mut table = pdpt;
                for &index in &indices[1..3] {
                    let entry = self.access.read(table, index);
                    table = if entry & PRESENT == 0 {
                        let new = self.new_table(frames).map_err(to_paging)?;
                        tables += 1;
                        self.access.write(table, index, new | PRESENT | WRITABLE);
                        new
                    } else {
                        entry & ADDRESS_MASK
                    };
                }
                let mut leaf = page | PRESENT | cache_bits(code.cache);
                if code.writable {
                    leaf |= WRITABLE;
                }
                if !code.executable {
                    leaf |= NO_EXECUTE;
                }
                self.access.write(table, indices[3], leaf);
            }
        }

        // The lower half is user space from here on. What is actually
        // reachable from ring 3 is decided by the leaves, and the
        // firmware's have no user bit.
        self.access
            .write(self.root, 0, pdpt | PRESENT | WRITABLE | USER);
        for slot in 1..table_indices(KERNEL_SPACE_BASE)[0] {
            self.access.write(self.root, slot, 0);
        }
        self.access.flush_all();
        Ok(tables)
    }

    /// A zeroed page table in a fresh frame. Tables are written through
    /// their physical address, so a frame that address does not reach
    /// writably is refused (and not returned to `frames`, which has no way
    /// back; that is one leaked frame on a path the checks in `take_over`
    /// make unexpected).
    fn new_table(&mut self, frames: &mut dyn FrameAllocator) -> Result<u64, MapError> {
        let frame = frames
            .allocate_frame()
            .ok_or(MapError::OutOfFrames)?
            .start_address()
            .as_u64();
        // The table is written through the window, so that is the address
        // that has to be mapped to it, and writable.
        match self.translate(self.access.window() + frame) {
            Some(t) if t.phys == frame && t.writable => {}
            _ => return Err(MapError::TableFrameNotWritable),
        }
        for index in 0..ENTRIES {
            self.access.write(frame, index, 0);
        }
        Ok(frame)
    }
}

/// Reaches a table through its physical address, which the firmware's
/// identity map (checked by `take_over`) makes a valid virtual address.
/// Private to this module: `PageTables` only hands it tables reached from
/// the active root or new tables `new_table` verified identity-mapped and
/// writable, with indices below 512, and only ever writes the kernel's own
/// tables (the host tests check that no firmware table is written).
struct PhysAccess {
    /// A table at physical address `p` is reached at `base + p`. Zero
    /// while the firmware's identity map is what the kernel runs under;
    /// the physical window's base once the lower half is user space
    /// (docs/adr/0013-fase3-physical-window.md).
    base: u64,
}

impl PhysAccess {
    const fn identity() -> Self {
        Self { base: 0 }
    }

    fn entry(&self, table: u64, index: usize) -> *mut u64 {
        ((self.base + table) as *mut u64).wrapping_add(index)
    }
}

impl TableAccess for PhysAccess {
    fn window(&self) -> u64 {
        self.base
    }

    fn read(&self, table: u64, index: usize) -> u64 {
        // SAFETY: `base + table` is where this page table is readable —
        // under the identity map, or through the kernel's window — and
        // `index < 512` keeps the 8-byte-aligned read inside that 4 KiB
        // page. Volatile because the CPU's page walker also reads (and
        // sets accessed bits in) this memory behind the compiler's back.
        unsafe { core::ptr::read_volatile(self.entry(table, index)) }
    }

    fn write(&mut self, table: u64, index: usize, value: u64) {
        // SAFETY: as for `read`, and `table` is one of the kernel's own
        // tables, mapped writable (see the type's documentation), which no
        // Rust reference points into.
        unsafe { core::ptr::write_volatile(self.entry(table, index), value) }
    }

    fn flush(&mut self, va: u64) {
        // SAFETY: `invlpg` only drops a cached translation; the next access
        // re-walks the tables. Not `nomem`: it must also order the entry
        // write before it.
        unsafe { asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags)) }
    }

    fn flush_all(&mut self) {
        // SAFETY: reloading CR3 with the value it already holds keeps the
        // same tables and drops every cached translation. Not `nomem`: the
        // entries written before it must land first.
        unsafe {
            asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack, preserves_flags));
        }
    }
}

/// The kernel's page tables, once it has taken over the root from the
/// firmware.
pub struct KernelPageTable {
    tables: PageTables<PhysAccess>,
}

impl KernelPageTable {
    /// How many bytes from `frame`'s start are identity-mapped writable by
    /// the page tables in use right now, or `None` if `frame` itself is
    /// not. This is the invariant an identity `PhysWindow` rests on, so
    /// the kernel checks it before writing through one.
    ///
    /// The answer runs to the end of the page that maps `frame`, so
    /// checking a whole range costs one walk per 2 MiB or 1 GiB page
    /// instead of one per frame.
    ///
    /// # Safety
    ///
    /// The active CR3 must be the firmware root and its table frames must
    /// be reachable at their physical addresses — the same precondition as
    /// `take_over`, which this is meant to run just before.
    pub unsafe fn active_identity_writable_run(frame: PhysFrame) -> Option<u64> {
        let tables = PageTables {
            root: read_cr3() & ADDRESS_MASK,
            access: PhysAccess::identity(),
        };
        let addr = frame.start_address().as_u64();
        match tables.translate(addr) {
            Some(t) if t.writable && t.phys == addr => {
                Some(t.page_size - (addr & (t.page_size - 1)))
            }
            _ => None,
        }
    }

    /// Checks the paging mode, copies the firmware's root table into a
    /// frame from `frames` and loads it into CR3.
    ///
    /// # Safety
    ///
    /// - Called at most once, on the only running core, before anything
    ///   else creates or changes page tables.
    /// - The firmware's page tables are still the active ones (nothing has
    ///   loaded CR3 since boot), and no interrupt handler touches page
    ///   tables.
    pub unsafe fn take_over(frames: &mut dyn FrameAllocator) -> Result<Self, PagingError> {
        let cr4 = read_cr4();
        if cr4 & CR4_LA57 != 0 {
            return Err(PagingError::FiveLevelPaging);
        }
        if cr4 & CR4_PCIDE != 0 {
            return Err(PagingError::PcidEnabled);
        }
        if read_efer() & EFER_NXE == 0 {
            return Err(PagingError::NoExecuteDisabled);
        }

        let cr3 = read_cr3();
        let firmware = PageTables {
            root: cr3 & ADDRESS_MASK,
            access: PhysAccess::identity(),
        };
        // The walker reads tables through their physical addresses, so the
        // identity map must hold where it matters: the root table itself
        // and the stack. (Unlikely garbage from a broken identity map would
        // also have to translate back to exactly these addresses.)
        let stack_probe = 0u8;
        let stack_addr = &raw const stack_probe as u64;
        for addr in [firmware.root, stack_addr] {
            if firmware.translate(addr).map(|t| t.phys) != Some(addr) {
                return Err(PagingError::NotIdentityMapped);
            }
        }

        let tables = PageTables::adopt(PhysAccess::identity(), firmware.root, frames)?;
        // SAFETY: the new root holds the same 512 entries as the active
        // one, so every translation (code, stack, data, the firmware's
        // runtime services) is unchanged across the switch; the old root is
        // left intact. The CR3 cache-control bits are carried over.
        unsafe { write_cr3(tables.root | (cr3 & CR3_FLAGS)) };
        Ok(Self { tables })
    }

    /// Physical address of the kernel's root table (PML4).
    pub fn root(&self) -> u64 {
        self.tables.root
    }

    /// Replaces the firmware's identity map with one built from the
    /// kernel's own frames, covering `0..limit`, leaving the null page
    /// unmapped and every page no-execute except the `executable` ranges.
    /// After this the kernel no longer reads or depends on any page table
    /// the firmware built.
    ///
    /// # Safety
    ///
    /// - Everything the kernel runs on or reaches through the identity map
    ///   — its image, its page tables, the framebuffer, and every frame the
    ///   allocator can hand out — must lie below `limit`.
    /// - `executable` must list every range of code the kernel still runs
    ///   through this map: its own image, and the firmware's runtime
    ///   services code. Leaving one out turns the next call into it into a
    ///   fault.
    /// - Nothing may depend on the null page being mapped, or on anything
    ///   the firmware mapped in the lower half above `limit`.
    /// - Called on the only core, with no interrupt handler touching page
    ///   tables.
    pub unsafe fn rebuild_identity_map(
        &mut self,
        frames: &mut dyn FrameAllocator,
        limit: u64,
        executable: &[MappedRange],
    ) -> Result<IdentityMapStats, PagingError> {
        self.tables.rebuild_identity(frames, limit, executable)
    }

    /// Opens the window onto physical memory at `base`, covering
    /// `[0, limit)`. Answers how many page tables it took.
    ///
    /// # Safety
    ///
    /// `base` must be a free PML4 slot of kernel space, the kernel must
    /// own its page tables (`take_over`), and nothing may already be
    /// reaching physical memory through `base`.
    pub unsafe fn map_physical_window(
        &mut self,
        frames: &mut dyn FrameAllocator,
        base: VirtAddr,
        limit: u64,
    ) -> Result<u64, PagingError> {
        self.tables
            .map_physical_window(frames, base.as_u64(), limit)
    }

    /// Reaches page tables through the window at `base` from now on,
    /// instead of at their physical addresses.
    ///
    /// # Safety
    ///
    /// The window must already map every page table, and every frame that
    /// may become one, writable at `base + physical address`.
    pub unsafe fn use_physical_window(&mut self, base: VirtAddr) {
        self.tables.access.base = base.as_u64();
    }

    /// Unmaps the lower half except `keep`, which stays where it is.
    ///
    /// # Safety
    ///
    /// Nothing of the kernel's may still live in the lower half: not its
    /// code, not its stack, not the window it reaches frames through, not
    /// the console's framebuffer. `keep` is what somebody else's code
    /// needs, at the addresses it was compiled for.
    pub unsafe fn keep_only_in_lower_half(
        &mut self,
        frames: &mut dyn FrameAllocator,
        keep: &[MappedRange],
    ) -> Result<u64, PagingError> {
        self.tables.keep_only(frames, keep)
    }
}

impl PageMapper for KernelPageTable {
    unsafe fn map(
        &mut self,
        page: Page,
        frame: PhysFrame,
        flags: PageFlags,
        frames: &mut dyn FrameAllocator,
    ) -> Result<(), MapError> {
        self.tables
            .map(page.start_address().as_u64(), frame, flags, frames)
    }

    unsafe fn unmap(&mut self, page: Page) -> Result<PhysFrame, UnmapError> {
        self.tables.unmap(page.start_address().as_u64())
    }

    fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
        self.tables
            .translate(addr.as_u64())
            .map(|t| PhysAddr::new(t.phys))
    }
}

fn read_cr3() -> u64 {
    let value;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

fn read_cr4() -> u64 {
    let value;
    // SAFETY: reading CR4 has no side effects.
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

fn read_efer() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: IA32_EFER exists on every x86_64 CPU (long mode is enabled
    // through it) and reading an MSR has no side effects.
    unsafe {
        asm!("rdmsr", in("ecx") IA32_EFER, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// # Safety
///
/// `value` must name a root table under which the code, stack and data in
/// use stay mapped exactly as before.
unsafe fn write_cr3(value: u64) {
    // SAFETY: forwarded from the caller. Not `nomem`: it changes what every
    // later memory access means.
    unsafe { asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use harlan_hal::frame::PhysRange;
    use std::collections::{BTreeMap, BTreeSet};

    /// What a frame holds before the kernel writes it: present-looking
    /// junk, so a table that is not fully zeroed shows up in a walk.
    const JUNK: u64 = 0xDEAD_BEEF_DEAD_B0A7;
    const USER: u64 = 1 << 2;

    /// Simulated physical memory holding page tables. Reading a frame that
    /// holds no table fails the test (the walker must never read
    /// non-table memory), and so does writing a firmware table.
    #[derive(Default)]
    struct FakeMemory {
        /// What the walker adds to a table's physical address, as the
        /// kernel's window does once the lower half is gone.
        window: u64,
        tables: BTreeMap<u64, [u64; ENTRIES]>,
        firmware: BTreeSet<u64>,
        writes: Vec<(u64, usize, u64)>,
        flushed: Vec<u64>,
        flushed_all: u32,
    }

    impl TableAccess for FakeMemory {
        fn window(&self) -> u64 {
            self.window
        }

        fn read(&self, table: u64, index: usize) -> u64 {
            self.tables
                .get(&table)
                .unwrap_or_else(|| panic!("read {table:#x}, which holds no table"))[index]
        }

        fn write(&mut self, table: u64, index: usize, value: u64) {
            assert!(
                !self.firmware.contains(&table),
                "wrote firmware table {table:#x}"
            );
            self.tables.entry(table).or_insert([JUNK; ENTRIES])[index] = value;
            self.writes.push((table, index, value));
        }

        fn flush(&mut self, va: u64) {
            self.flushed.push(va);
        }

        fn flush_all(&mut self) {
            self.flushed_all += 1;
        }
    }

    struct Frames(Vec<u64>);

    impl FrameAllocator for Frames {
        fn allocate_frame(&mut self) -> Option<PhysFrame> {
            (!self.0.is_empty())
                .then(|| PhysFrame::from_start_address(PhysAddr::new(self.0.remove(0))).unwrap())
        }
    }

    const FW_ROOT: u64 = 0x20_0000;
    const FW_PDPT: u64 = 0x20_1000;
    const FW_PD: u64 = 0x20_2000;
    const MIB: u64 = 1 << 20;
    const DATA: PageFlags = PageFlags::kernel(true, false);

    /// Firmware-style tables: identity map of the first 16 MiB in 2 MiB
    /// pages, with the 2 MiB page that holds the tables themselves mapped
    /// read-only, as OVMF does.
    fn firmware() -> FakeMemory {
        let mut root = [0; ENTRIES];
        root[0] = FW_PDPT | PRESENT | WRITABLE;
        let mut pdpt = [0; ENTRIES];
        pdpt[0] = FW_PD | PRESENT | WRITABLE;
        let mut pd = [0; ENTRIES];
        for (k, entry) in pd.iter_mut().enumerate().take(8) {
            *entry = (k as u64 * 2 * MIB) | PRESENT | WRITABLE | HUGE;
        }
        pd[1] &= !WRITABLE;
        let mut memory = FakeMemory::default();
        for (addr, table) in [(FW_ROOT, root), (FW_PDPT, pdpt), (FW_PD, pd)] {
            memory.tables.insert(addr, table);
            memory.firmware.insert(addr);
        }
        memory
    }

    fn adopted(frames: &mut Frames) -> PageTables<FakeMemory> {
        PageTables::adopt(firmware(), FW_ROOT, frames).unwrap()
    }

    fn page(offset: u64) -> u64 {
        KERNEL_SPACE_BASE + offset
    }

    fn frame(addr: u64) -> PhysFrame {
        PhysFrame::from_start_address(PhysAddr::new(addr)).unwrap()
    }

    /// The entry that maps `addr`, whatever level resolves it.
    fn leaf_entry(tables: &PageTables<FakeMemory>, addr: u64) -> u64 {
        let mut table = tables.root;
        for (level, index) in table_indices(addr).into_iter().enumerate() {
            let entry = tables.access.read(table, index);
            if level == 3 || entry & HUGE != 0 {
                return entry;
            }
            table = entry & ADDRESS_MASK;
        }
        unreachable!()
    }

    #[test]
    fn table_indices_split_an_address_in_walk_order() {
        assert_eq!(table_indices(KERNEL_SPACE_BASE), [256, 0, 0, 0]);
        let va = KERNEL_SPACE_BASE + (1 << 39) + (2 << 30) + (3 << 21) + (4 << 12) + 0x123;
        assert_eq!(table_indices(va), [257, 2, 3, 4]);
        assert_eq!(table_indices(0x0000_7FFF_FFFF_F000), [255, 511, 511, 511]);
    }

    #[test]
    fn the_heap_and_the_stacks_each_have_a_kernel_space_slot() {
        let (heap, stacks) = (KERNEL_HEAP_START.as_u64(), KERNEL_STACKS_START.as_u64());
        assert!(is_canonical(heap) && is_canonical(stacks));
        assert_eq!(table_indices(heap), [257, 0, 0, 0]);
        assert_eq!(table_indices(stacks), [258, 0, 0, 0]);
    }

    #[test]
    fn canonical_addresses_repeat_bit_47() {
        assert!(is_canonical(0x0000_7FFF_FFFF_FFFF));
        assert!(!is_canonical(0x0000_8000_0000_0000));
        assert!(is_canonical(KERNEL_SPACE_BASE));
        assert!(!is_canonical(0xFFFF_7FFF_FFFF_FFFF));
    }

    #[test]
    fn translate_follows_the_firmware_identity_map() {
        let tables = PageTables {
            root: FW_ROOT,
            access: firmware(),
        };
        let t = tables.translate(0x5_4321).unwrap();
        assert_eq!((t.phys, t.writable), (0x5_4321, true));
        let t = tables.translate(FW_PD + 8).unwrap();
        assert_eq!((t.phys, t.writable), (FW_PD + 8, false));
        assert!(tables.translate(16 * MIB).is_none());
        assert!(tables.translate(1 << 47).is_none());
    }

    #[test]
    fn translate_handles_1_gib_pages_and_ignores_their_pat_bit() {
        let mut memory = firmware();
        memory.tables.get_mut(&FW_PDPT).unwrap()[1] =
            (1 << 30) | PRESENT | WRITABLE | HUGE | (1 << 12);
        let tables = PageTables {
            root: FW_ROOT,
            access: memory,
        };
        assert_eq!(
            tables.translate((1 << 30) + 0x1234).unwrap().phys,
            (1 << 30) + 0x1234
        );
    }

    #[test]
    fn adopt_copies_the_root_into_a_frame_of_its_own() {
        let mut frames = Frames(vec![0x40_0000]);
        let tables = adopted(&mut frames);
        assert_eq!(tables.root, 0x40_0000);
        assert_eq!(
            tables.access.tables[&0x40_0000],
            tables.access.tables[&FW_ROOT]
        );
        assert_eq!(tables.translate(0x5_4321).unwrap().phys, 0x5_4321);
        // Every write went to the new root (FakeMemory also rejects
        // writes to firmware tables outright).
        assert!(tables.access.writes.iter().all(|&(t, _, _)| t == 0x40_0000));
    }

    #[test]
    fn adopt_refuses_a_root_frame_it_cannot_write() {
        let mut frames = Frames(vec![FW_ROOT + 0x3000]); // in the read-only 2 MiB page
        let err = PageTables::adopt(firmware(), FW_ROOT, &mut frames).err();
        assert_eq!(err, Some(PagingError::TableFrameNotWritable));
    }

    #[test]
    fn adopt_refuses_firmware_tables_that_use_kernel_space() {
        let mut memory = firmware();
        memory.tables.get_mut(&FW_ROOT).unwrap()[300] = FW_PDPT | PRESENT;
        let err = PageTables::adopt(memory, FW_ROOT, &mut Frames(vec![0x40_0000])).err();
        assert_eq!(err, Some(PagingError::KernelSpaceInUse));
    }

    #[test]
    fn map_builds_zeroed_tables_and_a_no_execute_data_leaf() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        tables
            .map(page(0x5000), frame(0x90_0000), DATA, &mut frames)
            .unwrap();

        assert!(frames.0.is_empty(), "PDPT, PD and PT should all be new");
        let t = tables.translate(page(0x5010)).unwrap();
        assert_eq!((t.phys, t.writable), (0x90_0010, true));
        let leaf = tables.access.tables[&0x40_3000][5];
        assert_eq!(leaf, 0x90_0000 | PRESENT | WRITABLE | NO_EXECUTE);
        assert_eq!(leaf & USER, 0);
        for table in [0x40_1000, 0x40_2000, 0x40_3000] {
            let linked = tables.access.tables[&table]
                .iter()
                .filter(|&&e| e != 0)
                .count();
            assert_eq!(linked, 1, "table {table:#x} holds junk");
        }
        assert_eq!(tables.access.flushed, [page(0x5000)]);
    }

    #[test]
    fn tables_are_fully_zeroed_before_they_are_linked() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        tables
            .map(page(0), frame(0x90_0000), DATA, &mut frames)
            .unwrap();
        let writes = &tables.access.writes;
        for table in [0x40_1000u64, 0x40_2000, 0x40_3000] {
            // The write that makes another table point at this one...
            let link = writes
                .iter()
                .position(|&(t, _, v)| t != table && v & ADDRESS_MASK == table)
                .unwrap();
            // ...comes after every one of its entries was zeroed.
            let zeroed: BTreeSet<usize> = writes[..link]
                .iter()
                .filter(|&&(t, _, v)| t == table && v == 0)
                .map(|&(_, index, _)| index)
                .collect();
            assert_eq!(
                zeroed.len(),
                ENTRIES,
                "{table:#x} linked before it was zeroed"
            );
        }
    }

    #[test]
    fn a_neighbouring_page_reuses_the_tables() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000]);
        let mut tables = adopted(&mut frames);
        tables
            .map(page(0), frame(0x90_0000), DATA, &mut frames)
            .unwrap();
        tables
            .map(page(0x1000), frame(0x91_0000), DATA, &mut frames)
            .unwrap();
        assert_eq!(frames.0, [0x40_4000]);
        assert_eq!(tables.translate(page(0x1000)).unwrap().phys, 0x91_0000);
    }

    #[test]
    fn read_only_executable_flags_give_a_bare_present_leaf() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        let flags = PageFlags::kernel(false, true);
        tables
            .map(page(0), frame(0x90_0000), flags, &mut frames)
            .unwrap();
        assert_eq!(tables.access.tables[&0x40_3000][0], 0x90_0000 | PRESENT);
        assert!(!tables.translate(page(0)).unwrap().writable);
    }

    #[test]
    fn mapping_an_already_mapped_page_is_refused_without_allocating() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000]);
        let mut tables = adopted(&mut frames);
        tables
            .map(page(0), frame(0x90_0000), DATA, &mut frames)
            .unwrap();
        let err = tables.map(page(0), frame(0x91_0000), DATA, &mut frames);
        assert_eq!(err, Err(MapError::AlreadyMapped));
        assert_eq!(frames.0, [0x40_4000]);
        assert_eq!(tables.translate(page(0)).unwrap().phys, 0x90_0000);
    }

    #[test]
    fn pages_outside_kernel_space_are_refused_without_writing() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000]);
        let mut tables = adopted(&mut frames);
        let writes_before = tables.access.writes.len();
        let err = tables.map(0x60_0000, frame(0x90_0000), DATA, &mut frames);
        assert_eq!(err, Err(MapError::OutsideKernelSpace));
        assert_eq!(tables.unmap(0x60_0000), Err(UnmapError::OutsideKernelSpace));
        assert_eq!(tables.access.writes.len(), writes_before);
        assert_eq!(frames.0, [0x40_1000]);
    }

    #[test]
    fn running_out_of_frames_leaves_only_complete_tables_behind() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000]);
        let mut tables = adopted(&mut frames);
        let err = tables.map(page(0), frame(0x90_0000), DATA, &mut frames);
        assert_eq!(err, Err(MapError::OutOfFrames));
        // The PDPT that was built is linked and fully zeroed; nothing else.
        assert_eq!(tables.access.tables[&0x40_1000], [0; ENTRIES]);
        assert!(tables.translate(page(0)).is_none());
        // A later attempt with frames to spare reuses it.
        let mut more = Frames(vec![0x40_2000, 0x40_3000]);
        tables
            .map(page(0), frame(0x90_0000), DATA, &mut more)
            .unwrap();
        assert_eq!(tables.translate(page(0)).unwrap().phys, 0x90_0000);
    }

    #[test]
    fn a_table_frame_that_cannot_be_written_is_refused_and_not_linked() {
        let mut frames = Frames(vec![0x40_0000, FW_ROOT + 0x3000]);
        let mut tables = adopted(&mut frames);
        let err = tables.map(page(0), frame(0x90_0000), DATA, &mut frames);
        assert_eq!(err, Err(MapError::TableFrameNotWritable));
        assert_eq!(tables.access.tables[&0x40_0000][256], 0);
    }

    #[test]
    fn unmap_returns_the_frame_and_flushes_the_translation() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        tables
            .map(page(0x7000), frame(0x90_0000), DATA, &mut frames)
            .unwrap();
        assert_eq!(tables.unmap(page(0x7000)), Ok(frame(0x90_0000)));
        assert!(tables.translate(page(0x7000)).is_none());
        assert_eq!(tables.access.flushed, [page(0x7000), page(0x7000)]);
        assert_eq!(tables.unmap(page(0x7000)), Err(UnmapError::NotMapped));
        assert_eq!(tables.unmap(page(1 << 39)), Err(UnmapError::NotMapped));
    }

    #[test]
    fn rebuilding_the_identity_map_leaves_the_null_page_out() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        // 1 GiB: one PDPT, one page directory and one page table.
        let stats = tables.rebuild_identity(&mut frames, GIB, &[]).unwrap();
        assert_eq!(
            stats,
            IdentityMapStats {
                tables: 3,
                limit: GIB,
                executable_pages: 0,
                read_only_pages: 0,
            }
        );

        assert_eq!(tables.translate(0), None, "the null page must be unmapped");
        for addr in [PAGE, 0x1234 + PAGE, LARGE_PAGE, GIB - PAGE, GIB - 1] {
            let seen = tables.translate(addr).expect("identity mapped");
            assert_eq!((seen.phys, seen.writable), (addr, true), "{addr:#x}");
        }
        assert_eq!(tables.translate(GIB), None, "nothing above the limit");
        assert_eq!(tables.access.flushed_all, 1);
        // The firmware's own tables were never written.
        assert!(
            tables
                .access
                .writes
                .iter()
                .all(|&(table, _, _)| table != FW_PDPT && table != FW_PD)
        );
    }

    /// Everything is no-execute but the ranges that hold code, and only the
    /// 2 MiB regions those fall in are split into 4 KiB pages.
    #[test]
    fn only_the_ranges_that_hold_code_stay_executable() {
        let image = PhysRange::new(PhysAddr::new(0x40_0000 + 0x2000), 2 * PAGE);
        let mut frames = Frames(vec![
            0x80_0000, 0x80_1000, 0x80_2000, 0x80_3000, 0x80_4000, 0x80_5000,
        ]);
        let mut tables = adopted(&mut frames);
        let stats = tables
            .rebuild_identity(&mut frames, GIB, &[MappedRange::read_only_code(image)])
            .unwrap();
        // PDPT, page directory, the first 2 MiB, and the 2 MiB with the image.
        assert_eq!((stats.tables, stats.executable_pages), (4, 2));

        let executable = |addr: u64| {
            let mut table = tables.root;
            for (level, index) in table_indices(addr).into_iter().enumerate() {
                let entry = tables.access.read(table, index);
                assert_ne!(entry & PRESENT, 0, "{addr:#x} is not mapped");
                if entry & NO_EXECUTE != 0 {
                    return false;
                }
                if level == 3 || entry & HUGE != 0 {
                    return true;
                }
                table = entry & ADDRESS_MASK;
            }
            unreachable!()
        };
        assert!(executable(image.start.as_u64()) && executable(image.end().as_u64() - 1));
        assert!(
            !executable(image.start.as_u64() - PAGE),
            "the page below the image"
        );
        assert!(
            !executable(image.end().as_u64()),
            "the page above the image"
        );
        assert!(!executable(0x1000), "low memory is data");
        assert!(!executable(0x2000_0000), "a plain 2 MiB page");
    }

    #[test]
    fn rebuilding_keeps_kernel_space_and_drops_the_rest_of_the_lower_half() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000]);
        let mut tables = adopted(&mut frames);
        // Something of the kernel's own in the higher half...
        tables
            .access
            .write(tables.root, 300, 0x50_0000 | PRESENT | WRITABLE);
        // ...and a leftover the firmware had mapped high in the lower half.
        tables
            .access
            .write(tables.root, 200, 0x60_0000 | PRESENT | WRITABLE);

        tables.rebuild_identity(&mut frames, GIB, &[]).unwrap();

        let root = tables.access.tables[&tables.root];
        assert_eq!(root[300], 0x50_0000 | PRESENT | WRITABLE, "kernel space");
        assert_eq!(
            root[200], 0,
            "the lower half is the new map and nothing else"
        );
        assert_ne!(root[0], 0);
    }

    #[test]
    fn a_bad_limit_or_too_few_frames_leaves_the_old_map_alone() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000]);
        let mut tables = adopted(&mut frames);
        let before = tables.access.tables[&tables.root][0];

        assert_eq!(
            tables.rebuild_identity(&mut frames, GIB + PAGE, &[]),
            Err(PagingError::BadIdentityLimit)
        );
        assert_eq!(
            tables.rebuild_identity(&mut frames, 1024 * GIB, &[]),
            Err(PagingError::BadIdentityLimit)
        );
        // One frame short of the PDPT, directory and page table it needs.
        assert_eq!(
            tables.rebuild_identity(&mut frames, GIB, &[]),
            Err(PagingError::OutOfFrames)
        );
        assert_eq!(tables.access.tables[&tables.root][0], before);
        assert_eq!(tables.translate(0x5_4321).unwrap().phys, 0x5_4321);
        assert_eq!(tables.access.flushed_all, 0);
    }

    #[test]
    fn executable_ranges_must_fit_entirely_inside_the_new_map() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        let outside = PhysRange::new(PhysAddr::new(GIB - PAGE), 2 * PAGE);
        assert_eq!(
            tables.rebuild_identity(&mut frames, GIB, &[MappedRange::read_only_code(outside)]),
            Err(PagingError::RequiredRangeOutsideIdentityMap)
        );
        assert_eq!(tables.access.flushed_all, 0);
    }

    /// Write xor execute: a page that holds nothing but code is mapped
    /// executable and read-only; one that mixes code and data has to stay
    /// writable, and then it is not read-only even though it runs.
    #[test]
    fn pages_that_are_only_code_are_mapped_read_only() {
        let aligned = PhysRange::new(PhysAddr::new(0x40_0000), 2 * PAGE);
        let straddling = PhysRange::new(PhysAddr::new(0x60_0800), PAGE);
        let mut frames = Frames(vec![
            0x80_0000, 0x80_1000, 0x80_2000, 0x80_3000, 0x80_4000, 0x80_5000, 0x80_6000,
        ]);
        let mut tables = adopted(&mut frames);
        let stats = tables
            .rebuild_identity(
                &mut frames,
                GIB,
                &[
                    MappedRange::read_only_code(aligned),
                    MappedRange::read_only_code(straddling),
                ],
            )
            .unwrap();
        // Two whole pages of code, plus the two the straddling range
        // touches.
        assert_eq!((stats.executable_pages, stats.read_only_pages), (4, 2));

        let leaf = |addr: u64| leaf_entry(&tables, addr);
        for addr in [aligned.start.as_u64(), aligned.start.as_u64() + PAGE] {
            let entry = leaf(addr);
            assert_eq!(entry & NO_EXECUTE, 0, "{addr:#x} must run");
            assert_eq!(entry & WRITABLE, 0, "{addr:#x} must not be writable");
        }
        for addr in [straddling.start.as_u64(), straddling.end().as_u64() - 1] {
            let entry = leaf(addr);
            assert_eq!(entry & NO_EXECUTE, 0, "{addr:#x} must run");
            assert_ne!(
                entry & WRITABLE,
                0,
                "{addr:#x} shares its page with data and must stay writable"
            );
        }
        // Plain data is writable and no-execute, as before.
        let data = leaf(0x10_0000);
        assert_ne!(data & WRITABLE, 0);
        assert_ne!(data & NO_EXECUTE, 0);
    }

    /// OVMF writes inside its own runtime services code, so a range the
    /// kernel does not control keeps its pages writable even though they
    /// run. Mapping them read-only faults on `shutdown` (measured).
    /// The window covers every byte of `[0, limit)`, writable and never
    /// executable, and lands only on its own PML4 slot.
    #[test]
    fn the_physical_window_reaches_all_of_memory_and_runs_none_of_it() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        let base = KERNEL_SPACE_BASE + 4 * (1 << 39);
        // One PDPT plus one page directory per GiB.
        assert_eq!(
            tables.map_physical_window(&mut frames, base, 2 * GIB),
            Ok(3)
        );
        for physical in [0, PAGE, LARGE_PAGE, GIB, 2 * GIB - PAGE] {
            let seen = tables
                .translate(base + physical)
                .unwrap_or_else(|| panic!("{physical:#x} is not in the window"));
            assert_eq!((seen.phys, seen.writable), (physical, true));
            let entry = leaf_entry(&tables, base + physical);
            assert_ne!(entry & NO_EXECUTE, 0, "the window must never run code");
            assert_ne!(entry & HUGE, 0, "2 MiB pages, not one table per 4 KiB");
        }
        // Nothing past the limit, and the kernel's other slots untouched.
        assert_eq!(tables.translate(base + 2 * GIB), None);
        assert_eq!(tables.translate(KERNEL_SPACE_BASE), None);
    }

    /// What the kernel does once it needs nothing down there: the lower
    /// half keeps the firmware's code and loses everything else.
    #[test]
    fn keeping_only_the_firmware_empties_the_rest_of_the_lower_half() {
        let mut frames = Frames(vec![
            0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000, 0x40_5000,
        ]);
        let mut tables = adopted(&mut frames);
        // A range that straddles two pages, to check both are kept.
        let firmware = PhysRange::new(PhysAddr::new(0x60_0FF0), 0x20);
        // What that code keeps between calls: mapped too, and never
        // executable.
        let data = PhysRange::new(PhysAddr::new(0x70_0000), PAGE);
        let kept = tables
            .keep_only(
                &mut frames,
                &[
                    MappedRange::writable_code(firmware, CachePolicy::WriteBack),
                    MappedRange::data(data, CachePolicy::WriteBack),
                ],
            )
            .expect("keeping the firmware");
        // A PDPT, one page directory and one page table: 6 MiB and
        // 7 MiB share the same 2 MiB slot.
        assert_eq!(kept, 3);

        for addr in [0x60_0000, 0x60_1000] {
            let seen = tables
                .translate(addr)
                .unwrap_or_else(|| panic!("{addr:#x} must stay mapped"));
            assert_eq!((seen.phys, seen.writable), (addr, true));
            assert_eq!(
                leaf_entry(&tables, addr) & NO_EXECUTE,
                0,
                "the firmware runs from here"
            );
        }
        let entry = leaf_entry(&tables, data.start.as_u64());
        assert_ne!(entry & PRESENT, 0, "the firmware's data stays mapped");
        assert_ne!(entry & WRITABLE, 0, "and writable");
        assert_ne!(entry & NO_EXECUTE, 0, "but nothing runs from it");

        // Everything else in the lower half is gone, including what the
        // firmware's own map had.
        for addr in [0x0, PAGE, 0x10_0000, 0x60_2000, 4 * MIB, GIB] {
            assert_eq!(tables.translate(addr), None, "{addr:#x} must be unmapped");
        }
    }

    /// Memory-mapped I/O the firmware talks to has to keep the caching
    /// it was reported with: write-back there would corrupt the device
    /// behind it.
    #[test]
    fn what_stays_mapped_keeps_the_caching_it_was_reported_with() {
        let mut frames = Frames(vec![
            0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000, 0x40_5000,
        ]);
        let mut tables = adopted(&mut frames);
        let mmio = PhysRange::new(PhysAddr::new(0x60_0000), PAGE);
        let ram = PhysRange::new(PhysAddr::new(0x60_1000), PAGE);
        tables
            .keep_only(
                &mut frames,
                &[
                    MappedRange::data(mmio, CachePolicy::Uncacheable),
                    MappedRange::data(ram, CachePolicy::WriteBack),
                ],
            )
            .expect("keeping both");

        let device = leaf_entry(&tables, mmio.start.as_u64());
        assert_ne!(device & CACHE_DISABLE, 0, "the device must not be cached");
        assert_ne!(device & WRITE_THROUGH, 0);
        let memory = leaf_entry(&tables, ram.start.as_u64());
        assert_eq!(
            memory & (CACHE_DISABLE | WRITE_THROUGH),
            0,
            "RAM is write-back"
        );

        // Write-combining is not available until the PAT is reprogrammed,
        // so it is mapped uncacheable: slower, never wrong.
        assert_eq!(
            cache_bits(CachePolicy::WriteCombining),
            cache_bits(CachePolicy::Uncacheable)
        );
        assert_eq!(cache_bits(CachePolicy::WriteThrough), WRITE_THROUGH);
        assert_eq!(cache_bits(CachePolicy::Unspecified), 0);
    }

    #[test]
    fn keeping_a_range_that_is_not_in_the_lower_half_is_refused() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000]);
        let mut tables = adopted(&mut frames);
        let too_high = PhysRange::new(PhysAddr::new(512 * GIB), PAGE);
        assert_eq!(
            tables.keep_only(
                &mut frames,
                &[MappedRange::writable_code(too_high, CachePolicy::WriteBack)]
            ),
            Err(PagingError::RequiredRangeOutsideIdentityMap)
        );
    }

    #[test]
    fn the_physical_window_refuses_a_place_that_is_not_its_own() {
        let mut frames = Frames(vec![
            0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000, 0x40_5000,
        ]);
        let mut tables = adopted(&mut frames);
        // In the lower half, which is user space.
        assert_eq!(
            tables.map_physical_window(&mut frames, 0x4000_0000, GIB),
            Err(PagingError::WindowOutsideKernelSpace)
        );
        // Not at the start of a PML4 slot.
        assert_eq!(
            tables.map_physical_window(&mut frames, KERNEL_SPACE_BASE + GIB, GIB),
            Err(PagingError::WindowOutsideKernelSpace)
        );
        // A limit that is not whole GiB.
        assert_eq!(
            tables.map_physical_window(&mut frames, KERNEL_SPACE_BASE, GIB + PAGE),
            Err(PagingError::BadIdentityLimit)
        );
        // And the slot cannot be taken twice.
        let base = KERNEL_SPACE_BASE + 4 * (1 << 39);
        assert_eq!(tables.map_physical_window(&mut frames, base, GIB), Ok(2));
        assert_eq!(
            tables.map_physical_window(&mut frames, base, GIB),
            Err(PagingError::WindowSlotTaken)
        );
    }

    #[test]
    fn code_someone_else_writes_into_stays_writable() {
        let firmware = PhysRange::new(PhysAddr::new(0x40_0000), 2 * PAGE);
        let mut frames = Frames(vec![
            0x80_0000, 0x80_1000, 0x80_2000, 0x80_3000, 0x80_4000, 0x80_5000,
        ]);
        let mut tables = adopted(&mut frames);
        let stats = tables
            .rebuild_identity(
                &mut frames,
                GIB,
                &[MappedRange::writable_code(firmware, CachePolicy::WriteBack)],
            )
            .unwrap();
        assert_eq!((stats.executable_pages, stats.read_only_pages), (2, 0));
        for addr in [firmware.start.as_u64(), firmware.end().as_u64() - PAGE] {
            let entry = leaf_entry(&tables, addr);
            assert_eq!(entry & NO_EXECUTE, 0, "{addr:#x} must run");
            assert_ne!(entry & WRITABLE, 0, "{addr:#x} must stay writable");
        }
    }

    /// A page for ring 3 carries the user bit all the way up the walk —
    /// the CPU ands them — and the kernel's pages never carry it.
    #[test]
    fn user_pages_are_reachable_from_ring_three_and_kernel_pages_are_not() {
        let mut frames = Frames(vec![
            0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000, 0x40_4000, 0x40_5000, 0x40_6000, 0x40_7000,
            0x40_8000,
        ]);
        let mut tables = adopted(&mut frames);
        // The lower half has to be the kernel's before anything can be
        // mapped into it: the firmware's tables are read-only, which is
        // exactly what `keep_only` leaves behind (Incremento 17).
        // The kernel reaches tables through its window from here on,
        // exactly as it does after Incremento 17.
        let window = KERNEL_SPACE_BASE + 4 * (1 << 39);
        tables
            .map_physical_window(&mut frames, window, GIB)
            .expect("the window");
        tables.access.window = window;
        tables.keep_only(&mut frames, &[]).expect("emptying it");
        let user = 0x0000_0000_2000_0000;
        tables
            .map(
                user,
                frame(0x50_0000),
                PageFlags::user(true, true),
                &mut frames,
            )
            .expect("mapping a page for the program");

        let mut table = tables.root;
        for (level, index) in table_indices(user).into_iter().enumerate() {
            let entry = tables.access.read(table, index);
            assert_ne!(entry & USER, 0, "level {level} must allow ring 3");
            if level == 3 {
                assert_eq!(entry & NO_EXECUTE, 0, "the program has to run");
                break;
            }
            table = entry & ADDRESS_MASK;
        }

        // A page of the kernel's, mapped the same way, stays out of
        // reach from ring 3.
        tables
            .map(
                page(0),
                frame(0x51_0000),
                PageFlags::kernel(true, false),
                &mut frames,
            )
            .expect("mapping a page of the kernel's");
        assert_eq!(leaf_entry(&tables, page(0)) & USER, 0);
    }

    #[test]
    fn a_page_is_either_the_kernel_s_or_a_program_s_and_says_which() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        // Refused before any table is even looked at, so no `keep_only`
        // is needed here.
        // The kernel cannot take a page of user space...
        assert_eq!(
            tables.map(
                0x2000_0000,
                frame(0x50_0000),
                PageFlags::kernel(true, false),
                &mut frames
            ),
            Err(MapError::OutsideKernelSpace)
        );
        // ...and a program cannot be handed one of the kernel's.
        assert_eq!(
            tables.map(
                page(0),
                frame(0x50_0000),
                PageFlags::user(true, false),
                &mut frames
            ),
            Err(MapError::OutsideKernelSpace)
        );
    }

    #[test]
    fn translate_reports_the_page_that_resolved_it() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000, 0x40_3000]);
        let mut tables = adopted(&mut frames);
        // Inside the firmware's 2 MiB pages, a walk that lands anywhere
        // answers for the whole page, so a range check can skip to its end.
        let inside = 5 * MIB + 0x123;
        let large = tables.translate(inside).unwrap();
        assert_eq!(
            (large.page_size, large.phys, large.writable),
            (LARGE_PAGE, inside, true)
        );
        // From a frame-aligned address the remaining run is a whole
        // number of frames, which is how the kernel asks.
        let aligned = 5 * MIB;
        let run = large.page_size - (aligned & (large.page_size - 1));
        assert_eq!(run, MIB);
        assert_eq!(run % PAGE, 0);
        // A 4 KiB mapping answers for 4 KiB only.
        tables
            .map(page(0), frame(0x50_0000), DATA, &mut frames)
            .unwrap();
        assert_eq!(tables.translate(page(0)).unwrap().page_size, PAGE);
    }

    #[test]
    fn large_pages_in_kernel_space_are_not_split() {
        let mut frames = Frames(vec![0x40_0000, 0x40_1000, 0x40_2000]);
        let mut tables = adopted(&mut frames);
        tables.access.tables.insert(0x40_1000, [0; ENTRIES]);
        tables.access.tables.get_mut(&0x40_1000).unwrap()[0] =
            0x4000_0000 | PRESENT | WRITABLE | HUGE;
        tables.access.tables.get_mut(&0x40_0000).unwrap()[256] = 0x40_1000 | PRESENT | WRITABLE;
        let err = tables.map(page(0), frame(0x90_0000), DATA, &mut frames);
        assert_eq!(err, Err(MapError::HugePageInTheWay));
        assert_eq!(tables.unmap(page(0)), Err(UnmapError::HugePageInTheWay));
        assert_eq!(tables.translate(page(0x1234)).unwrap().phys, 0x4000_1234);
    }
}
