//! The kernel heap (docs/adr/0006-fase2-kernel-heap.md): a `FreeListHeap`
//! behind an `IrqLock`, installed as the `#[global_allocator]`, over pages
//! of kernel space mapped at boot from free frames.

pub mod free_list;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

use free_list::{FreeListHeap, HeapCorruption, HeapStats};
use harlan_hal::InterruptControl;
use harlan_hal::paging::{PAGE_SIZE, Page, PageFlags, PageMapper};

use super::frame_allocator::BitmapFrameAllocator;
use crate::sync::IrqLock;

/// Fixed for Fase 2: the heap is mapped once at boot and never grows.
pub const HEAP_SIZE: usize = 4 * 1024 * 1024;

/// Allocate/verify/free cycles the boot self-test runs: a quick check on
/// every boot, or a long run with the `heap-stress` feature
/// (`cargo xtask boot-test --heap-stress`).
pub const STRESS_CYCLES: u64 = if cfg!(feature = "heap-stress") {
    200_000
} else {
    2_000
};

pub struct KernelHeap<I> {
    heap: IrqLock<I, FreeListHeap>,
}

impl<I: InterruptControl> KernelHeap<I> {
    pub const fn new(interrupts: I) -> Self {
        Self {
            heap: IrqLock::new(interrupts, FreeListHeap::empty()),
        }
    }

    /// # Safety
    ///
    /// As `FreeListHeap::init`: `[start, start + size)` stays valid for
    /// reads and writes for as long as the heap is used, and nothing else
    /// uses it.
    pub unsafe fn init(&self, start: *mut u8, size: usize) {
        // SAFETY: forwarded from the caller.
        unsafe { self.heap.lock().init(start, size) }
    }

    pub fn check(&self) -> Result<HeapStats, HeapCorruption> {
        self.heap.lock().check()
    }
}

// SAFETY: `FreeListHeap::allocate` only returns blocks inside its region,
// aligned as the layout asks, at least `layout.size()` bytes long and
// disjoint from every other live block (its invariants, host-tested and
// re-checked by `check`); the lock serializes every access. `dealloc`
// forwards the `GlobalAlloc` caller's promise that `ptr` came from `alloc`
// with the same layout.
unsafe impl<I: InterruptControl> GlobalAlloc for KernelHeap<I> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.heap
            .lock()
            .allocate(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let ptr = NonNull::new(ptr).expect("heap: dealloc of a null pointer");
        // SAFETY: forwarded from the caller (see the impl's comment).
        unsafe { self.heap.lock().deallocate(ptr, layout) }
    }
}

/// The global allocator. Not installed in host test builds, which keep the
/// system allocator; there it is an ordinary, never-initialized static.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(test), global_allocator)]
pub static HEAP: KernelHeap<harlan_arch_x86_64::Cpu> = KernelHeap::new(harlan_arch_x86_64::Cpu);

/// Maps up to `HEAP_SIZE` bytes of fresh frames from `start` on and hands
/// them to `heap`. Stops early, and logs why, if frames or page tables run
/// out; the pages mapped so far still become the heap. Returns the heap's
/// size in bytes (0: no heap, and any allocation will panic).
pub fn init<I: InterruptControl>(
    heap: &KernelHeap<I>,
    mapper: &mut dyn PageMapper,
    frames: &mut BitmapFrameAllocator<'_>,
    start: Page,
) -> usize {
    let data = PageFlags {
        writable: true,
        executable: false,
    };
    let mut mapped = 0;
    while mapped < HEAP_SIZE {
        let page = Page::containing_address(start.start_address() + mapped as u64);
        let Some(frame) = frames.allocate() else {
            log::warn!("HARLAN: heap: out of frames after {mapped} bytes");
            break;
        };
        // SAFETY: `frame` is fresh from the allocator, so nothing else uses
        // it; the heap's range of kernel space is used by the heap alone.
        if let Err(err) = unsafe { mapper.map(page, frame, data, frames) } {
            log::warn!("HARLAN: heap: mapping stopped after {mapped} bytes: {err:?}");
            // Never mapped: it goes straight back.
            let _ = frames.deallocate(frame);
            break;
        }
        mapped += PAGE_SIZE as usize;
    }
    if mapped > 0 {
        // SAFETY: `[start, start + mapped)` was just mapped writable to
        // fresh frames that nothing else uses, and is never unmapped. A
        // second initialization panics instead of taking effect.
        unsafe { heap.init(start.start_address() as usize as *mut u8, mapped) };
    }
    mapped
}

