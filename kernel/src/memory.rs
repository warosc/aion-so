//! Memory management: the frame allocator (Incremento 5), the boot check
//! of the architecture's page mapper (Incremento 6) and the kernel heap
//! built on both (Incremento 7).

pub mod frame_allocator;
pub mod heap;
pub mod higher_half;
pub mod stacks;
pub mod zeroed_frames;

use alloc::boxed::Box;
use alloc::vec;

use alloc::vec::Vec;
use frame_allocator::{BitmapFrameAllocator, DeallocError, FramePurpose};

use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::frame::{FRAME_SIZE, PhysFrame};
use harlan_hal::memory_map::MemoryMap;
use harlan_hal::paging::{MapError, PAGE_SIZE, Page, PageFlags, PageMapper, UnmapError};
use zeroed_frames::{KernelFrames, ZeroedFrames};

/// Size of the boot frame allocator's bitmap, in 64-frame words: 1024
/// words = 65 536 frames = 256 MiB of physical address space, matching the
/// `-m 256M` machine `cargo xtask` runs. Usable RAM above that is ignored
/// (and logged), never mis-indexed. Real hardware (Fase 5) will need this
/// sized from the memory map instead.
pub const FRAME_BITMAP_WORDS: usize = 1024;

/// Copies what the frame allocator needs into the heap and returns an
/// allocator over those copies: the memory map and the bitmap, with every
/// frame that was already handed out still handed out. After this, nothing
/// the allocator reads or writes lives in the firmware's memory, which is
/// what lets that memory be reclaimed.
pub fn move_off_firmware_memory(
    frames: &KernelFrames<'_>,
    map: &MemoryMap,
) -> (KernelFrames<'static>, &'static MemoryMap) {
    let heap_map: &'static MemoryMap = Box::leak(Box::new(*map));
    let mut bitmap = vec![0u64; FRAME_BITMAP_WORDS].into_boxed_slice();
    bitmap.copy_from_slice(frames.bitmap());
    let storage: &'static mut [u64] = Box::leak(bitmap);
    // One byte per covered frame, so that from here on the kernel can say
    // what every frame it holds is for.
    let purposes: &'static mut [u8] =
        Box::leak(vec![0u8; FRAME_BITMAP_WORDS * 64].into_boxed_slice());
    // SAFETY: the same window, over the same frames, as the allocator this
    // one replaces.
    let moved = unsafe {
        ZeroedFrames::new(
            BitmapFrameAllocator::adopt(storage, heap_map, frames.boot_services_reclaimed()),
            frames.window(),
        )
        .with_purposes(purposes)
    };
    (moved, heap_map)
}

/// Takes `sample` frames from the pool, checks each arrives zeroed, writes a
/// pattern of its own into every byte, verifies them all and gives them
/// back. Run right after reclaiming the firmware's memory: if any of those
/// frames were still in use, or two of them were the same frame, the
/// patterns would not survive. Panics on a mismatch; returns how many
/// frames it exercised.
pub fn frame_pool_self_test(frames: &mut KernelFrames<'_>, sample: usize) -> usize {
    let window = frames.window();
    let mut taken: Vec<PhysFrame> = Vec::with_capacity(sample);
    while taken.len() < sample {
        let Some(frame) = frames.allocate_for(FramePurpose::Kernel) else {
            break;
        };
        let fill = taken.len() as u8 | 1;
        // SAFETY: the frame has just been handed to us, so nothing else is
        // using it, and the window reaches it.
        unsafe {
            let bytes = core::slice::from_raw_parts(window.frame_ptr(frame), FRAME_SIZE as usize);
            assert!(
                bytes.iter().all(|&byte| byte == 0),
                "frame {frame:?} was handed out holding old data"
            );
            window
                .frame_ptr(frame)
                .write_bytes(fill, FRAME_SIZE as usize);
        }
        taken.push(frame);
    }
    for (index, &frame) in taken.iter().enumerate() {
        let fill = index as u8 | 1;
        // SAFETY: still ours, same window.
        let bytes =
            unsafe { core::slice::from_raw_parts(window.frame_ptr(frame), FRAME_SIZE as usize) };
        assert!(
            bytes.iter().all(|&byte| byte == fill),
            "frame {frame:?} lost the pattern written into it: something else is using it"
        );
    }
    let exercised = taken.len();
    for frame in taken {
        // Freed as what it was taken for: a mismatch would be an error.
        assert_eq!(frames.deallocate_as(frame, FramePurpose::Kernel), Ok(()));
    }
    exercised
}

