//! Physical memory management. Incremento 5 adds the frame allocator; the
//! page mapper (Incremento 6) and the kernel heap (Incremento 7) build on it.

pub mod frame_allocator;

use frame_allocator::{BitmapFrameAllocator, DeallocError};
use harlan_hal::frame::PhysFrame;

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
pub fn self_test(frames: &mut BitmapFrameAllocator<'_>, live_stack_addr: u64) {
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
    assert_eq!(frames.deallocate(a), Ok(()));
    assert_eq!(frames.deallocate(a), Err(DeallocError::NotAllocated));
    assert_eq!(frames.deallocate(b), Ok(()));
    assert_eq!(frames.free_frames(), before);
    log::info!("HARLAN: frame allocator self-test OK");
}