#[derive(Debug)]
pub struct StressReport {
    pub cycles: u64,
    pub peak_live_bytes: usize,
    pub after: HeapStats,
}

/// Deterministic allocate/verify/free workload through `heap`'s
/// `GlobalAlloc` interface. Every live block is filled with its own byte
/// and verified before it is freed, so two blocks overlapping, or the heap
/// writing its bookkeeping into a live block, shows up as a mismatch; the
/// free list is re-checked every 64 cycles. At the end everything is freed
/// and the free byte count must be back where it started. Panics on any
/// failure.
pub fn stress<I: InterruptControl>(heap: &KernelHeap<I>, cycles: u64, seed: u64) -> StressReport {
    const SLOTS: usize = 256;
    let mut slots: [Option<(NonNull<u8>, Layout, u8)>; SLOTS] = [None; SLOTS];
    let mut rng = SplitMix64(seed);
    let free_before = heap
        .check()
        .expect("heap corrupted before the stress run")
        .free;
    let (mut live_bytes, mut peak_live_bytes) = (0, 0);

    for cycle in 0..cycles {
        let r = rng.next();
        let slot = &mut slots[r as usize % SLOTS];
        match slot.take() {
            Some((ptr, layout, fill)) => {
                verify_and_free(heap, ptr, layout, fill);
                live_bytes -= layout.size();
            }
            None => {
                // Sizes 1..=8192, spread evenly over powers of two (so
                // mostly small); alignment 1..=128, and 4096 once in 64
                // allocations.
                let size = 1 + ((r >> 16) as usize % (1 << ((r >> 8) % 14)));
                let align = if (r >> 40).is_multiple_of(64) {
                    4096
                } else {
                    1 << ((r >> 32) % 8)
                };
                let layout = Layout::from_size_align(size, align).expect("valid layout");
                // SAFETY: `layout` has a non-zero size.
                let ptr = NonNull::new(unsafe { heap.alloc(layout) })
                    .unwrap_or_else(|| panic!("heap exhausted at cycle {cycle} ({layout:?})"));
                assert!(
                    ptr.as_ptr().addr().is_multiple_of(align),
                    "misaligned block"
                );
                let fill = (r >> 48) as u8;
                // SAFETY: a fresh block of at least `size` bytes.
                unsafe { ptr.as_ptr().write_bytes(fill, size) };
                *slot = Some((ptr, layout, fill));
                live_bytes += size;
                peak_live_bytes = peak_live_bytes.max(live_bytes);
            }
        }
        if cycle.is_multiple_of(64) {
            heap.check()
                .unwrap_or_else(|c| panic!("heap corrupted at cycle {cycle}: {c:?}"));
        }
    }
    for (ptr, layout, fill) in slots.iter_mut().filter_map(Option::take) {
        verify_and_free(heap, ptr, layout, fill);
    }
    let after = heap.check().expect("heap corrupted after the stress run");
    assert_eq!(after.free, free_before, "the stress run leaked heap memory");
    StressReport {
        cycles,
        peak_live_bytes,
        after,
    }
}

fn verify_and_free<I: InterruptControl>(
    heap: &KernelHeap<I>,
    ptr: NonNull<u8>,
    layout: Layout,
    fill: u8,
) {
    // SAFETY: a live block of `layout.size()` bytes that `stress` filled
    // and nothing else has touched.
    let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) };
    assert!(
        bytes.iter().all(|&b| b == fill),
        "heap corruption: block at {:#x} was overwritten",
        ptr.as_ptr().addr()
    );
    // SAFETY: allocated by `stress` through `heap` with `layout`, freed once.
    unsafe { heap.dealloc(ptr.as_ptr(), layout) };
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Stress cycles per round of `soak`.
pub const SOAK_ROUND_CYCLES: u64 = 20_000;

/// Soak builds only (`soak` feature, `cargo xtask soak-test`): endless
/// rounds of `stress` over the global heap, each with a different seed,
/// instead of the shell. Interrupts stay live throughout, so the timer
/// keeps firing between (and never inside) the heap's critical sections.
/// Logs a running total after every round; any corruption panics.
#[cfg(target_arch = "x86_64")]
pub fn soak() -> ! {
    log::info!("HARLAN: soak mode: heap stress rounds instead of the shell");
    let (mut round, mut total) = (0u64, 0u64);
    loop {
        round += 1;
        total += stress(&HEAP, SOAK_ROUND_CYCLES, 0x534F_414B ^ round).cycles;
        log::info!("HARLAN: soak round {round}: {total} heap cycles, 0 corruption");
    }
}

