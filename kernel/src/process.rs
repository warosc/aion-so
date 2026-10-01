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
    /// The frames behind that memory, and what each was taken for, so
    /// that they can be given back when it exits.
    pub frames: [Option<(PhysFrame, FramePurpose)>; MAX_FRAMES],
    /// Where the CPU starts it. A flat program starts at the beginning of
    /// its page; an ELF says where (ADR 0026).
    pub entry: VirtAddr,
    /// Where it enters the kernel: its own stack, with guard pages
    /// (docs/adr/0018-fase3-context-switch.md).
    pub kernel_stack: Stack,
}

/// What a process is being built out of, while it is being built.
///
/// Kept apart from `Process` so that the bookkeeping — what has been
/// taken, what is owned — is written once and used by both ways of
/// starting one.
struct Building {
    space: AddressSpace,
    owned: [Option<PhysRange>; MAX_RANGES],
    frames: [Option<(PhysFrame, FramePurpose)>; MAX_FRAMES],
    ranges: usize,
    taken: usize,
}

impl Building {
    fn new(space: AddressSpace) -> Self {
        Self {
            space,
            owned: [None; MAX_RANGES],
            frames: [None; MAX_FRAMES],
            ranges: 0,
            taken: 0,
        }
    }

    fn own(&mut self, range: PhysRange) -> Result<(), SpawnError> {
        if self.ranges == MAX_RANGES {
            return Err(SpawnError::TooManyRanges);
        }
        self.owned[self.ranges] = Some(range);
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
            frames: self.frames,
            entry,
            kernel_stack,
        }
    }
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
        let mut ranges = [PhysRange::new(PhysAddr::new(0), 0); MAX_RANGES];
        for (at, owned) in self.owned.iter().enumerate() {
            if let Some(range) = owned {
                ranges[at] = *range;
            }
        }
        ranges
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
    building.own(PhysRange::new(PhysAddr::new(CODE_BASE.as_u64()), PAGE_SIZE))?;
    building.own(PhysRange::new(
        PhysAddr::new(STACK_BASE.as_u64()),
        PAGE_SIZE,
    ))?;
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
        building.own(PhysRange::new(
            PhysAddr::new(segment.at.as_u64()),
            segment.memory_size,
        ))?;
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
    building.own(PhysRange::new(
        PhysAddr::new(STACK_BASE.as_u64()),
        PAGE_SIZE,
    ))?;

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
}
