//! A process: its own address space, its memory, and the program in it.
//!
//! Until now the program ran inside the kernel's tables, separated by one
//! bit per page (Incremento 18). That keeps it out of the kernel and does
//! nothing about a second program. A space of its own is what makes two
//! processes strangers to each other
//! (docs/adr/0017-fase3-process-address-space.md).
//!
//! No scheduler yet: the kernel starts one, it leaves through `exit`, and
//! the kernel goes back to its own space and carries on.

use harlan_arch_x86_64::paging::AddressSpace;
use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::fat::DirectoryEntry;
use harlan_hal::frame::{PhysFrame, PhysRange};
use harlan_hal::info;
use harlan_hal::paging::{PAGE_SIZE, Page, PageFlags};

use crate::memory::frame_allocator::FramePurpose;
use crate::memory::stacks::{self, Stack};
use crate::memory::zeroed_frames::KernelFrames;

/// Where a program's code goes, in every process: they do not share a
/// space, so they can share addresses.
pub const CODE_BASE: VirtAddr = VirtAddr::new(0x0040_0000);
/// And its stack, a megabyte above.
pub const STACK_BASE: VirtAddr = VirtAddr::new(0x0050_0000);
pub const STACK_TOP: VirtAddr = VirtAddr::new(0x0050_1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// No space left for the tables or the memory.
    OutOfMemory,
    /// The space could not be created.
    Space(harlan_arch_x86_64::paging::PagingError),
    /// Its memory could not be mapped.
    Mapping(harlan_hal::paging::MapError),
    /// The program does not fit in the page it is given.
    ProgramTooBig,
    /// More ranges of memory than a process may own.
    TooManyRanges,
    /// More frames than a process may hold.
    TooManyFrames,
    /// Two segments of one program fall in the same page, so the kernel
    /// would have to guess which permissions it should have. It will not
    /// (docs/adr/0026-fase4-elf-user-programs.md, point 3).
    SegmentsShareAPage { page: u64 },
}

/// How many ranges of memory a process may own: one per segment of its
/// program, and its stack.
pub const MAX_RANGES: usize = harlan_hal::elf::MAX_SEGMENTS + 1;
/// And how many frames behind them. A program of a few pages and a page of
/// stack; the limit is here so that a program too big is an error with a
/// name rather than an allocation in the middle of a load.
pub const MAX_FRAMES: usize = 32;

/// What the kernel knows about a running program.
pub struct Process {
    /// Its tables. The kernel's half is in here too, shared.
    pub space: AddressSpace,
    /// What it was given, to check the pointers it hands the kernel
    /// against (ADR 0014, point 9). A flat program owns two ranges, its
    /// code and its stack; an ELF owns one per segment and its stack.
    pub owned: [Option<PhysRange>; MAX_RANGES],
    /// Which of `owned` the program may write, which is not the same
    /// question (ADR 0028, point 9).
    ///
    /// A syscall that **fills** a buffer has to ask this one. A page that
    /// is read-only for the user is still writable by the kernel unless
    /// CR0.WP says otherwise — and it does say otherwise, which turns a
    /// program handing over the address of its own code into a page fault
    /// in ring 0: a bad argument becoming a kernel panic. The fix is to
    /// refuse the argument, which needs the kernel to know.
    pub writable: [Option<PhysRange>; MAX_RANGES],
    /// The frames behind that memory, and what each was taken for, so
    /// that they can be given back when it exits.
    pub frames: [Option<(PhysFrame, FramePurpose)>; MAX_FRAMES],
    /// Where the CPU starts it. A flat program starts at the beginning of
    /// its page; an ELF says where (ADR 0026).
    pub entry: VirtAddr,
    /// Where it enters the kernel: its own stack, with guard pages
    /// (docs/adr/0018-fase3-context-switch.md).
    pub kernel_stack: Stack,
    /// The files it has open (docs/adr/0028-fase4-file-abi-v0.md).
    ///
    /// The third resource a process owns, and the first that is not
    /// memory. Its own type, so that what a descriptor is can be tested
    /// without a process — which needs page tables, which need a machine.
    pub open: OpenFiles,
}

/// How many files one process may hold open at a time (ADR 0028, point 5).
///
/// Four, and fixed, so the table lives inside `Process` and opening a file
/// allocates nothing. A program that needs a fifth in Fase 4 is doing
/// something this phase does not have to support.
pub const MAX_OPEN: usize = 4;

