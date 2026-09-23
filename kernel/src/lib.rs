#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod identity;
mod memory;
mod shell;
mod sync;

use harlan_hal::addr::PhysAddr;
use harlan_hal::frame::{FRAME_SIZE, PhysFrame, PhysRange};
use harlan_hal::memory_map::{MemoryMap, MemoryRegionKind};
use harlan_hal::{Console, PowerControl};
use memory::frame_allocator::BitmapFrameAllocator;

/// Grepped by `cargo xtask boot-test` in the QEMU debugcon capture.
/// Keep in sync with tools/xtask's default `--marker` value.
pub const BOOT_OK_MARKER: &str = "HARLAN-PHASE0-BOOT-OK";

#[cfg(target_arch = "x86_64")]
pub const ARCH_NAME: &str = "x86_64";
#[cfg(not(target_arch = "x86_64"))]
pub const ARCH_NAME: &str = "unknown";

/// Boot handoff payload. Since Fase 2 Incremento 2, built from the real
/// UEFI memory map at the `ExitBootServices` transition — see
/// docs/adr/0002-fase2-exit-boot-services.md. `kernel` still has zero
/// dependency on the `uefi` crate: `MemoryMap` is `hal`'s own,
/// firmware-agnostic type.
pub struct BootInfo {
    pub memory_map: MemoryMap,
    /// Where the firmware loaded the kernel image, if it said. The kernel
    /// keeps these pages executable and marks the rest of the identity map
    /// no-execute (docs/adr/0008-fase2-write-xor-execute.md).
    pub kernel_image: Option<PhysRange>,
    /// Where the framebuffer the hardware console writes to lives. The
    /// kernel refuses to rebuild the identity map if that would leave this
    /// range unmapped, because the next character would fault.
    pub framebuffer: Option<PhysRange>,
}