/// Boot-time check of the global heap: ordinary `alloc` types land inside
/// the heap's range and behave, then `STRESS_CYCLES` of `stress`.
#[cfg(target_arch = "x86_64")]
pub fn self_test(heap_start: u64, heap_bytes: usize) {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write;

    let in_heap = |addr: usize| {
        let addr = addr as u64;
        addr >= heap_start && addr < heap_start + heap_bytes as u64
    };
    let numbers: Vec<u64> = (1..=1000).collect();
    assert!(
        in_heap(numbers.as_ptr().addr()),
        "Vec memory is not from the kernel heap"
    );
    assert_eq!(numbers.iter().sum::<u64>(), 500_500);
    let boxed = Box::new([0x5Au8; 256]);
    assert!(in_heap(boxed.as_ptr().addr()) && boxed.iter().all(|&b| b == 0x5A));
    let mut text = String::new();
    write!(
        text,
        "{} {}",
        crate::identity::PRODUCT_NAME,
        crate::identity::VERSION
    )
    .expect("formatting");
    assert!(text.starts_with("HARLAN OS"));
    drop((numbers, boxed, text));

    let report = stress(&HEAP, STRESS_CYCLES, 0x4841_524C_414E);
    log::info!(
        "HARLAN: heap stress complete, {} cycles, 0 corruption (peak {} live bytes; after: {} hole(s), {} of {} bytes free)",
        report.cycles,
        report.peak_live_bytes,
        report.after.holes,
        report.after.free,
        report.after.size
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interrupt control of a host test: there is nothing to disable.
    struct NoInterrupts;

    impl InterruptControl for NoInterrupts {
        fn disable(&self) -> bool {
            false
        }

        fn enable(&self) {}

        fn are_enabled(&self) -> bool {
            false
        }
    }

    fn heap_over(buffer: &mut [u128]) -> KernelHeap<NoInterrupts> {
        let heap = KernelHeap::new(NoInterrupts);
        // SAFETY: the buffer outlives the heap in every test, and only the
        // heap touches it.
        unsafe { heap.init(buffer.as_mut_ptr().cast(), buffer.len() * 16) };
        heap
    }

    #[test]
    fn global_alloc_interface_round_trips() {
        let mut buffer = vec![0u128; 64];
        let heap = heap_over(&mut buffer);
        let layout = Layout::from_size_align(100, 32).unwrap();
        // SAFETY: non-zero size.
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null() && ptr.addr().is_multiple_of(32));
        // SAFETY: allocated just above with `layout`.
        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(heap.check().unwrap().holes, 1);
    }

    #[test]
    fn exhaustion_is_a_null_pointer_not_a_panic() {
        let mut buffer = vec![0u128; 8];
        let heap = heap_over(&mut buffer);
        // SAFETY: non-zero size.
        let ptr = unsafe { heap.alloc(Layout::from_size_align(4096, 8).unwrap()) };
        assert!(ptr.is_null());
    }

    /// The exact workload the kernel runs at boot, against the same heap
    /// and lock types, over a 1 MiB host buffer.
    #[test]
    fn the_boot_stress_workload_passes_on_the_host() {
        let mut buffer = vec![0u128; 1024 * 1024 / 16];
        let heap = heap_over(&mut buffer);
        let report = stress(&heap, 100_000, 0x4841_524C_414E);
        assert_eq!(report.cycles, 100_000);
        assert_eq!((report.after.holes, report.after.free), (1, 1024 * 1024));
        assert!(report.peak_live_bytes > 0);
    }

    #[test]
    #[should_panic(expected = "was overwritten")]
    fn the_stress_verifier_catches_an_overwritten_block() {
        let mut buffer = vec![0u128; 64];
        let heap = heap_over(&mut buffer);
        let layout = Layout::from_size_align(32, 16).unwrap();
        // SAFETY: non-zero size.
        let ptr = NonNull::new(unsafe { heap.alloc(layout) }).unwrap();
        // SAFETY: a fresh 32-byte block.
        unsafe { ptr.as_ptr().write_bytes(0xAA, 32) };
        // SAFETY: inside the same block: a stray write, as a bug would do.
        unsafe { ptr.as_ptr().add(7).write(0) };
        verify_and_free(&heap, ptr, layout, 0xAA);
    }
}