/// A file a process has open, and how far through it that process is.
///
/// The directory entry as it was when the file was opened, not a pointer
/// into the volume: enough to make the next read from nothing, so the
/// kernel never lends a process anything that belongs to the filesystem
/// (ADR 0028, point 6).
#[derive(Clone, Copy, Debug)]
pub struct OpenFile {
    pub entry: DirectoryEntry,
    /// Where the next `read` starts. Only ever moved forward, by exactly
    /// what a read returned: there is no `seek` in v0, so a descriptor
    /// cannot be pointed anywhere a read has not already reached.
    pub position: u32,
}

/// What one process has open: a fixed table, where a descriptor is an
/// index (ADR 0028, points 4 and 5).
///
/// Fixed and small so that opening a file allocates nothing, and an index
/// rather than a pointer or a global number so that a program cannot
/// invent a valid descriptor or name another process's: the numbers mean
/// nothing outside this table.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenFiles([Option<OpenFile>; MAX_OPEN]);

impl OpenFiles {
    pub const fn new() -> Self {
        Self([None; MAX_OPEN])
    }

    /// Puts `file` in the first free slot and answers its descriptor.
    ///
    /// `None` when all four are taken, which a program sees as
    /// `ERR_TOO_MANY_OPEN`. The **first free** slot rather than the next
    /// number: a descriptor that was closed is handed out again, so a
    /// program that opens and closes in a loop does not run out.
    pub fn open(&mut self, file: OpenFile) -> Option<usize> {
        let at = self.0.iter().position(Option::is_none)?;
        self.0[at] = Some(file);
        Some(at)
    }

    /// The file a descriptor names, if it is open.
    ///
    /// A number from ring 3 is an index into this table and nothing else:
    /// out of range and closed are the same answer, which is why a program
    /// learns nothing by guessing.
    pub fn at(&mut self, descriptor: usize) -> Option<&mut OpenFile> {
        self.0.get_mut(descriptor)?.as_mut()
    }

    /// Closes a descriptor, answering whether it was open.
    pub fn close(&mut self, descriptor: usize) -> bool {
        match self.0.get_mut(descriptor) {
            Some(slot) => slot.take().is_some(),
            None => false,
        }
    }

    /// Whether this table holds that name.
    ///
    /// Compared without case, as every other name comparison is: a short
    /// name is stored upper case and nobody types it that way.
    pub fn holds(&self, name: &str) -> bool {
        self.0
            .iter()
            .flatten()
            .any(|file| file.entry.is_named(name))
    }

    /// Closes everything.
    ///
    /// Nothing is written: a descriptor holds no unwritten bytes, because
    /// writing is whole-file and finishes inside the one syscall that
    /// started it (ADR 0028, point 1).
    pub fn close_everything(&mut self) {
        self.0 = [None; MAX_OPEN];
    }

    /// How many are open. For the log, and for a test that wants to say
    /// what it expects without reaching inside.
    pub fn count(&self) -> usize {
        self.0.iter().flatten().count()
    }
}

/// What a process is being built out of, while it is being built.
///
/// Kept apart from `Process` so that the bookkeeping — what has been
/// taken, what is owned — is written once and used by both ways of
/// starting one.
struct Building {
    space: AddressSpace,
    owned: [Option<PhysRange>; MAX_RANGES],
    writable: [Option<PhysRange>; MAX_RANGES],
    frames: [Option<(PhysFrame, FramePurpose)>; MAX_FRAMES],
    ranges: usize,
    taken: usize,
}

impl Building {
    fn new(space: AddressSpace) -> Self {
        Self {
            space,
            owned: [None; MAX_RANGES],
            writable: [None; MAX_RANGES],
            frames: [None; MAX_FRAMES],
            ranges: 0,
            taken: 0,
        }
    }

    /// Records a range as this process's, and whether it may write it.
    ///
    /// Both at once, from one call, so that there is no way to add a range
    /// and forget to say which it is — the two arrays cannot drift apart
    /// because nothing else writes them.
    fn own(&mut self, range: PhysRange, writable: bool) -> Result<(), SpawnError> {
        if self.ranges == MAX_RANGES {
            return Err(SpawnError::TooManyRanges);
        }
        self.owned[self.ranges] = Some(range);
        if writable {
            self.writable[self.ranges] = Some(range);
        }
        self.ranges += 1;
        Ok(())
    }