/// The kernel takes ownership of everything `boot` hands over (see
/// docs/adr/0007-fase2-own-memory.md): once the heap is up it moves all of
/// it there, so that nothing of the kernel's is left in the firmware's
/// memory and that memory can be reclaimed.
pub fn kmain<C: Console + 'static, P: PowerControl + 'static>(
    boot_info: BootInfo,
    console: C,
    power: P,
) -> ! {
    // SAFETY: called exactly once, as the first thing kmain does, before
    // any other arch-specific state is touched.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        harlan_arch_x86_64::interrupts::init();
    }
    // SAFETY: called exactly once, immediately after `init()` above (GDT/
    // IDT already installed) and before anything else runs.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        harlan_arch_x86_64::interrupts::init_timer();
    }
    // SAFETY: called exactly once, right after `init_timer()` (the PIC is
    // remapped) and with interrupts still disabled — nothing above
    // enabled them.
    #[cfg(target_arch = "x86_64")]
    {
        if let Err(err) = unsafe { harlan_arch_x86_64::interrupts::init_keyboard() } {
            // Not fatal: the kernel is still useful (and debuggable via
            // debugcon) without input, and must never hang on a device.
            log::error!("HARLAN: PS/2 keyboard unavailable: {err:?}");
        }
    }
    // Every device is configured; this is the one place interrupts come
    // on. Before this point nothing can fire, so no handler can observe a
    // half-initialized device.
    #[cfg(target_arch = "x86_64")]
    {
        use harlan_hal::InterruptControl;
        harlan_arch_x86_64::Cpu.enable();
    }

    log::info!("{BOOT_OK_MARKER}");
    log::info!("HARLAN: architecture = {ARCH_NAME}");
    log::info!("HARLAN: GDT/IDT installed, breakpoint self-test OK");
    log::info!(
        "HARLAN: memory map = {} region(s), {} usable pages, {} boot-services pages held back",
        boot_info.memory_map.len(),
        boot_info.memory_map.total_usable_pages(),
        boot_info
            .memory_map
            .total_pages(MemoryRegionKind::BootServices)
    );

    // Lives as long as kmain, which never returns. It sits on the
    // firmware's stack, i.e. in boot-services memory the allocator itself
    // withholds, so the bitmap can never be handed out as a frame.
    let mut frame_bitmap = [0u64; memory::FRAME_BITMAP_WORDS];
    let bitmap = BitmapFrameAllocator::new(&mut frame_bitmap, &boot_info.memory_map);
    // The identity window below rests on one invariant: every frame the
    // allocator may hand out is mapped, writable, at its own address. It
    // was taken on faith from the map the kernel is running under; now it
    // is asked of the live page tables, before a single frame is written.
    // One walk per firmware page, not per frame: a 2 MiB or 1 GiB page has
    // a single entry, so identity and writability hold for all of it.
    #[cfg(target_arch = "x86_64")]
    {
        let covered = bitmap.covered_frames();
        let mut number = 0;
        while number < covered {
            let frame = PhysFrame::containing_address(PhysAddr::new(number * FRAME_SIZE));
            // SAFETY: CR3 is still the firmware root and nothing has
            // changed a page table yet (this runs before `take_over`);
            // the call only reads them.
            let run = unsafe {
                harlan_arch_x86_64::paging::KernelPageTable::active_identity_writable_run(frame)
            };
            match run {
                // `run` is a whole number of frames; `max(1)` only
                // guarantees the loop moves.
                Some(bytes) => number += (bytes / FRAME_SIZE).max(1),
                None => {
                    assert!(
                        !bitmap.manages(frame),
                        "the allocator would hand out {frame:?}, which the firmware does not map writable at its own address"
                    );
                    number += 1;
                }
            }
        }
        log::info!("HARLAN: identity window verified for {covered} covered frame(s)");
    }
    // Every frame leaves the allocator zeroed from here on: no page table
    // can start with junk entries and no frame carries what its previous
    // owner left in it.
    // SAFETY: the loop above just proved, against the live tables, that
    // every frame this allocator can hand out is identity-mapped writable.
    let mut frames = unsafe {
        memory::zeroed_frames::ZeroedFrames::new(
            bitmap,
            memory::zeroed_frames::PhysWindow::identity(),
        )
    };
    const MIB: u64 = 1024 * 1024;
    log::info!(
        "HARLAN: frame allocator = {} free frame(s) ({} MiB) in the {} MiB covered, {} usable frame(s) beyond it ignored",
        frames.free_frames(),
        frames.free_frames() * FRAME_SIZE / MIB,
        frames.covered_frames() * FRAME_SIZE / MIB,
        frames.uncovered_usable_frames()
    );
    // Virtual and physical addresses coincide (the firmware's identity map;
    // `KernelPageTable::take_over` checks it before writing anything), so
    // the address of a local is a physical address on the live stack. This
    // self-test writes no frame, so it can run before that check.
    let stack_probe = 0u8;
    memory::self_test(
        &mut frames,
        harlan_hal::addr::VirtAddr::new(core::ptr::addr_of!(stack_probe) as u64),
    );

    #[cfg(target_arch = "x86_64")]
    let mut kernel_stack_top = None;
    #[cfg(target_arch = "x86_64")]
    let mut heap_ready = false;
    #[cfg(target_arch = "x86_64")]
    let mut kernel_stacks = None;
    #[cfg(target_arch = "x86_64")]
    let mut heap_range = None;

    // The kernel takes the root page table over from the firmware
    // (docs/adr/0005-fase2-kernel-page-tables.md). Not fatal if refused: it
    // keeps running on the firmware's tables, just without a page mapper.
    #[cfg(target_arch = "x86_64")]
    let mut page_mapper = {
        use harlan_arch_x86_64::paging::KernelPageTable;
        // SAFETY: called once, here, on the only core, before anything else
        // creates page tables; the firmware's tables are still the active
        // ones, and no interrupt handler touches page tables.
        match unsafe { KernelPageTable::take_over(&mut frames) } {
            Ok(mapper) => {
                log::info!(
                    "HARLAN: paging = kernel root table at {:#x}, firmware identity map shared read-only",
                    mapper.root()
                );
                Some(mapper)
            }
            Err(err) => {
                log::error!(
                    "HARLAN: paging take-over refused ({err:?}); staying on the firmware's page tables"
                );
                None
            }
        }
    };
    #[cfg(target_arch = "x86_64")]
    if let Some(mapper) = &mut page_mapper {
        let test_page = harlan_hal::paging::Page::from_start_address(
            harlan_arch_x86_64::paging::KERNEL_SPACE_START,
        )
        .expect("kernel space starts on a page boundary");
        memory::paging_self_test(mapper, &mut frames, test_page);

        // The kernel heap (docs/adr/0006-fase2-kernel-heap.md). Without it
        // any allocation panics; nothing outside this block allocates yet.
        let heap_start = harlan_arch_x86_64::paging::KERNEL_HEAP_START;
        let heap_page = harlan_hal::paging::Page::from_start_address(heap_start)
            .expect("the heap starts on a page boundary");
        let heap_bytes = memory::heap::init(&memory::heap::HEAP, mapper, &mut frames, heap_page);
        heap_ready = heap_bytes > 0;
        if heap_bytes > 0 {
            log::info!(
                "HARLAN: heap = {} KiB at {heap_start:#x}; {} frame(s) left, {} zeroed so far",
                heap_bytes / 1024,
                frames.free_frames(),
                frames.zeroed_frames()
            );
            heap_range = Some((heap_start, heap_bytes as u64));
            memory::heap::self_test(heap_start, heap_bytes);
        } else {
            log::error!("HARLAN: no kernel heap: every allocation will panic");
        }

        // Stacks of the kernel's own, each between unmapped guard pages, so
        // an overflow faults at once instead of overwriting what is below
        // it. Not fatal if they cannot be mapped: the kernel stays on the
        // firmware's stack, unguarded, and says so.
        match memory::stacks::map_kernel_stacks(
            mapper,
            &mut frames,
            harlan_arch_x86_64::paging::KERNEL_STACKS_START,
        ) {
            Ok((kernel, double_fault)) => {
                // SAFETY: `double_fault` was just mapped for this, is
                // writable, stays mapped for the life of the kernel and
                // nothing else uses it. Not called from a double fault.
                unsafe { harlan_arch_x86_64::set_double_fault_stack(double_fault.top()) };
                log::info!(
                    "HARLAN: stacks = kernel {} KiB at {:#x}, double fault {} KiB at {:#x}, guard pages around both",
                    kernel.size() / 1024,
                    kernel.bottom(),
                    double_fault.size() / 1024,
                    double_fault.bottom()
                );
                kernel_stack_top = Some(kernel.top());
                kernel_stacks = Some((kernel, double_fault));
            }
            Err(err) => log::error!(
                "HARLAN: no guarded kernel stacks ({err:?}); staying on the firmware's stack"
            ),
        }
    }

    // With a mapper, a heap and a guarded stack, the kernel moves
    // everything it still keeps in the firmware's memory — the memory map,
    // the frame bitmap, the console and the power control — into its own
    // heap, and goes on running on its own stack. Nothing of the firmware's
    // is in use after that.
    #[cfg(target_arch = "x86_64")]
    if let (Some(mapper), Some(top)) = (page_mapper, kernel_stack_top)
        && heap_ready
    {
        let (heap_frames, heap_map) =
            memory::move_off_firmware_memory(&frames, &boot_info.memory_map);
        let context = alloc::boxed::Box::leak(alloc::boxed::Box::new(KernelContext {
            frames: heap_frames,
            map: heap_map,
            kernel_image: boot_info.kernel_image,
            framebuffer: boot_info.framebuffer,
            heap_range,
            kernel_stacks,
            mapper,
            console: alloc::boxed::Box::leak(alloc::boxed::Box::new(console)),
            power: alloc::boxed::Box::leak(alloc::boxed::Box::new(power)),
        }));
        log::info!("HARLAN: memory map, frame bitmap, console and power moved to the heap");
        // SAFETY: `top` is the top of the stack mapped for this — its own
        // frames, nothing else using them, an unmapped guard page at each
        // end — `kernel_main_on_stack` never returns, and `context` lives
        // on the heap, which outlives the stack being left behind.
        unsafe {
            harlan_arch_x86_64::stack::switch_to(
                top.as_u64(),
                kernel_main_on_stack,
                (context as *mut KernelContext).cast(),
            )
        }
    }

    log::error!("HARLAN: no mapper, heap or guarded stack: still running on the firmware's memory");
    let mut console = console;
    banner(&mut console);
    shell::run_shell(&mut console, &power)
}