/// Boot-time check of the allocator against the real memory map, logged
/// through debugcon. An assertion failure means the allocator is broken,
/// so it panics (through the logging panic handler) rather than let it
/// hand out memory. `live_stack_addr` is the address of anything on the
/// stack the kernel is running on.
pub fn self_test(frames: &mut KernelFrames<'_>, live_stack_addr: VirtAddr) {
    // Under the firmware's identity map the two coincide; this runs before
    // the kernel takes over the page tables.
    let live_stack_addr = PhysAddr::new(live_stack_addr.as_u64());
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
        .allocate_for(FramePurpose::Kernel)
        .expect("free frames counted but none handed out");
    let b = frames
        .allocate()
        .expect("free frames counted but none handed out");
    // Once purposes are being recorded, giving a frame back as something
    // else is an error and frees nothing.
    if frames.purpose_of(a) == Some(FramePurpose::Kernel) {
        assert_eq!(
            frames.deallocate_as(a, FramePurpose::Heap),
            Err(DeallocError::WrongPurpose {
                expected: FramePurpose::Heap,
                actual: Some(FramePurpose::Kernel),
            })
        );
    }
    assert_ne!(a, b, "frame allocator handed out {a:?} twice");
    assert!(frames.manages(a) && frames.manages(b));
    assert_eq!(frames.free_frames(), before - 2);
    // A frame that carried data must come back zeroed, not with what its
    // previous owner left in it.
    let window = frames.window();
    // SAFETY: `a` is ours until it is deallocated below, and the window
    // reaches it (its contract, checked by the paging take-over).
    unsafe { window.frame_ptr(a).write_bytes(0xA5, FRAME_SIZE as usize) };
    assert_eq!(frames.deallocate_as(a, FramePurpose::Kernel), Ok(()));
    // Which error depends on whether purposes are being recorded yet: the
    // frame is no longer held, and it no longer has a purpose either.
    assert!(
        matches!(
            frames.deallocate_as(a, FramePurpose::Kernel),
            Err(DeallocError::NotAllocated | DeallocError::WrongPurpose { .. })
        ),
        "giving {a:?} back twice must fail"
    );
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
    assert_eq!(frames.deallocate_as(reused, FramePurpose::Kernel), Ok(()));
    assert_eq!(frames.deallocate_as(b, FramePurpose::Kernel), Ok(()));
    assert_eq!(frames.free_frames(), before);
    log::info!("HARLAN: frame allocator self-test OK (frames arrive zeroed)");
}

/// Records what the frames behind `[start, start + len)` are used for, by
/// asking the page tables which frame each page maps to. The heap and the
/// stacks are taken before the kernel has anywhere to write purposes down;
/// this fills that in afterwards. Returns how many frames it labelled.
pub fn label_mapped_frames(
    mapper: &dyn PageMapper,
    frames: &mut KernelFrames<'_>,
    start: VirtAddr,
    len: u64,
    purpose: FramePurpose,
) -> u64 {
    let mut labelled = 0;
    let mut addr = start;
    while addr < start + len {
        if let Some(phys) = mapper.translate(addr)
            && let Some(frame) = PhysFrame::from_start_address(phys)
        {
            labelled += u64::from(frames.label(frame, purpose));
        }
        addr = addr + PAGE_SIZE;
    }
    labelled
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
    unsafe { core::ptr::write_volatile(addr.as_ptr::<u64>(), PATTERN) };
    // SAFETY: the same frame through the firmware's identity map (checked
    // by the paging take-over before it wrote anything); a read, after the
    // write above.
    let identity = VirtAddr::new(frame.start_address().as_u64());
    let seen = unsafe { core::ptr::read_volatile(identity.as_ptr::<u64>()) };
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
    assert_eq!(frames.deallocate_as(frame, FramePurpose::Kernel), Ok(()));
    log::info!("HARLAN: page mapper self-test OK");
}