    fn took(&mut self, frame: PhysFrame, purpose: FramePurpose) -> Result<(), SpawnError> {
        if self.taken == MAX_FRAMES {
            return Err(SpawnError::TooManyFrames);
        }
        self.frames[self.taken] = Some((frame, purpose));
        self.taken += 1;
        Ok(())
    }

    /// Gives back every frame taken so far. For a process that will not
    /// exist: its page tables go with its space, which nothing has
    /// activated, and the allocator ends where it started.
    fn give_back(&mut self, frames: &mut KernelFrames<'_>) {
        for (frame, purpose) in self.frames.iter().flatten() {
            let _ = frames.deallocate_as(*frame, *purpose);
        }
        self.frames = [None; MAX_FRAMES];
        self.taken = 0;
    }

    fn finish(self, entry: VirtAddr, kernel_stack: Stack) -> Process {
        Process {
            space: self.space,
            owned: self.owned,
            writable: self.writable,
            frames: self.frames,
            entry,
            kernel_stack,
            open: OpenFiles::new(),
        }
    }
}

/// The same ranges in a fixed array, with the empty slots as ranges of
/// no bytes — which `owned_by` can never match, because it refuses a
/// length of zero.
fn fixed(ranges: &[Option<PhysRange>; MAX_RANGES]) -> [PhysRange; MAX_RANGES] {
    let mut out = [PhysRange::new(PhysAddr::new(0), 0); MAX_RANGES];
    for (at, range) in ranges.iter().enumerate() {
        if let Some(range) = range {
            out[at] = *range;
        }
    }
    out
}

/// Whether `[ptr, ptr + len)` falls inside one of `ranges`.
///
/// Every pointer that arrives from ring 3 goes through here before the
/// kernel reads a byte of it. A free function, so that it can be tested
/// without a process — which needs page tables to exist.
pub fn owned_by(ranges: &[PhysRange], ptr: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = ptr.checked_add(len) else {
        return false;
    };
    ranges
        .iter()
        .any(|range| ptr >= range.start.as_u64() && end <= range.end().as_u64())
}

impl Process {
    /// Whether `[ptr, ptr + len)` is memory this process owns.
    pub fn owns(&self, ptr: u64, len: u64) -> bool {
        self.ranges()
            .iter()
            .any(|range| owned_by(&[*range], ptr, len))
    }

    /// The ranges it owns, in a fixed array so that the syscall handler
    /// can copy them out without borrowing the scheduler.
    pub fn ranges(&self) -> [PhysRange; MAX_RANGES] {
        fixed(&self.owned)
    }

    /// The ranges it owns **and** may write, in the same shape.
    pub fn writable_ranges(&self) -> [PhysRange; MAX_RANGES] {
        fixed(&self.writable)
    }

    /// Puts `file` in the first free slot and answers its descriptor.
    pub fn open_file(&mut self, file: OpenFile) -> Option<usize> {
        self.open.open(file)
    }

    /// The file a descriptor names, if this process opened it and has not
    /// closed it.
    pub fn open_at(&mut self, descriptor: usize) -> Option<&mut OpenFile> {
        self.open.at(descriptor)
    }

    /// Closes a descriptor, answering whether it was open.
    pub fn close_file(&mut self, descriptor: usize) -> bool {
        self.open.close(descriptor)
    }

    /// Whether this process has that name open.
    pub fn has_open(&self, name: &str) -> bool {
        self.open.holds(name)
    }

    /// Closes everything this process had open.
    ///
    /// Called when it ends, like giving its frames back
    /// (docs/adr/0021-fase3-reclaiming-a-dead-space.md): a process that
    /// has ended keeps nothing.
    pub fn close_everything(&mut self) {
        self.open.close_everything();
    }

    /// Where the CPU starts it.
    pub fn entry(&self) -> VirtAddr {
        self.entry
    }

    pub fn stack_top(&self) -> VirtAddr {
        STACK_TOP
    }

    /// Makes this process's space the one the CPU walks.
    ///
    /// # Safety
    ///
    /// The caller must be running in the higher half — which every space
    /// shares — and must not be holding a pointer into the lower half of
    /// the space it is leaving.
    pub unsafe fn activate(&self) {
        // SAFETY: forwarded from this function's contract.
        unsafe { self.space.activate() };
    }
}

