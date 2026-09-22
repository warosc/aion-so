//! Memory management: the frame allocator (Incremento 5), the boot check
//! of the architecture's page mapper (Incremento 6) and the kernel heap
//! built on both (Incremento 7).

pub mod frame_allocator;
pub mod heap;
pub mod stacks;
pub mod zeroed_frames;

use frame_allocator::DeallocError;
use harlan_hal::frame::{FRAME_SIZE, PhysFrame};
use harlan_hal::paging::{MapError, Page, PageFlags, PageMapper, UnmapError};
use zeroed_frames::KernelFrames;

/// Size of the boot frame allocator's bitmap, in 64-frame words: 1024
/// words = 65 536 frames = 256 MiB of physical address space, matching the
/// `-m 256M` machine `cargo xtask` runs. Usable RAM above that is ignored
/// (and logged), never mis-indexed. Real hardware (Fase 5) will need this
/// sized from the memory map instead.
pub const FRAME_BITMAP_WORDS: usize = 1024;

/// Boot-time check of the allocator against the real memory map, logged
/// through debugcon. An assertion failure means the allocator is broken,
/// so it panics (through the logging panic handler) rather than let it
/// hand out memory. `live_stack_addr` is the address of anything on the
/// stack the kernel is running on.
pub fn self_test(frames: &mut KernelFrames<'_>, live_stack_addr: u64) {
    assert!(
        !frames.manages(PhysFrame::containing_address(live_stack_addr)),
        "frame allocator would hand out the live stack at {live_stack_addr:#x}"
    );

    let before = frames.free_frames();
    if before < 2 {
        // Not a broken allocator, just no memory to spare: say so and move
        // on (the round trip below needs two frames).
        log::warn!("HARLAN: frame allocator self-test skipped: fewer than 2 free frames");
        return;
    }
    let a = frames
        .allocate()
        .expect("free frames counted but none handed out");
    let b = frames
        .allocate()
        .expect("free frames counted but none handed out");
    assert_ne!(a, b, "frame allocator handed out {a:?} twice");
    assert!(frames.manages(a) && frames.manages(b));
    assert_eq!(frames.free_frames(), before - 2);
    // A frame that carried data must come back zeroed, not with what its
    // previous owner left in it.
    let window = frames.window();
    // SAFETY: `a` is ours until it is deallocated below, and the window
    // reaches it (its contract, checked by the paging take-over).
    unsafe { window.frame_ptr(a).write_bytes(0xA5, FRAME_SIZE as usize) };
    assert_eq!(frames.deallocate(a), Ok(()));
    assert_eq!(frames.deallocate(a), Err(DeallocError::NotAllocated));
    let reused = frames
        .allocate()
        .expect("the freed frame is available again");
    // SAFETY: `reused` is ours; same window.
    let bytes =
        unsafe { core::slice::from_raw_parts(window.frame_ptr(reused), FRAME_SIZE as usize) };
    assert!(
        bytes.iter().all(|&byte| byte == 0),
        "frame {reused:?} was handed out holding old data"
    );
    assert_eq!(frames.deallocate(reused), Ok(()));
    assert_eq!(frames.deallocate(b), Ok(()));
    assert_eq!(frames.free_frames(), before);
    log::info!("HARLAN: frame allocator self-test OK (frames arrive zeroed)");
}

/// Boot-time check of the page mapper on the real page tables: maps a
/// fresh frame at `test_page` (a kernel-space page nothing else uses),
/// writes through that mapping and reads the value back through the
/// frame's identity address, which proves the CPU really walks the tables
/// the mapper built; then unmaps it and frees the frame. The page tables
/// built on the way stay (three frames), ready for the next mapping
/// nearby. Panics on a broken mapper.
pub fn paging_self_test(
    mapper: &mut dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    test_page: Page,
) {
    let Some(frame) = frames.allocate() else {
        log::warn!("HARLAN: page mapper self-test skipped: no free frame");
        return;
    };
    let data = PageFlags {
        writable: true,
        executable: false,
    };
    // SAFETY: `frame` was just allocated, so nothing else uses it; the only
    // other access is the deliberate read through its identity address
    // below.
    unsafe { mapper.map(test_page, frame, data, frames) }
        .expect("mapping a fresh kernel-space page failed");
    let addr = test_page.start_address();
    assert_eq!(mapper.translate(addr), Some(frame.start_address()));
    // SAFETY: same page and frame as above; refused without writing.
    let again = unsafe { mapper.map(test_page, frame, data, frames) };
    assert_eq!(again, Err(MapError::AlreadyMapped));

    const PATTERN: u64 = u64::from_le_bytes(*b"HARLANOS");
    // SAFETY: `test_page` is mapped writable to `frame`, which nothing else
    // uses; a page-aligned address is aligned for `u64`.
    unsafe { core::ptr::write_volatile(addr as *mut u64, PATTERN) };
    // SAFETY: the same frame through the firmware's identity map (checked
    // by the paging take-over before it wrote anything); a read, after the
    // write above.
    let seen = unsafe { core::ptr::read_volatile(frame.start_address() as *const u64) };
    assert_eq!(
        seen, PATTERN,
        "a write through the new mapping missed its frame"
    );

    // SAFETY: nothing refers to `test_page` past this point.
    assert_eq!(unsafe { mapper.unmap(test_page) }, Ok(frame));
    assert_eq!(mapper.translate(addr), None);
    // SAFETY: nothing is mapped at `test_page` any more.
    assert_eq!(
        unsafe { mapper.unmap(test_page) },
        Err(UnmapError::NotMapped)
    );
    assert_eq!(frames.deallocate(frame), Ok(()));
    log::info!("HARLAN: page mapper self-test OK");
}
