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
//! physical memory. Only `IdentityAccess` and the register accessors touch
//! the hardware.

use core::arch::asm;

use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::frame::{FrameAllocator, PhysFrame, PhysRange};
use harlan_hal::paging::{MapError, Page, PageFlags, PageMapper, UnmapError};

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

const ENTRIES: usize = 512;
const PAGE: u64 = 4096;
const LARGE_PAGE: u64 = 2 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// How much physical address space `rebuild_identity_map` covers by
/// default: enough for the RAM the frame allocator can manage, the
/// framebuffer and legacy MMIO, on every machine this kernel has run on.
pub const DEFAULT_IDENTITY_LIMIT: u64 = 4 * GIB;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
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
}

/// How the walker reaches table memory, given a table's physical address.
trait TableAccess {
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
        if page < KERNEL_SPACE_BASE {
            return Err(MapError::OutsideKernelSpace);
        }
        let indices = table_indices(page);
        let mut table = self.root;
        for &index in &indices[..3] {
            let entry = self.access.read(table, index);
            table = if entry & PRESENT == 0 {
                let new = self.new_table(frames)?;
                // Linked only once fully zeroed: the hardware walker never
                // sees a half-built table.
                self.access.write(table, index, new | PRESENT | WRITABLE);
                new
            } else if entry & HUGE != 0 {
                return Err(MapError::HugePageInTheWay);
            } else {
                entry & ADDRESS_MASK
            };
        }
        if self.access.read(table, indices[3]) & PRESENT != 0 {
            return Err(MapError::AlreadyMapped);
        }
        let mut leaf = frame.start_address().as_u64() | PRESENT;
        if flags.writable {
            leaf |= WRITABLE;
        }
        if !flags.executable {
            leaf |= NO_EXECUTE;
        }
        self.access.write(table, indices[3], leaf);
        self.access.flush(page);
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
        executable: &[PhysRange],
    ) -> Result<IdentityMapStats, PagingError> {
        if limit == 0 || !limit.is_multiple_of(GIB) || limit > 512 * GIB {
            return Err(PagingError::BadIdentityLimit);
        }
        // Marking a range executable it cannot reach would build a map
        // without the code the caller says it still runs.
        if executable.iter().any(|range| {
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
        let (mut tables, mut executable_pages) = (1, 0);
        for slot in 0..(limit / GIB) as usize {
            let directory = self.new_table(frames).map_err(to_paging)?;
            tables += 1;
            for entry in 0..ENTRIES {
                let base = slot as u64 * GIB + entry as u64 * LARGE_PAGE;
                // A 2 MiB page will do wherever the whole range is data.
                // The first 2 MiB (the null page has to be left out) and
                // anything holding code need 4 KiB granularity.
                let fine_grained = base == 0
                    || executable.iter().any(|range| {
                        range.overlaps(PhysAddr::new(base), PhysAddr::new(base + LARGE_PAGE))
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
                    let runs_code = executable.iter().any(|range| {
                        range.overlaps(PhysAddr::new(page), PhysAddr::new(page + PAGE))
                    });
                    executable_pages += u64::from(runs_code);
                    let flags = if runs_code {
                        PRESENT | WRITABLE
                    } else {
                        PRESENT | WRITABLE | NO_EXECUTE
                    };
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
        })
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
        match self.translate(frame) {
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
struct IdentityAccess;

impl TableAccess for IdentityAccess {
    fn read(&self, table: u64, index: usize) -> u64 {
        // SAFETY: `table` is the physical address of a page table, valid as
        // a virtual address under the identity map; `index < 512` keeps the
        // 8-byte-aligned read inside that 4 KiB page. Volatile because the
        // CPU's page walker also reads (and sets accessed bits in) this
        // memory behind the compiler's back.
        unsafe { core::ptr::read_volatile((table as *const u64).add(index)) }
    }

    fn write(&mut self, table: u64, index: usize, value: u64) {
        // SAFETY: as for `read`, and `table` is one of the kernel's own
        // tables, mapped writable (see the type's documentation), which no
        // Rust reference points into.
        unsafe { core::ptr::write_volatile((table as *mut u64).add(index), value) }
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
    tables: PageTables<IdentityAccess>,
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
            access: IdentityAccess,
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
            access: IdentityAccess,
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

        let tables = PageTables::adopt(IdentityAccess, firmware.root, frames)?;
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
        executable: &[PhysRange],
    ) -> Result<IdentityMapStats, PagingError> {
        self.tables.rebuild_identity(frames, limit, executable)
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
        tables: BTreeMap<u64, [u64; ENTRIES]>,
        firmware: BTreeSet<u64>,
        writes: Vec<(u64, usize, u64)>,
        flushed: Vec<u64>,
        flushed_all: u32,
    }

    impl TableAccess for FakeMemory {
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
    const DATA: PageFlags = PageFlags {
        writable: true,
        executable: false,
    };

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
        let flags = PageFlags {
            writable: false,
            executable: true,
        };
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
            .rebuild_identity(&mut frames, GIB, core::slice::from_ref(&image))
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
            tables.rebuild_identity(&mut frames, GIB, &[outside]),
            Err(PagingError::RequiredRangeOutsideIdentityMap)
        );
        assert_eq!(tables.access.flushed_all, 0);
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