/// Builds a process around `program`: a space of its own, a page of code
/// with the program copied in, and a page of stack.
///
/// # Safety
///
/// The kernel must own its tables and reach frames through its own
/// window. Nothing else may be using the frames this takes.
pub unsafe fn spawn(
    mapper: &mut harlan_arch_x86_64::paging::KernelPageTable,
    frames: &mut KernelFrames<'_>,
    program: &[u8],
    kernel_stack: Stack,
) -> Result<Process, SpawnError> {
    if program.len() > PAGE_SIZE as usize {
        return Err(SpawnError::ProgramTooBig);
    }
    // SAFETY: forwarded from this function's contract.
    let mut space = unsafe { mapper.new_address_space(frames) }.map_err(SpawnError::Space)?;

    let code_frame = frames
        .allocate_for(FramePurpose::Kernel)
        .ok_or(SpawnError::OutOfMemory)?;
    let stack_frame = frames
        .allocate_for(FramePurpose::Stack)
        .ok_or(SpawnError::OutOfMemory)?;

    // The program goes in through the kernel's window onto physical
    // memory, not through the process's mapping, which is read-only.
    let window = frames.window();
    // SAFETY: the frame is fresh from the allocator, so nothing else uses
    // it, and the window reaches it (its own contract).
    unsafe {
        core::ptr::copy_nonoverlapping(
            program.as_ptr(),
            window.frame_ptr(code_frame),
            program.len(),
        )
    };

    // SAFETY: both frames are this process's alone, and its space is
    // empty below the kernel's half.
    unsafe {
        space
            .map(
                Page::containing_address(CODE_BASE),
                code_frame,
                PageFlags::user(false, true),
                frames,
            )
            .map_err(SpawnError::Mapping)?;
        space
            .map(
                Page::containing_address(STACK_BASE),
                stack_frame,
                PageFlags::user(true, false),
                frames,
            )
            .map_err(SpawnError::Mapping)?;
    }

    info!(
        "HARLAN: a process with its own space at {:#x}: {} byte(s) of code at {:#x}, a stack at {:#x}",
        space.root(),
        program.len(),
        CODE_BASE,
        STACK_BASE
    );
    let mut building = Building::new(space);
    building.took(code_frame, FramePurpose::Kernel)?;
    building.took(stack_frame, FramePurpose::Stack)?;
    // Its code is not writable by it — that is what W^X means for a
    // program (ADR 0008) — and its stack is.
    building.own(
        PhysRange::new(PhysAddr::new(CODE_BASE.as_u64()), PAGE_SIZE),
        false,
    )?;
    building.own(
        PhysRange::new(PhysAddr::new(STACK_BASE.as_u64()), PAGE_SIZE),
        true,
    )?;
    Ok(building.finish(CODE_BASE, kernel_stack))
}

/// Builds a process out of an ELF: a space of its own, one mapping per
/// segment with the permissions the file asks for, and a page of stack.
///
/// The segments are mapped exactly as the file describes them — a segment
/// that wanted to be written and executed was refused before this was
/// called (ADR 0026, point 3) — and the memory a segment claims beyond
/// what the file carries is zeroed, because that is its `.bss` and a
/// program that found rubbish there would fail somewhere else entirely.
///
/// # Safety
///
/// As `spawn`.
pub unsafe fn spawn_elf(
    mapper: &mut harlan_arch_x86_64::paging::KernelPageTable,
    frames: &mut KernelFrames<'_>,
    program: &harlan_hal::elf::Program,
    file: &[u8],
    kernel_stack: Stack,
) -> Result<Process, SpawnError> {
    // SAFETY: forwarded from this function's contract.
    let space = unsafe { mapper.new_address_space(frames) }.map_err(SpawnError::Space)?;
    let mut building = Building::new(space);
    // A load that stops half way has still taken frames. Giving them back
    // is what keeps the boot's own count — free before any process existed,
    // free again once they are all gone — meaning what it says.
    // SAFETY: as this function's contract.
    if let Err(err) = unsafe { load(&mut building, program, file, frames) } {
        building.give_back(frames);
        return Err(err);
    }

    info!(
        "HARLAN: a process from an ELF: space at {:#x}, {} segment(s), entry {:#x}, a stack at {:#x}",
        building.space.root(),
        program.segment_count(),
        program.entry,
        STACK_BASE
    );
    Ok(building.finish(program.entry, kernel_stack))
}

