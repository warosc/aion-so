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
}

/// What the kernel knows about a running program.
pub struct Process {
    /// Its tables. The kernel's half is in here too, shared.
    pub space: AddressSpace,
    /// What it was given, to check the pointers it hands the kernel
    /// against (ADR 0014, point 9).
    pub code: PhysRange,
    pub stack: PhysRange,
    /// The frames behind that memory, so that they can be given back when
    /// it exits.
    pub frames: [PhysFrame; 2],
    /// Where it enters the kernel: its own stack, with guard pages
    /// (docs/adr/0018-fase3-context-switch.md).
    pub kernel_stack: Stack,
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
        owned_by(&[self.code, self.stack], ptr, len)
    }

    /// Where its code is, as the CPU will see it.
    pub fn entry(&self) -> VirtAddr {
        CODE_BASE
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
    Ok(Process {
        space,
        code: PhysRange::new(PhysAddr::new(CODE_BASE.as_u64()), PAGE_SIZE),
        stack: PhysRange::new(PhysAddr::new(STACK_BASE.as_u64()), PAGE_SIZE),
        frames: [code_frame, stack_frame],
        kernel_stack,
    })
}

/// Gives back what a process that has exited was using: its memory and
/// its kernel stack.
///
/// Its page tables are **not** given back: the mapper does not yet know
/// which tables belong to which space, so five frames per dead process
/// stay held. Named here rather than forgotten
/// (docs/adr/0018-fase3-context-switch.md).
///
/// # Safety
///
/// The process must not be running, and nothing may still be using its
/// memory or its kernel stack — including the stack this is called on.
pub unsafe fn destroy(
    mapper: &mut dyn harlan_hal::paging::PageMapper,
    frames: &mut KernelFrames<'_>,
    process: &Process,
) -> u64 {
    let mut returned = 0;
    for (frame, purpose) in [
        (process.frames[0], FramePurpose::Kernel),
        (process.frames[1], FramePurpose::Stack),
    ] {
        if frames.deallocate_as(frame, purpose).is_ok() {
            returned += 1;
        }
    }
    // SAFETY: the process is not running and nothing points into its
    // kernel stack any more (the caller's contract).
    returned += unsafe { stacks::unmap(mapper, frames, process.kernel_stack) };
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