/// Everything the long-running part of the kernel owns. It lives on the
/// heap, so the stack switch and the reclaiming of the firmware's memory
/// leave it untouched.
#[cfg(target_arch = "x86_64")]
struct KernelContext {
    frames: memory::zeroed_frames::KernelFrames<'static>,
    map: &'static MemoryMap,
    kernel_image: Option<PhysRange>,
    framebuffer: Option<PhysRange>,
    /// Where the heap is and how big, to label its frames.
    heap_range: Option<(harlan_hal::addr::VirtAddr, u64)>,
    kernel_stacks: Option<(memory::stacks::Stack, memory::stacks::Stack)>,
    mapper: harlan_arch_x86_64::paging::KernelPageTable,
    console: &'static mut dyn Console,
    power: &'static dyn PowerControl,
}

/// Entry point on the kernel's own stack (see `stack::switch_to`).
#[cfg(target_arch = "x86_64")]
extern "C" fn kernel_main_on_stack(context: *mut u8) -> ! {
    // SAFETY: `context` is the `KernelContext` `kmain` leaked onto the heap
    // and handed over here; nothing else refers to it.
    kernel_main(unsafe { &mut *context.cast::<KernelContext>() })
}

#[cfg(target_arch = "x86_64")]
fn kernel_main(context: &mut KernelContext) -> ! {
    use harlan_arch_x86_64::paging::DEFAULT_IDENTITY_LIMIT;

    // Which stack this is running on, so every boot log says whether the
    // switch to the guarded stack happened.
    let here = 0u8;
    log::info!(
        "HARLAN: kernel running on the stack at {:#x}",
        core::ptr::addr_of!(here) as u64
    );

    // What still runs through the identity map: the kernel's own image and
    // the firmware's runtime services code (`reboot` and `shutdown` call
    // into it). Everything else in the map becomes no-execute.
    let mut executable = alloc::vec::Vec::new();
    executable.extend(context.kernel_image);
    let mut ranges_sound = true;
    for region in context
        .map
        .iter()
        .filter(|region| region.kind == MemoryRegionKind::RuntimeCode)
    {
        // Firmware data: a `page_count` that overflows means the map
        // cannot be trusted to say what has to stay executable.
        match region.page_count.checked_mul(FRAME_SIZE) {
            Some(len) => executable.push(PhysRange::new(region.start_phys_addr, len)),
            None => {
                ranges_sound = false;
                log::error!(
                    "HARLAN: the RuntimeCode region at {} claims {} pages, which overflows",
                    region.start_phys_addr,
                    region.page_count
                );
            }
        }
    }
    if executable.is_empty() {
        log::warn!(
            "HARLAN: no executable range known; the identity map would fault on its own code"
        );
    }

    // An identity map of the kernel's own, with the null page left out, so
    // that no firmware page table is in use any more and a null
    // dereference faults.
    //
    // SAFETY: the kernel's image, its page tables, the framebuffer and
    // every frame the allocator can hand out are all below the limit; its
    // stack and heap are in kernel space, which this leaves alone;
    // `executable` lists every range of code still reached through this map
    // (the kernel image and the firmware's runtime services); nothing
    // depends on the null page; single core, and no interrupt handler
    // touches page tables.
    // The console still writes to the framebuffer through this map, and
    // the rebuild only covers what fits under the limit.
    let framebuffer_mapped = context.framebuffer.is_none_or(|range| {
        range.len > 0
            && range.start.checked_add(range.len).is_some()
            && range.end().as_u64() <= DEFAULT_IDENTITY_LIMIT
    });
    if !framebuffer_mapped {
        log::error!(
            "HARLAN: the framebuffer at {:?} would fall outside the {} GiB identity map; the firmware's tables stay in use",
            context.framebuffer,
            DEFAULT_IDENTITY_LIMIT / (1024 * 1024 * 1024)
        );
    }
    let own_tables = ranges_sound
        && framebuffer_mapped
        && !executable.is_empty()
        && match unsafe {
            context.mapper.rebuild_identity_map(
                &mut context.frames,
                DEFAULT_IDENTITY_LIMIT,
                &executable,
            )
        } {
            Ok(stats) => {
                log::info!(
                    "HARLAN: identity map rebuilt from {} table(s) of the kernel's own, covering {} GiB, null page unmapped, {} page(s) executable of {} range(s), everything else no-execute",
                    stats.tables,
                    stats.limit / (1024 * 1024 * 1024),
                    stats.executable_pages,
                    executable.len()
                );
                true
            }
            Err(err) => {
                log::error!(
                    "HARLAN: identity map not rebuilt ({err:?}); the firmware's tables stay in use"
                );
                false
            }
        };

    // Only now, with nothing of the firmware's left in use, does its memory
    // join the pool.
    if own_tables {
        let reclaimed = context.frames.reclaim_boot_services();
        log::info!(
            "HARLAN: boot-services memory reclaimed: +{reclaimed} frame(s) ({} MiB), {} free now",
            reclaimed * FRAME_SIZE / (1024 * 1024),
            context.frames.free_frames()
        );
        // Prove the pool really owns what it just took over.
        let exercised = memory::frame_pool_self_test(&mut context.frames, 256);
        log::info!("HARLAN: frame pool self-test OK ({exercised} frames written and verified)");
    } else {
        log::warn!("HARLAN: boot-services memory stays held back");
    }

    // What the kernel is holding, and what for. The heap and the stacks
    // were taken before there was anywhere to write purposes down, so they
    // are filled in here, from the page tables.
    {
        use memory::frame_allocator::FramePurpose;
        if let Some((start, len)) = context.heap_range {
            memory::label_mapped_frames(
                &context.mapper,
                &mut context.frames,
                start,
                len,
                FramePurpose::Heap,
            );
        }
        for stack in context.kernel_stacks.iter().flat_map(|(a, b)| [a, b]) {
            memory::label_mapped_frames(
                &context.mapper,
                &mut context.frames,
                stack.bottom(),
                stack.size(),
                FramePurpose::Stack,
            );
        }
        let mut held = 0;
        for purpose in FramePurpose::ALL {
            let frames = context.frames.frames_for(purpose);
            held += frames;
            if frames > 0 {
                log::info!("HARLAN: frames in use: {frames} for {}", purpose.name());
            }
        }
        log::info!(
            "HARLAN: frames = {} free, {held} in use",
            context.frames.free_frames()
        );
    }

    // Soak builds (`cargo xtask soak-test`) never reach the shell: they run
    // heap stress rounds until QEMU is stopped.
    if cfg!(feature = "soak") {
        memory::heap::soak();
    }

    let console = &mut *context.console;
    banner(console);
    shell::run_shell(console, context.power)
}

fn banner(console: &mut dyn Console) {
    console.write_str(identity::PRODUCT_NAME);
    console.write_str(" ");
    console.write_str(identity::VERSION);
    console.write_str("\n");
    console.write_str(identity::TAGLINE);
    console.write_str("\n\n");
    console.write_str("Boot............ UEFI OK\n");
    console.write_str("Architecture.... ");
    console.write_str(ARCH_NAME);
    console.write_str("\n");
    console.write_str("Kernel.......... READY\n\n");
}