/// Maps a program's segments and its stack into `building`.
///
/// Separate from `spawn_elf` so that there is exactly one place that knows
/// what to do when any of this fails: everything taken goes back.
///
/// # Safety
///
/// As `spawn_elf`.
unsafe fn load(
    building: &mut Building,
    program: &harlan_hal::elf::Program,
    file: &[u8],
    frames: &mut KernelFrames<'_>,
) -> Result<(), SpawnError> {
    for segment in program.segments() {
        let first = segment.at.as_u64() & !(PAGE_SIZE - 1);
        let last = (segment.end() - 1) & !(PAGE_SIZE - 1);
        let mut page_at = first;
        while page_at <= last {
            let frame = frames
                .allocate_for(FramePurpose::Kernel)
                .ok_or(SpawnError::OutOfMemory)?;
            building.took(frame, FramePurpose::Kernel)?;

            // What of this page the file carries. The frame arrives
            // zeroed, so the rest of it — the `.bss` — is already right.
            let window = frames.window();
            let page_start = page_at;
            let into_segment = page_start.saturating_sub(segment.at.as_u64());
            let skip_in_page = segment.at.as_u64().saturating_sub(page_start);
            if into_segment < segment.file_size {
                let from = segment.file_offset + into_segment;
                let left = segment.file_size - into_segment;
                let taking = left.min(PAGE_SIZE - skip_in_page) as usize;
                let from = from as usize;
                // SAFETY: the segment was checked to lie inside the file
                // before this ran (`elf::parse`), the frame is fresh from
                // the allocator so nothing else uses it, and the window
                // reaches it.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        file[from..from + taking].as_ptr(),
                        window.frame_ptr(frame).add(skip_in_page as usize),
                        taking,
                    )
                };
            }

            let flags = PageFlags::user(segment.writable(), segment.executable());
            // SAFETY: the frame is this process's alone and its space is
            // empty below the kernel's half.
            unsafe {
                building
                    .space
                    .map(
                        Page::containing_address(VirtAddr::new(page_at)),
                        frame,
                        flags,
                        frames,
                    )
                    .map_err(|err| match err {
                        // Two segments of one program in one page. The
                        // kernel will not guess which permissions the page
                        // should have (ADR 0026, point 3).
                        harlan_hal::paging::MapError::AlreadyMapped => {
                            SpawnError::SegmentsShareAPage { page: page_at }
                        }
                        other => SpawnError::Mapping(other),
                    })?;
            }
            page_at += PAGE_SIZE;
        }
        building.own(
            PhysRange::new(PhysAddr::new(segment.at.as_u64()), segment.memory_size),
            segment.writable(),
        )?;
    }

    // And a stack, which the file says nothing about: it is the kernel's
    // to give.
    let stack_frame = frames
        .allocate_for(FramePurpose::Stack)
        .ok_or(SpawnError::OutOfMemory)?;
    building.took(stack_frame, FramePurpose::Stack)?;
    // SAFETY: as above.
    unsafe {
        building
            .space
            .map(
                Page::containing_address(STACK_BASE),
                stack_frame,
                PageFlags::user(true, false),
                frames,
            )
            .map_err(SpawnError::Mapping)?;
    }
    building.own(
        PhysRange::new(PhysAddr::new(STACK_BASE.as_u64()), PAGE_SIZE),
        true,
    )?;

    Ok(())
}

/// Gives back everything a process that has ended was using: its memory,
/// its kernel stack, and the page tables of its own half
/// (docs/adr/0021-fase3-reclaiming-a-dead-space.md).
///
/// The three are told apart by what they were labelled when they were
/// handed out, so a leaf frame can never be given back as a page table or
/// the other way round. That check is the whole of the safety argument
/// for walking a dead space's tables.
///
/// # Safety
///
/// The process must not be running, its space must not be the one the CPU
/// is walking, and nothing may still be using its memory or its kernel
/// stack — including the stack this is called on.
pub unsafe fn destroy(
    mapper: &mut dyn harlan_hal::paging::PageMapper,
    frames: &mut KernelFrames<'_>,
    process: &mut Process,
) -> u64 {
    let mut returned = 0;
    for (frame, purpose) in process.frames.iter().flatten() {
        if frames.deallocate_as(*frame, *purpose).is_ok() {
            returned += 1;
        }
    }
    // SAFETY: the process is not running and nothing points into its
    // kernel stack any more (the caller's contract).
    returned += unsafe { stacks::unmap(mapper, frames, process.kernel_stack) };
    // SAFETY: the space is not active and nothing uses it again (the
    // caller's contract). Every frame offered is refused unless it was
    // labelled a page table, so the process's own memory — already given
    // back above — cannot go round twice.
    returned += unsafe {
        process
            .space
            .destroy(&mut |frame| frames.deallocate_as(frame, FramePurpose::PageTable).is_ok())
    };
    returned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges() -> [PhysRange; 2] {
        [
            PhysRange::new(PhysAddr::new(CODE_BASE.as_u64()), PAGE_SIZE),
            PhysRange::new(PhysAddr::new(STACK_BASE.as_u64()), PAGE_SIZE),
        ]
    }

    /// The check the ABI rests on: what a process hands the kernel has to
    /// be memory that process was given, and nothing else.
    #[test]
    fn a_pointer_is_only_good_if_it_is_this_process_s_own() {
        let mine = ranges();
        assert!(owned_by(&mine, CODE_BASE.as_u64(), 1));
        assert!(owned_by(&mine, CODE_BASE.as_u64(), PAGE_SIZE));
        assert!(owned_by(&mine, STACK_BASE.as_u64(), PAGE_SIZE));
        assert!(owned_by(&mine, STACK_BASE.as_u64() + PAGE_SIZE - 1, 1));

        assert!(
            !owned_by(&mine, CODE_BASE.as_u64(), PAGE_SIZE + 1),
            "runs past the page"
        );
        assert!(!owned_by(&mine, CODE_BASE.as_u64() - 1, 2), "starts below");
        assert!(!owned_by(&mine, CODE_BASE.as_u64(), 0), "nothing at all");
        assert!(!owned_by(&mine, u64::MAX, 1), "would wrap");
        assert!(!owned_by(&mine, 0xFFFF_8000_0000_0000, 8), "the kernel's");
        assert!(!owned_by(&mine, 0x0045_0000, 8), "the gap between the two");

        // Another process's memory is not this one's, even at the same
        // address: they are different spaces.
        let other = [PhysRange::new(PhysAddr::new(0x0080_0000), PAGE_SIZE)];
        assert!(!owned_by(&mine, 0x0080_0000, 8));
        assert!(!owned_by(&other, CODE_BASE.as_u64(), 8));
    }

    // -----------------------------------------------------------------
    // The descriptor table (docs/adr/0028-fase4-file-abi-v0.md)
    // -----------------------------------------------------------------

    /// An entry for a file of `size` bytes, named `name`.
    fn entry_named(name: &str, size: u32) -> DirectoryEntry {
        let mut bytes = [b' '; 12];
        let mut at = 0;
        for byte in name.bytes() {
            bytes[at] = byte;
            at += 1;
        }
        DirectoryEntry {
            name: bytes,
            name_len: at,
            attributes: 0x20,
            first_cluster: 3,
            size,
        }
    }

    fn open_file(name: &str) -> OpenFile {
        OpenFile {
            entry: entry_named(name, 100),
            position: 0,
        }
    }

    /// Descriptors are handed out from the lowest free slot, which is what
    /// makes a program that opens and closes in a loop not run out.
    #[test]
    fn a_closed_descriptor_is_handed_out_again() {
        let mut open = OpenFiles::new();
        assert_eq!(open.open(open_file("A.TXT")), Some(0));
        assert_eq!(open.open(open_file("B.TXT")), Some(1));
        assert_eq!(open.open(open_file("C.TXT")), Some(2));
        assert_eq!(open.count(), 3);

        assert!(open.close(1), "it was open");
        assert_eq!(open.count(), 2);
        assert_eq!(
            open.open(open_file("D.TXT")),
            Some(1),
            "the slot that was freed, not the next number"
        );

        // And a loop of open-then-close never runs out.
        for _ in 0..1000 {
            let at = open.open(open_file("E.TXT")).expect("a slot");
            assert!(open.close(at));
        }
    }

    /// Four, and the fifth is refused rather than taking somebody's slot.
    #[test]
    fn a_fifth_file_is_refused() {
        let mut open = OpenFiles::new();
        for at in 0..MAX_OPEN {
            assert_eq!(open.open(open_file("A.TXT")), Some(at));
        }
        assert_eq!(open.open(open_file("B.TXT")), None, "the fifth");
        // And nothing was lost: the four that were there are still there.
        assert_eq!(open.count(), MAX_OPEN);
        for at in 0..MAX_OPEN {
            assert!(open.at(at).is_some(), "descriptor {at}");
        }
    }

    /// A descriptor that was never opened, one that was closed, and one
    /// past the end of the table are the same answer. A program that
    /// guesses learns nothing from the difference, because there is none.
    #[test]
    fn a_descriptor_that_is_not_open_answers_nothing() {
        let mut open = OpenFiles::new();
        assert!(open.at(0).is_none(), "never opened");
        assert!(open.at(MAX_OPEN).is_none(), "past the table");
        assert!(open.at(usize::MAX).is_none(), "far past the table");
        assert!(!open.close(0), "closing one that is not open");
        assert!(!open.close(usize::MAX), "closing one that cannot exist");

        // With the table **full**, which is the case that matters: an
        // out-of-range descriptor must not be clamped into a real one.
        // Asked of an empty table, a clamped index lands on an empty slot
        // and answers `None` for the wrong reason — a mutation that
        // clamped survived until this was here.
        let mut full = OpenFiles::new();
        for _ in 0..MAX_OPEN {
            full.open(open_file("A.TXT")).expect("a slot");
        }
        assert!(
            full.at(MAX_OPEN).is_none(),
            "one past the last descriptor is not the last descriptor"
        );
        assert!(full.at(usize::MAX).is_none(), "nor is the largest number");
        assert!(!full.close(MAX_OPEN), "and it cannot be closed either");
        assert_eq!(full.count(), MAX_OPEN, "closing it closed nothing");

        let at = open.open(open_file("A.TXT")).expect("a slot");
        assert!(open.at(at).is_some());
        assert!(open.close(at));
        assert!(
            open.at(at).is_none(),
            "a closed descriptor is as absent as one that never was"
        );
        assert!(!open.close(at), "closed twice is closed once");
    }

    /// A position moves where a read put it, and nothing else moves it:
    /// there is no `seek` in v0.
    #[test]
    fn a_position_is_where_the_table_says() {
        let mut open = OpenFiles::new();
        let at = open.open(open_file("A.TXT")).expect("a slot");
        assert_eq!(
            open.at(at).expect("open").position,
            0,
            "starts at the start"
        );

        open.at(at).expect("open").position = 40;
        assert_eq!(open.at(at).expect("open").position, 40);

        // A second descriptor on the same file has its own position: two
        // readers of one file do not move each other along.
        let second = open.open(open_file("A.TXT")).expect("a slot");
        assert_eq!(open.at(second).expect("open").position, 0);
        assert_eq!(open.at(at).expect("open").position, 40);
    }

    /// Whether a name is open is asked without case, because that is how
    /// every other name comparison here works — and because the question
    /// is asked by `write_file`, where getting it wrong means writing over
    /// a file somebody is reading (ADR 0028, point 12).
    #[test]
    fn a_name_is_held_whichever_way_it_is_asked_for() {
        let mut open = OpenFiles::new();
        open.open(open_file("HELLO.TXT")).expect("a slot");
        assert!(open.holds("HELLO.TXT"));
        assert!(open.holds("hello.txt"), "without case");
        assert!(open.holds("Hello.Txt"), "without case");
        assert!(!open.holds("HELLO.BIN"), "a different file");
        assert!(!open.holds("HELLO"), "not a prefix of it");
        assert!(!open.holds(""), "nothing at all");
    }

    /// A process that ends keeps nothing, which includes the files it had
    /// open — otherwise a descriptor it left behind would keep that file
    /// unwritable for ever.
    #[test]
    fn ending_closes_everything() {
        let mut open = OpenFiles::new();
        for _ in 0..MAX_OPEN {
            open.open(open_file("A.TXT")).expect("a slot");
        }
        assert!(open.holds("A.TXT"));

        open.close_everything();
        assert_eq!(open.count(), 0);
        assert!(!open.holds("A.TXT"), "nothing keeps that file open now");
        assert_eq!(
            open.open(open_file("B.TXT")),
            Some(0),
            "and the table is free again"
        );
    }
}
