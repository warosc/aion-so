#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod console;
#[cfg(target_arch = "x86_64")]
pub mod devices;
pub mod events;
pub mod fs;
pub mod identity;
pub mod ipc;
#[cfg(target_arch = "x86_64")]
pub mod klog;
mod memory;
#[cfg(target_arch = "x86_64")]
pub mod process;
#[cfg(target_arch = "x86_64")]
pub mod scheduler;
mod shell;
mod sync;
#[cfg(target_arch = "x86_64")]
pub mod user;

use harlan_hal::addr::PhysAddr;
use harlan_hal::frame::{FRAME_SIZE, PhysFrame, PhysRange};
use harlan_hal::memory_map::{CachePolicy, MemoryMap, MemoryRegionKind};
use harlan_hal::paging::MappedRange;
use harlan_hal::{Console, PowerControl};
use harlan_hal::{error, info, warn};
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
    /// Which parts of that image are code. Everything else in it is data:
    /// no-execute, while the code itself becomes read-only
    /// (docs/adr/0011-fase2-write-xor-execute-inside-the-image.md). `None`
    /// if the headers could not be read, and then the whole image stays
    /// executable as it did before.
    pub kernel_code: Option<harlan_hal::pe::CodeRanges>,
    /// Where the framebuffer the hardware console writes to lives. The
    /// kernel refuses to rebuild the identity map if that would leave this
    /// range unmapped, because the next character would fault.
    pub framebuffer: Option<PhysRange>,
    /// The one UEFI call the kernel cannot make itself: telling the
    /// firmware where its runtime services will answer from now on. The
    /// bootloader lends it; the kernel decides the address and the moment
    /// (docs/adr/0016-fase3-set-virtual-address-map.md).
    pub relocate_runtime: Option<memory::runtime::Relocate>,
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
            error!("HARLAN: PS/2 keyboard unavailable: {err:?}");
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

    info!("{BOOT_OK_MARKER}");
    info!("HARLAN: architecture = {ARCH_NAME}");
    info!("HARLAN: GDT/IDT installed, breakpoint self-test OK");
    info!(
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
        info!("HARLAN: identity window verified for {covered} covered frame(s)");
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
    info!(
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
                info!(
                    "HARLAN: paging = kernel root table at {:#x}, firmware identity map shared read-only",
                    mapper.root()
                );
                Some(mapper)
            }
            Err(err) => {
                error!(
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
            info!(
                "HARLAN: heap = {} KiB at {heap_start:#x}; {} frame(s) left, {} zeroed so far",
                heap_bytes / 1024,
                frames.free_frames(),
                frames.zeroed_frames()
            );
            heap_range = Some((heap_start, heap_bytes as u64));
            memory::heap::self_test(heap_start, heap_bytes);
        } else {
            error!("HARLAN: no kernel heap: every allocation will panic");
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
            Ok((kernel, double_fault, syscall)) => {
                // SAFETY: `double_fault` was just mapped for this, is
                // writable, stays mapped for the life of the kernel and
                // nothing else uses it. Not called from a double fault.
                unsafe { harlan_arch_x86_64::set_double_fault_stack(double_fault.top()) };
                info!(
                    "HARLAN: stacks = kernel {} KiB at {:#x}, double fault {} KiB at {:#x}, syscall {} KiB at {:#x}, guard pages around each",
                    kernel.size() / 1024,
                    kernel.bottom(),
                    double_fault.size() / 1024,
                    double_fault.bottom(),
                    syscall.size() / 1024,
                    syscall.bottom()
                );
                kernel_stack_top = Some(kernel.top());
                kernel_stacks = Some((kernel, double_fault, syscall));
            }
            Err(err) => error!(
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
            kernel_code: boot_info.kernel_code,
            framebuffer: boot_info.framebuffer,
            relocate_runtime: boot_info.relocate_runtime,
            physmap: None,
            heap_range,
            kernel_stacks,
            mapper,
            console: alloc::boxed::Box::leak(alloc::boxed::Box::new(console)),
            power: alloc::boxed::Box::leak(alloc::boxed::Box::new(power)),
        }));
        info!("HARLAN: memory map, frame bitmap, console and power moved to the heap");
        // SAFETY: `top` is the top of the stack mapped for this — its own
        // frames, nothing else using them, an unmapped guard page at each
        // end — `kernel_main_on_stack` never returns, and `context` lives
        // on the heap, which outlives the stack being left behind.
        unsafe {
            harlan_arch_x86_64::stack::switch_to(
                top.as_u64(),
                kernel_main_on_stack::<C, P>,
                (context as *mut KernelContext<C, P>).cast(),
            )
        }
    }

    error!("HARLAN: no mapper, heap or guarded stack: still running on the firmware's memory");
    let mut console = console;
    banner(&mut console);
    shell::run_shell(&mut console, &power)
}

/// Everything the long-running part of the kernel owns. It lives on the
/// heap, so the stack switch and the reclaiming of the firmware's memory
/// leave it untouched.
#[cfg(target_arch = "x86_64")]
struct KernelContext<C: Console + 'static, P: PowerControl + 'static> {
    frames: memory::zeroed_frames::KernelFrames<'static>,
    map: &'static MemoryMap,
    kernel_image: Option<PhysRange>,
    kernel_code: Option<harlan_hal::pe::CodeRanges>,
    framebuffer: Option<PhysRange>,
    relocate_runtime: Option<memory::runtime::Relocate>,
    /// Where physical memory is readable, once it is not the lower half.
    physmap: Option<harlan_hal::addr::VirtAddr>,
    /// Where the heap is and how big, to label its frames.
    heap_range: Option<(harlan_hal::addr::VirtAddr, u64)>,
    kernel_stacks: Option<(
        memory::stacks::Stack,
        memory::stacks::Stack,
        memory::stacks::Stack,
    )>,
    mapper: harlan_arch_x86_64::paging::KernelPageTable,
    // Not `dyn`: a trait object's vtable pointer is written at runtime
    // and names the image where the firmware loaded it, which stops being
    // mapped once the lower half becomes user space (ADR 0013). Keeping
    // the concrete types means there is no such pointer to go stale.
    console: &'static mut C,
    power: &'static P,
}

/// Entry point on the kernel's own stack (see `stack::switch_to`).
#[cfg(target_arch = "x86_64")]
extern "C" fn kernel_main_on_stack<C: Console + 'static, P: PowerControl + 'static>(
    context: *mut u8,
) -> ! {
    // SAFETY: `context` is the `KernelContext` `kmain` leaked onto the heap
    // and handed over here; nothing else refers to it.
    kernel_main(unsafe { &mut *context.cast::<KernelContext<C, P>>() })
}

#[cfg(target_arch = "x86_64")]
fn kernel_main<C: Console + 'static, P: PowerControl + 'static>(
    context: &mut KernelContext<C, P>,
) -> ! {
    // Which stack this is running on, so every boot log says whether the
    // switch to the guarded stack happened.
    let here = 0u8;
    info!(
        "HARLAN: kernel running on the stack at {:#x}, code at {:#x}",
        core::ptr::addr_of!(here) as u64,
        kernel_main::<C, P> as *const () as u64
    );

    // Out of the address the firmware chose and into kernel space, so the
    // lower half can become user space (ADR 0012). Only the image's own
    // mapping moves: the stack and the heap are already up here.
    if let Some(image) = context.kernel_image {
        use harlan_hal::InterruptControl;
        let cpu = harlan_arch_x86_64::Cpu;
        // The descriptor tables name the old addresses until the far side
        // reinstalls them, so nothing may be delivered in between.
        cpu.disable();
        // SAFETY: the alias slot (PML4 259) is used by nothing else; the
        // image is where this kernel runs from and is still writable at
        // its own address (the identity map is rebuilt later); interrupts
        // are off until the far side has reinstalled the tables.
        let plan = unsafe {
            memory::higher_half::prepare(
                &mut context.mapper,
                &mut context.frames,
                harlan_arch_x86_64::paging::KERNEL_IMAGE_START,
                image,
                context.kernel_code,
            )
        };
        match plan {
            Ok(plan) => match memory::higher_half::moved(
                continue_in_kernel_space::<C, P> as *const () as u64,
                image,
                &plan,
            ) {
                Some(target) => {
                    // SAFETY: `target` is this very function's twin in the
                    // alias, which is mapped executable and holds the same
                    // code; the signature is the one declared below, and
                    // it never returns. `context` and `plan` live on the
                    // kernel stack and heap, both of which stay where they
                    // are.
                    let entry: extern "C" fn(
                        *mut KernelContext<C, P>,
                        *const memory::higher_half::Move,
                    ) -> ! = unsafe { core::mem::transmute(target.as_ptr::<()>()) };
                    entry(context as *mut KernelContext<C, P>, &plan)
                }
                None => error!(
                    "HARLAN: the kernel's own code is not inside the image the firmware reported; staying where it is"
                ),
            },
            Err(err) => error!(
                "HARLAN: the kernel could not move into kernel space ({err:?}); it keeps running from where the firmware loaded it"
            ),
        }
        cpu.enable();
    } else {
        error!("HARLAN: no image range known; the kernel keeps running where it was loaded");
    }

    run(context)
}

/// The kernel, now reached through its kernel-space alias.
///
/// Called exactly once, by `kernel_main`, through a pointer into the alias
/// and with interrupts disabled.
#[cfg(target_arch = "x86_64")]
extern "C" fn continue_in_kernel_space<C: Console + 'static, P: PowerControl + 'static>(
    context: *mut KernelContext<C, P>,
    plan: *const memory::higher_half::Move,
) -> ! {
    use harlan_hal::InterruptControl;

    // SAFETY: both point at what `kernel_main` handed over — the leaked
    // context on the heap and its own stack slot — and neither moved.
    let (context, plan) = unsafe { (&mut *context, *plan) };

    // The tables still name the addresses the kernel used to have.
    // SAFETY: interrupts are disabled, the relocations are applied, and
    // the double-fault stack is reinstalled right below.
    unsafe { harlan_arch_x86_64::interrupts::reinstall_descriptors() };
    if let Some((_, double_fault, _)) = context.kernel_stacks {
        // SAFETY: the same stack as in `kmain`: mapped, guarded, used by
        // nothing else, and this is not a double fault.
        unsafe { harlan_arch_x86_64::set_double_fault_stack(double_fault.top()) };
    }
    // The log sink is code, and code just moved: point it at where it
    // lives now, before anything logs through the old address.
    klog::install();

    // The window the kernel reaches physical memory through moves out of
    // the lower half too (ADR 0013). Until it does, every frame handed out
    // is zeroed through the identity map.
    match unsafe {
        context.mapper.map_physical_window(
            &mut context.frames,
            harlan_arch_x86_64::paging::KERNEL_PHYSMAP_START,
            harlan_arch_x86_64::paging::DEFAULT_IDENTITY_LIMIT,
        )
    } {
        Ok(tables) => {
            let base = harlan_arch_x86_64::paging::KERNEL_PHYSMAP_START;
            // SAFETY: the window just mapped covers every frame the
            // allocator can hand out — the same range the identity map
            // covered — writable and with nothing else using it.
            unsafe {
                context
                    .frames
                    .set_window(memory::zeroed_frames::PhysWindow::new(base.as_u64()))
            };
            // SAFETY: the same window, which maps every page table and
            // every frame that may become one, writable.
            unsafe { context.mapper.use_physical_window(base) };
            context.physmap = Some(base);
            info!(
                "HARLAN: physical memory readable at {base:#x} ({} GiB through {tables} table(s)); frames and page tables are reached there now",
                harlan_arch_x86_64::paging::DEFAULT_IDENTITY_LIMIT / (1024 * 1024 * 1024)
            );
            // The console draws straight into the framebuffer, so it has
            // to be told where that is now.
            if let Some(framebuffer) = context.framebuffer {
                let moved =
                    harlan_hal::addr::VirtAddr::new(base.as_u64() + framebuffer.start.as_u64());
                // SAFETY: the same framebuffer, reached through the window
                // that maps all of physical memory below the limit; the
                // check below keeps it inside that limit.
                if framebuffer.end().as_u64() <= harlan_arch_x86_64::paging::DEFAULT_IDENTITY_LIMIT
                {
                    unsafe { context.console.framebuffer_moved(moved) };
                    info!("HARLAN: the console draws at {moved:#x} now");
                }
            }
        }
        Err(err) => error!(
            "HARLAN: no window onto physical memory ({err:?}); the kernel keeps reaching frames through the identity map"
        ),
    }

    harlan_arch_x86_64::Cpu.enable();

    let here = 0u8;
    info!(
        "HARLAN: kernel moved into kernel space: code at {:#x} (was {:#x}), {} page(s) mapped, {} read-only, {} address(es) relocated; stack at {:#x}",
        continue_in_kernel_space::<C, P> as *const () as u64,
        (continue_in_kernel_space::<C, P> as *const () as u64).wrapping_sub(plan.delta),
        plan.pages,
        plan.read_only,
        plan.relocations,
        core::ptr::addr_of!(here) as u64
    );
    run(context)
}

/// The long-running kernel: its own identity map, the firmware's memory
/// back in the pool, and the shell.
#[cfg(target_arch = "x86_64")]
fn run<C: Console + 'static, P: PowerControl + 'static>(context: &mut KernelContext<C, P>) -> ! {
    // The console goes to its module, where a syscall can reach it
    // (docs/adr/0029-fase4-console-and-list-abi.md). Here and not where the
    // context was built: the trait object formed below carries a vtable
    // address, and one formed before the kernel moved to the higher half
    // would name an image that is no longer mapped — which is the reason
    // `KernelContext` keeps concrete types.
    //
    // From this line on, `context.console` is not read again. The module is
    // the only way to the console, and the kernel's own shell gets it back
    // through `console::take`.
    // SAFETY: the console was leaked onto the heap by `kmain`, so it lives
    // as long as the kernel; this runs after the move to the higher half,
    // so the vtable address is mapped; and nothing else uses the console
    // through another reference from here on.
    unsafe { console::adopt(&raw mut *context.console) };

    use harlan_arch_x86_64::paging::DEFAULT_IDENTITY_LIMIT;

    // What still runs through the identity map: the kernel's own code and
    // the firmware's runtime services code (`reboot` and `shutdown` call
    // into it). Everything else in the map becomes no-execute, and these
    // ranges become read-only (ADR 0011).
    let mut executable = alloc::vec::Vec::new();
    match context.kernel_code {
        Some(code) => {
            executable.extend(code.iter().map(MappedRange::read_only_code));
            info!(
                "HARLAN: kernel code = {} section(s), {} KiB of the {} KiB image",
                code.len(),
                code.total_bytes() / 1024,
                context.kernel_image.map_or(0, |image| image.len) / 1024
            );
        }
        // The headers could not be read: the whole image stays
        // executable, and writable, which is what the kernel did before
        // ADR 0011. Its data lives in there too.
        None => executable.extend(
            context
                .kernel_image
                .map(|image| MappedRange::writable_code(image, CachePolicy::WriteBack)),
        ),
    }
    let mut ranges_sound = true;
    for region in context
        .map
        .iter()
        .filter(|region| region.kind == MemoryRegionKind::RuntimeCode)
    {
        // Firmware data: a `page_count` that overflows means the map
        // cannot be trusted to say what has to stay executable.
        match region.page_count.checked_mul(FRAME_SIZE) {
            // Writable: OVMF writes inside its own runtime code, and
            // `shutdown` faults with `#PF error_code=0x3` if this range is
            // read-only (measured; ADR 0011).
            Some(len) => executable.push(MappedRange::writable_code(
                PhysRange::new(region.start_phys_addr, len),
                region.attributes.cache,
            )),
            None => {
                ranges_sound = false;
                error!(
                    "HARLAN: the RuntimeCode region at {} claims {} pages, which overflows",
                    region.start_phys_addr, region.page_count
                );
            }
        }
    }
    if executable.is_empty() {
        warn!("HARLAN: no executable range known; the identity map would fault on its own code");
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
        error!(
            "HARLAN: the framebuffer at {:?} would fall outside the {} GiB identity map; the firmware's tables stay in use",
            context.framebuffer,
            DEFAULT_IDENTITY_LIMIT / (1024 * 1024 * 1024)
        );
    }
    // With the window in kernel space the kernel needs nothing down
    // there at all: only the firmware's own code stays, at the addresses
    // it was compiled for (ADR 0013).
    let own_tables = if let (Some(window), true) = (context.physmap, ranges_sound) {
        // Everything the firmware still needs: UEFI asks for every
        // descriptor carrying EFI_MEMORY_RUNTIME to stay mapped, whatever
        // its type — the memory-mapped I/O a runtime service talks to
        // carries it too — and with the caching it was reported with.
        let mut firmware: alloc::vec::Vec<harlan_hal::paging::MappedRange> = alloc::vec::Vec::new();
        let mut every_range_sound = true;
        for region in context
            .map
            .iter()
            .filter(|region| region.attributes.runtime)
        {
            let Some(len) = region.page_count.checked_mul(FRAME_SIZE) else {
                every_range_sound = false;
                error!(
                    "HARLAN: the runtime region at {} claims {} pages, which overflows",
                    region.start_phys_addr, region.page_count
                );
                continue;
            };
            let range = PhysRange::new(region.start_phys_addr, len);
            let cache = region.attributes.cache;
            let executable = region.kind == MemoryRegionKind::RuntimeCode;
            info!(
                "HARLAN: the firmware keeps {}..{} ({} KiB, {}, {cache:?})",
                range.start,
                range.end(),
                range.len / 1024,
                if executable { "code" } else { "data" }
            );
            firmware.push(if executable {
                harlan_hal::paging::MappedRange::writable_code(range, cache)
            } else {
                harlan_hal::paging::MappedRange::data(range, cache)
            });
        }
        // A map with a hole in it cannot say what may go: the region that
        // was dropped might be one the firmware needs.
        if !context.map.is_complete() {
            every_range_sound = false;
            error!("HARLAN: the firmware's memory map was truncated, so nothing here is complete");
        }
        // Emptying the lower half without everything a runtime call needs
        // would turn `shutdown` into a fault, so a map that cannot be
        // trusted leaves it as it is.
        if !every_range_sound {
            error!(
                "HARLAN: the firmware's map cannot be trusted; the lower half stays mapped as it is"
            );
            false
        } else {
            // First the firmware is asked to move out of the lower half
            // altogether (ADR 0016). If it will not, its ranges stay where
            // they are and the kernel keeps them mapped, as before.
            let runtime = harlan_arch_x86_64::paging::KERNEL_RUNTIME_START;
            let moved = context.relocate_runtime.and_then(|relocate| {
                // SAFETY: the kernel owns its tables and reaches frames
                // through its own window; the firmware's old mappings are
                // still in place, and this is the only call.
                unsafe {
                    memory::runtime::move_to_kernel_space(
                        &mut context.mapper,
                        &mut context.frames,
                        context.map,
                        runtime,
                        relocate,
                    )
                }
                .ok()
            });
            let keep: &[harlan_hal::paging::MappedRange] =
                if moved.is_some() { &[] } else { &firmware };
            // SAFETY: the kernel's code, stack, heap, page tables and the
            // window it reaches frames through are all in kernel space by
            // now, and the console draws through the window. What is left
            // below, if anything, is the firmware's at its own addresses.
            match unsafe {
                context
                    .mapper
                    .keep_only_in_lower_half(&mut context.frames, keep)
            } {
                Ok(tables) => {
                    if moved.is_some() {
                        info!(
                            "HARLAN: the lower half is empty: nothing of the firmware's is left there, and the kernel reaches memory at {window:#x}"
                        );
                    } else {
                        info!(
                            "HARLAN: the lower half is free: {} firmware range(s) kept through {tables} table(s), everything else unmapped; the kernel reaches memory at {window:#x}",
                            keep.len()
                        );
                    }
                    true
                }
                Err(err) => {
                    error!(
                        "HARLAN: the lower half could not be emptied ({err:?}); the kernel keeps the map it has"
                    );
                    false
                }
            }
        }
    } else {
        ranges_sound
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
                    info!(
                        "HARLAN: identity map rebuilt from {} table(s) of the kernel's own, covering {} GiB, null page unmapped, {} page(s) executable of {} range(s) ({} of them read-only), everything else no-execute and writable",
                        stats.tables,
                        stats.limit / (1024 * 1024 * 1024),
                        stats.executable_pages,
                        executable.len(),
                        stats.read_only_pages
                    );
                    true
                }
                Err(err) => {
                    error!(
                        "HARLAN: identity map not rebuilt ({err:?}); the firmware's tables stay in use"
                    );
                    false
                }
            }
    };

    // Only now, with nothing of the firmware's left in use, does its memory
    // join the pool.
    if own_tables {
        let reclaimed = context.frames.reclaim_boot_services();
        info!(
            "HARLAN: boot-services memory reclaimed: +{reclaimed} frame(s) ({} MiB), {} free now",
            reclaimed * FRAME_SIZE / (1024 * 1024),
            context.frames.free_frames()
        );
        // Prove the pool really owns what it just took over.
        let exercised = memory::frame_pool_self_test(&mut context.frames, 256);
        info!("HARLAN: frame pool self-test OK ({exercised} frames written and verified)");
    } else {
        warn!("HARLAN: boot-services memory stays held back");
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
        for stack in context.kernel_stacks.iter().flat_map(|(a, b, c)| [a, b, c]) {
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
                info!("HARLAN: frames in use: {frames} for {}", purpose.name());
            }
        }
        info!(
            "HARLAN: frames = {} free, {held} in use",
            context.frames.free_frames()
        );
    }

    // What is on the bus. Reading only: no BAR is written, no device is
    // configured (docs/adr/0022-fase4-pci-enumeration.md).
    // SAFETY: one core, and nothing else in this kernel uses the two
    // configuration ports.
    let devices = unsafe { harlan_arch_x86_64::pci::scan() };
    for found in devices.iter() {
        info!(
            "HARLAN: pci {:02x}:{:02x}.{} {:04x}:{:04x} {} (class {:02x}.{:02x})",
            found.at.bus,
            found.at.device,
            found.at.function,
            found.header.vendor,
            found.header.device,
            found.header.class_name(),
            found.header.class,
            found.header.subclass
        );
    }
    if devices.lost() > 0 {
        warn!(
            "HARLAN: {} more function(s) on the bus than the kernel keeps ({})",
            devices.lost(),
            harlan_arch_x86_64::pci::KEPT_AT_MOST
        );
    }
    // The one Fase 4 is going to learn to talk to. Named by vendor, not
    // by class: the machine also has an emulated IDE controller, and
    // "the first storage device" would be whichever the scan met first.
    const VIRTIO_VENDOR: u16 = 0x1AF4;
    const MASS_STORAGE: u8 = 0x01;
    match devices
        .iter()
        .find(|f| f.header.vendor == VIRTIO_VENDOR && f.header.class == MASS_STORAGE)
    {
        Some(disk) => {
            info!(
                "HARLAN: a virtio disk at pci {:02x}:{:02x}.{} ({:04x}:{:04x})",
                disk.at.bus,
                disk.at.device,
                disk.at.function,
                disk.header.vendor,
                disk.header.device
            );
            // Its registers, as the firmware left them. A 64-bit BAR takes
            // two of the six entries, so the walk steps over the half it
            // has already read rather than reading it as a BAR of its own.
            let mut index = 0;
            while index < 6 {
                match harlan_hal::pci::decode_bar(&disk.header.bars, index) {
                    Some(bar) => {
                        info!("HARLAN:   bar {index}: {bar}");
                        index += bar.entries();
                    }
                    None => index += 1,
                }
            }
        }
        None => warn!("HARLAN: no virtio storage on the bus"),
    }

    // And the disk, if the kernel owns its tables: its registers have to
    // be mapped, which is only the kernel's to do once the firmware's
    // identity map is gone (docs/adr/0023-fase4-device-registers.md).
    let disk = if own_tables {
        // SAFETY: the kernel owns its tables and reaches frames through
        // its own window, the scan above is of this machine's bus, and
        // nothing else drives this device.
        match unsafe {
            devices::virtio_blk::start(&mut context.mapper, &mut context.frames, &devices)
        } {
            Ok(disk) => {
                info!(
                    "HARLAN: the disk at pci {:02x}:{:02x}.{} is negotiated: registers from bar {} at {:#x}, offers {:#x}, agreed {:#x}, {} queue(s), queue 0 holds {} descriptor(s) and is notified at {}",
                    disk.at.bus,
                    disk.at.device,
                    disk.at.function,
                    disk.bar,
                    disk.registers,
                    disk.offered,
                    disk.accepted,
                    disk.queues,
                    disk.queue_size,
                    disk.notify_offset
                );
                Some(disk)
            }
            Err(err) => {
                error!("HARLAN: the disk could not be started ({err:?})");
                None
            }
        }
    } else {
        None
    };

    // And one sector off it, which is the whole point of the phase
    // (docs/adr/0024-fase4-dma-and-the-queue.md).
    // The reader outlives this block: the program the kernel runs comes
    // off the same disk, further down
    // (docs/adr/0026-fase4-elf-user-programs.md).
    if let Some(disk) = disk {
        // SAFETY: the disk is negotiated and not started, the kernel owns
        // its tables, and `frames` reaches frames through its window.
        match unsafe {
            devices::virtio_blk::start_queue(&disk, &mut context.mapper, &mut context.frames)
        } {
            Ok(mut reader) => {
                // The boot sector, and the copy FAT32 keeps at sector 6.
                // Reading both checks three things at once: that the BPB
                // parses, that the sector number reaches the device — six
                // is not zero — and that the image really has its backup
                // where it belongs
                // (docs/adr/0025-fase4-fat32-read-only.md).
                let mut boot_sector = [0u8; 512];
                // SAFETY: the reader owns its queue and its request frame,
                // and nothing else has a request in flight.
                let first = unsafe { reader.read_sector(&disk, 0, &mut boot_sector) };
                match first.map(|()| harlan_hal::fat::BootSector::parse(&boot_sector)) {
                    Ok(Ok(volume)) => {
                        info!(
                            "HARLAN: the disk holds FAT32: {} sector(s) of {} byte(s), {} cluster(s) of {} sector(s), {} table(s) of {} sector(s) from sector {}, root at cluster {} (sector {:?}), data from sector {}",
                            volume.total_sectors,
                            volume.bytes_per_sector,
                            volume.clusters,
                            volume.sectors_per_cluster,
                            volume.fat_count,
                            volume.sectors_per_fat,
                            volume.first_fat_sector(),
                            volume.root_cluster,
                            volume.sector_of_cluster(volume.root_cluster),
                            volume.first_data_sector()
                        );
                        // From here the disk belongs to `fs`, where a
                        // syscall can reach it
                        // (docs/adr/0028-fase4-file-abi-v0.md). The boot
                        // reads the rest of its files through the same
                        // path a program's `open` takes.
                        fs::adopt(disk, reader, volume);
                        events::record(events::What::Disk, u64::from(volume.clusters));
                        check_backup_boot_sector(&volume, &boot_sector);
                        read_a_file();
                        count_this_boot();
                    }
                    Ok(Err(err)) => {
                        events::record(events::What::NoDisk, 0);
                        error!("HARLAN: the disk does not hold a FAT32 volume ({err:?})")
                    }
                    Err(err) => error!("HARLAN: the boot sector could not be read ({err:?})"),
                }
            }
            Err(err) => error!("HARLAN: the disk's queue could not be started ({err:?})"),
        }
    }

    // Soak builds (`cargo xtask soak-test`) never reach the shell: they run
    // heap stress rounds until QEMU is stopped.
    if cfg!(feature = "soak") {
        memory::heap::soak();
    }

    // Ring 3, if the lower half is the kernel's to map into and there is
    // a stack for syscalls to land on (ADR 0014). The program says hello
    // and exits; the kernel carries on in `into_the_shell`, on that same
    // syscall stack.
    if let (true, Some((_, _, syscall_stack))) = (own_tables, context.kernel_stacks) {
        // SAFETY: the kernel is where it means to stay, and this stack is
        // its own, guarded, and used by nothing else.
        unsafe { harlan_arch_x86_64::syscall::init(syscall_stack.top().as_u64()) };
        // Where the CPU lands when it takes an interrupt while ring 3 is
        // running. Same stack: a syscall cannot be interrupted (`FMASK`
        // clears `IF`), so the two never use it at once.
        // SAFETY: as above.
        unsafe { harlan_arch_x86_64::set_kernel_stack(syscall_stack.top()) };
        // The program that comes off the disk, if there is a disk. Read
        // before anything is started, because one of the processes is it
        // (docs/adr/0026-fase4-elf-user-programs.md).
        let mut elf_bytes = alloc::vec![0u8; 64 * 1024];
        let elf = load_program(USER_PROGRAM, &mut elf_bytes);
        // And the shell, which is the one that does not end
        // (docs/adr/0030-fase4-shell-in-ring-3.md). Its own buffer: both
        // are loaded before anything is started, so they cannot share one.
        let mut shell_bytes = alloc::vec![0u8; 64 * 1024];
        let shell_elf = load_program(SHELL_PROGRAM, &mut shell_bytes);

        // Eight processes, each with a space of its own and a kernel
        // stack of its own.
        //
        // Four do their work: two say who they are and take turns (ADR
        // 0017 and ADR 0018), and two exchange a message, which is what
        // Fase 3 set out to show (ADR 0019). The receiver is spawned
        // **before** the sender on purpose: round robin reaches it first,
        // it finds an empty mailbox and parks, and the sender is what
        // wakes it. The other order would never wait.
        //
        // The other four try what must not work (ADR 0020). What they
        // demonstrate is not that each attempt fails, but that the four
        // above finish their work afterwards, on a machine that is still
        // running.
        const RECEIVER_SLOT: u8 = 2;
        // Kernel memory, named so that the trespassers can aim at it. The
        // address is the kernel's own code: mapped, with something in it,
        // and a fault away from ring 3.
        let kernel_address = user::handle as *const () as u64;
        match harlan_hal::paging::PageMapper::translate(
            &context.mapper,
            harlan_hal::addr::VirtAddr::new(kernel_address),
        ) {
            Some(frame) => info!(
                "HARLAN: {kernel_address:#x} is kernel code, mapped at {frame}; two processes are about to try to reach it"
            ),
            None => error!(
                "HARLAN: {kernel_address:#x} is not mapped, so trying to read it would prove nothing"
            ),
        }
        let talking_one = user::talker_program(b'1');
        let talking_two = user::talker_program(b'2');
        let receiving = user::receiver_program();
        let sending = user::sender_program(RECEIVER_SLOT);
        let reading_the_kernel = user::reads_kernel_memory(kernel_address);
        let writing_its_code = user::writes_its_own_code();
        let running_its_stack = user::runs_its_own_stack();
        let lying = user::lies_about_a_pointer(kernel_address);
        // The eight written by hand, and then the one that was compiled.
        // The order matters: the sender was built naming a slot, so the
        // new one goes last (docs/adr/0026-fase4-elf-user-programs.md).
        let mut programs: alloc::vec::Vec<ToStart> = alloc::vec![
            ToStart::Flat(&talking_one),
            ToStart::Flat(&talking_two),
            ToStart::Flat(&receiving),
            ToStart::Flat(&sending),
            ToStart::Flat(&reading_the_kernel),
            ToStart::Flat(&writing_its_code),
            ToStart::Flat(&running_its_stack),
            ToStart::Flat(&lying),
        ];
        if let Some((program, file)) = &elf {
            programs.push(ToStart::Elf(program, file));
        }
        // The shell is **not** in this list. It does not end, so a round
        // containing it never finishes, and the frame balance below — the
        // check that says the processes which ended gave back everything
        // they took — would stop being measured on every boot. It starts
        // afterwards, in a round of its own.

        // What the allocator has before any process exists. Everything
        // taken from here on belongs to a process, and once they are all
        // gone the number has to come back
        // (docs/adr/0021-fase3-reclaiming-a-dead-space.md).
        let free_before_any_process = context.frames.free_frames();
        let mut next_stack = syscall_stack.top();
        let mut started = 0;
        let mut first_entry = None;
        for (which, program) in programs.iter().enumerate() {
            let stack = memory::stacks::map_with_guard(
                &mut context.mapper,
                &mut context.frames,
                harlan_hal::paging::Page::containing_address(next_stack),
                memory::stacks::SYSCALL_STACK_PAGES,
            );
            let Ok(stack) = stack else {
                error!("HARLAN: no kernel stack for process {which}");
                break;
            };
            next_stack = stack.top();
            // SAFETY: the kernel owns its tables and reaches frames
            // through its own window; nothing else uses what this takes.
            let spawned = unsafe {
                match program {
                    ToStart::Flat(bytes) => {
                        process::spawn(&mut context.mapper, &mut context.frames, bytes, stack)
                    }
                    ToStart::Elf(program, file) => process::spawn_elf(
                        &mut context.mapper,
                        &mut context.frames,
                        program,
                        file,
                        stack,
                    ),
                }
            };
            match spawned {
                Ok(process) => {
                    let entry = process.entry();
                    let here = process.space.translate(entry);
                    match (first_entry, here) {
                        (None, _) => first_entry = here,
                        (Some(before), Some(now)) if before != now => info!(
                            "HARLAN: {entry:#x} is {before} in one process and {now} in another: different memory, same address"
                        ),
                        (before, now) => error!(
                            "HARLAN: the processes do not have separate memory at {entry:#x} ({before:?}, {now:?})"
                        ),
                    }
                    let process = alloc::boxed::Box::leak(alloc::boxed::Box::new(process));
                    // SAFETY: `spawn` built it, and its kernel stack is
                    // its own.
                    match unsafe { scheduler::add(process) } {
                        // The sender was built naming a slot, so a
                        // process that lands somewhere else would be
                        // sending to a stranger.
                        Some(slot) if slot == which => {
                            info!("HARLAN: process {which} runs in slot {slot}");
                            events::record(events::What::Started, slot as u64);
                            started += 1;
                        }
                        Some(slot) => error!(
                            "HARLAN: process {which} landed in slot {slot}, not the one it was built for"
                        ),
                        None => error!("HARLAN: no room in the scheduler for process {which}"),
                    }
                }
                Err(err) => error!("HARLAN: process {which} could not be started ({err:?})"),
            }
        }

        if started > 0 {
            // The timer gives the CPU away too, not just `yield`.
            harlan_arch_x86_64::interrupts::set_tick_handler(scheduler::on_tick);
            // And a process that faults ends there, rather than taking the
            // machine with it (ADR 0020). Until this is set, a fault in
            // ring 3 stops the CPU like one in the kernel.
            harlan_arch_x86_64::interrupts::set_user_fault_handler(user::on_fault);
            // SAFETY: the kernel is in its own space, on its own stack,
            // and no process is running yet.
            unsafe { scheduler::run_until_empty(context.mapper.root()) };
            // With the CPU back, what the dead were using can go.
            let mut returned = 0;
            // SAFETY: nothing is running on them.
            for dead in unsafe { scheduler::dead_processes() } {
                // SAFETY: the process is gone and nothing is on its
                // stack: this runs on the kernel's own.
                returned +=
                    unsafe { process::destroy(&mut context.mapper, &mut context.frames, dead) };
            }
            let free_now = context.frames.free_frames();
            match free_now.cmp(&free_before_any_process) {
                core::cmp::Ordering::Equal => {
                    events::record(events::What::Frames, returned);
                    info!(
                        "HARLAN: {returned} frame(s) back from the processes that ended; the allocator has the {free_now} it started with"
                    )
                }
                core::cmp::Ordering::Less => error!(
                    "HARLAN: {returned} frame(s) back from the processes that ended, but {} are still held; {free_now} free",
                    free_before_any_process - free_now
                ),
                core::cmp::Ordering::Greater => error!(
                    "HARLAN: {returned} frame(s) back from the processes that ended, which is {} more than they ever took; {free_now} free",
                    free_now - free_before_any_process
                ),
            }
        }

        // The self-test round is over and accounted for, so it is written
        // down (ADR 0031, point 8). Here and not inside the branch below:
        // a disk with no `SHELL.ELF` on it still has a disk, and losing a
        // boot's events because the shell was missing would lose exactly
        // the boot somebody is asking about.
        write_events();

        // And now the shell, in a round of its own, because this one does
        // not end (docs/adr/0030-fase4-shell-in-ring-3.md). Everything
        // above has finished and been accounted for; from here the machine
        // belongs to whoever is typing.
        if let Some((program, file)) = &shell_elf {
            // A kernel stack of its own, the next one after the ones the
            // demonstrations used — they are gone, but their stacks were
            // mapped and this is simpler than reusing one.
            let stack = memory::stacks::map_with_guard(
                &mut context.mapper,
                &mut context.frames,
                harlan_hal::paging::Page::containing_address(next_stack),
                memory::stacks::SYSCALL_STACK_PAGES,
            );
            let shell_stack = match stack {
                Ok(stack) => stack,
                Err(err) => {
                    error!("HARLAN: no kernel stack for the shell ({err:?})");
                    // Never returns: the kernel's own shell takes over.
                    into_the_shell::<C, P>((context as *mut KernelContext<C, P>).cast())
                }
            };
            // SAFETY: the kernel owns its tables and reaches frames through
            // its own window; nothing else uses what this takes.
            match unsafe {
                process::spawn_elf(
                    &mut context.mapper,
                    &mut context.frames,
                    program,
                    file,
                    shell_stack,
                )
            } {
                Ok(shell) => {
                    let shell = alloc::boxed::Box::leak(alloc::boxed::Box::new(shell));
                    // SAFETY: `spawn_elf` built it, and its kernel stack is
                    // its own.
                    match unsafe { scheduler::add(shell) } {
                        Some(slot) => {
                            info!("HARLAN: the shell runs in slot {slot}");
                            events::record(events::What::Shell, slot as u64);
                            // Again, because the round below does not come
                            // back while the shell lives: without this the
                            // line saying a shell started would sit in the
                            // ring until the machine stopped, which is the
                            // one moment it cannot be written.
                            write_events();
                            harlan_arch_x86_64::interrupts::set_tick_handler(scheduler::on_tick);
                            harlan_arch_x86_64::interrupts::set_user_fault_handler(user::on_fault);
                            // SAFETY: the kernel is in its own space, on
                            // its own stack, and nothing is running.
                            //
                            // This does not come back while the shell is
                            // alive. It does if the shell exits, and then
                            // the kernel's own takes over below — which is
                            // what `exit` at the prompt is for.
                            unsafe { scheduler::run_until_empty(context.mapper.root()) };
                            // SAFETY: nothing is running on them.
                            for dead in unsafe { scheduler::dead_processes() } {
                                // SAFETY: the process is gone and nothing
                                // is on its stack.
                                unsafe {
                                    process::destroy(&mut context.mapper, &mut context.frames, dead)
                                };
                            }
                            events::record(events::What::ShellGone, 0);
                            write_events();
                            info!(
                                "HARLAN: the shell in ring 3 is gone; the kernel's own takes over"
                            );
                        }
                        None => error!("HARLAN: no room in the scheduler for the shell"),
                    }
                }
                Err(err) => error!("HARLAN: the shell could not be started ({err:?})"),
            }
        }
    }

    into_the_shell::<C, P>((context as *mut KernelContext<C, P>).cast())
}

/// Where the kernel goes once the program has exited — or straight away,
/// if there was none to run.
#[cfg(target_arch = "x86_64")]
extern "C" fn into_the_shell<C: Console + 'static, P: PowerControl + 'static>(
    context: *mut u8,
) -> ! {
    use harlan_hal::InterruptControl;

    // SAFETY: the same context `run` was given; nothing else refers to it.
    let context = unsafe { &mut *context.cast::<KernelContext<C, P>>() };
    // Out of the process's space and back into the kernel's own, so that
    // nothing of a program that has exited is mapped any more.
    // SAFETY: this code, its stack and its heap are in the higher half,
    // which every space shares, and nothing here points into the lower
    // half of the space being left.
    unsafe { context.mapper.activate() };
    // A syscall runs with interrupts off (`FMASK`), and the shell needs
    // the keyboard.
    harlan_arch_x86_64::Cpu.enable();
    // Taken rather than borrowed for each line: the shell loops for ever,
    // and holding the lock across that loop would leave interrupts disabled
    // for ever — the keyboard IRQ would never fire and the shell would
    // never get a key.
    //
    // One owner at a time: from here on a syscall finds no console, which is
    // right, because this runs only when nothing is runnable and a syscall
    // can only come from a process that is running.
    let Some(console) = console::take() else {
        // `run` adopts it and nothing else takes it, so this cannot happen.
        // If it ever does, the kernel has no shell, and saying so is better
        // than carrying on as if it had one.
        error!("HARLAN: there is no console to run a shell on");
        loop {
            use harlan_hal::CpuControl;
            harlan_arch_x86_64::Cpu.halt_once();
        }
    };
    // SAFETY: `take` handed over the only way to the console, so this is the
    // only reference to it; it points at heap the kernel leaked, which
    // outlives this function (which never returns).
    let console = unsafe { &mut *console };
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

/// Counts this boot in a file on the disk, and says what the file said
/// before.
///
/// This is the phase's exit criterion in one function: a file created on
/// one boot, read on the next, with nothing between them but the disk
/// (docs/adr/0027-fase4-fat32-write.md). The number only grows because
/// what was written survived; a disk that forgot would start again at one
/// every time, and so would a reader that could not find what it wrote.
fn count_this_boot() {
    const NAME: &str = "BOOTS.TXT";

    // What the last boot left, if there was one.
    let before = match fs::find(NAME) {
        Ok(entry) => {
            let mut bytes = [0u8; 32];
            match fs::read_file(&entry, &mut bytes) {
                Ok(read) => core::str::from_utf8(&bytes[..read])
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok()),
                Err(err) => {
                    error!("HARLAN: {NAME} is there and could not be read ({err:?})");
                    None
                }
            }
        }
        Err(fs::FileError::NoSuchFile) => None,
        Err(err) => {
            error!("HARLAN: {NAME} could not be looked up ({err:?})");
            return;
        }
    };

    let this = before.unwrap_or(0) + 1;
    // Up to ten digits and a newline, which is more than a `u32` needs.
    let mut text = [0u8; 11];
    let written = write_number(&mut text, this);
    match fs::write_file(NAME, &text[..written]) {
        Ok(entry) => {
            // The boot number is what stands in for a date: without a wall
            // clock, "boot 7" is the closest thing to *when* this machine
            // can say (ADR 0031, point 10).
            events::this_boot(this);
            events::record(events::What::Boot, u64::from(this));
            match before {
                Some(before) => info!(
                    "HARLAN: this is boot {this}; {NAME} said {before} and now says {this}, in {} byte(s) from cluster {}",
                    entry.size, entry.first_cluster
                ),
                None => info!(
                    "HARLAN: this is boot {this}; {NAME} was not there and now says {this}, in {} byte(s) from cluster {}",
                    entry.size, entry.first_cluster
                ),
            }
        }
        Err(err) => error!("HARLAN: {NAME} could not be written ({err:?})"),
    }
}

/// Writes the events of this boot onto what the disk already held.
///
/// Read, join, write whole: the only shape ADR 0027 allows, and the one
/// that makes the file the history of the machine rather than of the last
/// boot (ADR 0031, point 6).
///
/// Everything here is on the stack of a kernel that has a 4 MB heap, so the
/// two buffers are heap-allocated: 32 KB on a kernel stack with a guard
/// page either side would be most of it.
fn write_events() {
    const NAME: &str = "EVENTS.LOG";

    // What this boot has to say.
    let mut mine = alloc::vec![0u8; events::EVENTS * events::LINE];
    let mine_len = events::copy_into(&mut mine);

    // What was already there. A missing file is not a failure: the first
    // boot of a fresh disk has nothing to add to.
    let mut old = alloc::vec![0u8; events::MAX_FILE];
    let old_len = match fs::find(NAME) {
        Ok(entry) => match fs::read_file(&entry, &mut old) {
            Ok(read) => read,
            Err(err) => {
                error!("HARLAN: {NAME} is there and could not be read ({err:?})");
                0
            }
        },
        Err(fs::FileError::NoSuchFile) => 0,
        Err(err) => {
            error!("HARLAN: {NAME} could not be looked up ({err:?})");
            return;
        }
    };

    let mut joined = alloc::vec![0u8; events::MAX_FILE];
    let len = events::join(
        &old[..old_len],
        &mine[..mine_len],
        events::MAX_FILE,
        &mut joined,
    );
    match fs::write_file(NAME, &joined[..len]) {
        Ok(entry) => {
            let (kept, lost) = events::counted();
            if lost > 0 {
                warn!(
                    "HARLAN: {NAME} is {} byte(s); {kept} event(s) recorded this boot and {lost} lost to the ring before they could be written",
                    entry.size
                );
            } else {
                // `mine_len` and not the count of events this boot: a
                // second flush writes only what the first left, and saying
                // otherwise would be the log describing itself wrongly.
                info!(
                    "HARLAN: {NAME} is {} byte(s); {mine_len} new byte(s) appended, {kept} event(s) recorded this boot",
                    entry.size
                );
            }
        }
        Err(err) => error!("HARLAN: {NAME} could not be written ({err:?})"),
    }
}

/// Writes `number` into `text` as decimal with a newline, and answers how
/// many bytes that took. There is no formatter in a `no_std` kernel that
/// writes into a buffer without allocating, and this is four lines.
fn write_number(text: &mut [u8; 11], number: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut left = number;
    loop {
        digits[count] = b'0' + (left % 10) as u8;
        count += 1;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    for (at, digit) in digits[..count].iter().rev().enumerate() {
        text[at] = *digit;
    }
    text[count] = b'\n';
    count + 1
}

/// How a process is to be started: from a page of bytes written by hand,
/// or from a program that was compiled and read off the disk.
///
/// Both still exist because they demonstrate different things. A compiler
/// does not produce a program that writes to its own code (ADR 0020), and
/// a program written by hand in hexadecimal is not how a shell gets
/// written (ADR 0026).
enum ToStart<'a> {
    Flat(&'a [u8]),
    Elf(&'a harlan_hal::elf::Program, &'a [u8]),
}

/// The name of the program that demonstrates the ABI, and of the shell.
const USER_PROGRAM: &str = "HELLO.ELF";
const SHELL_PROGRAM: &str = "SHELL.ELF";

/// Reads a program off the disk into `into` and parses it.
///
/// One function rather than a block per program: the second copy of a
/// sequence of checks is where one of them gets left out.
///
/// `None`, with a line saying why, for a disk that is not there, a file
/// that is not there, one too big for the buffer, or one that is not a
/// program this kernel loads. A kernel that could not load a program
/// still boots; it just has one process fewer.
fn load_program<'a>(
    name: &str,
    into: &'a mut [u8],
) -> Option<(harlan_hal::elf::Program, &'a [u8])> {
    let read = read_program(name, into)?;
    match harlan_hal::elf::parse(
        &into[..read],
        harlan_arch_x86_64::paging::KERNEL_SPACE_START.as_u64(),
    ) {
        Ok(program) => {
            info!(
                "HARLAN: {name} is an ELF with {} loadable segment(s), entry {:#x}, reaching {:#x}",
                program.segment_count(),
                program.entry,
                program.highest_address()
            );
            for segment in program.segments() {
                info!(
                    "HARLAN:   segment at {:#x}, {} byte(s) of file and {} of memory, {}{}{}",
                    segment.at,
                    segment.file_size,
                    segment.memory_size,
                    if segment.readable() { "r" } else { "-" },
                    if segment.writable() { "w" } else { "-" },
                    if segment.executable() { "x" } else { "-" }
                );
            }
            events::record_with(events::What::Loaded, read as u64, name);
            Some((program, &into[..read]))
        }
        Err(err) => {
            error!("HARLAN: {name} is not a program this kernel loads ({err:?})");
            None
        }
    }
}

/// Reads the user program off the disk, as bytes.
///
/// Answers `None` and says why if it is not there or cannot be read: a
/// kernel whose disk failed has no user program, and that has to be a line
/// in the log rather than a boot that carries on as if it had one.
fn read_program(name: &str, into: &mut [u8]) -> Option<usize> {
    let entry = match fs::find(name) {
        Ok(entry) => entry,
        Err(err) => {
            error!("HARLAN: {name} cannot be loaded ({err:?})");
            return None;
        }
    };
    match fs::read_file(&entry, into) {
        Ok(read) => {
            info!("HARLAN: {name} is {read} byte(s), read off the disk");
            Some(read)
        }
        Err(err) => {
            error!("HARLAN: {name} could not be read ({err:?})");
            None
        }
    }
}

/// Lists the root directory and reads one file off it, which is as far as
/// a filesystem has to work before a shell can use it.
fn read_a_file() {
    let mut files = 0;
    if let Err(err) = fs::each_name(|entry| {
        files += 1;
        info!(
            "HARLAN:   {} — {} byte(s){}, from cluster {}",
            entry.name(),
            entry.size,
            if entry.is_directory() {
                ", a directory"
            } else {
                ""
            },
            entry.first_cluster
        );
        true
    }) {
        error!("HARLAN: the root directory could not be read ({err:?})");
        return;
    }
    info!("HARLAN: {files} thing(s) in the root directory");

    // One of them, read through its chain and checked against what xtask
    // put there. "It read something" and "it read the right thing" are
    // different answers.
    const NAME: &str = "HELLO.TXT";
    const EXPECTED: &str = "HARLAN reads its own disk.\n";
    let entry = match fs::find(NAME) {
        Ok(entry) => entry,
        Err(err) => {
            error!("HARLAN: {NAME} could not be looked up ({err:?})");
            return;
        }
    };
    let mut bytes = [0u8; 512];
    match fs::read_file(&entry, &mut bytes) {
        Ok(read) => {
            let text = core::str::from_utf8(&bytes[..read]).unwrap_or("not text");
            if text == EXPECTED {
                info!(
                    "HARLAN: {NAME} is {read} byte(s) and reads {:?}, which is what is in it",
                    text.trim_end()
                );
            } else {
                error!("HARLAN: {NAME} reads {text:?}, and {EXPECTED:?} is what is in it");
            }
        }
        Err(err) => error!("HARLAN: {NAME} could not be read ({err:?})"),
    }

    // And a longer one, to show the chain really is followed: four
    // clusters and a byte, so a reader that stopped at a cluster boundary
    // or ran past the file's length would not match.
    const LONG: &str = "LONG.BIN";
    match fs::find(LONG) {
        Ok(entry) => {
            let mut bytes = [0u8; 4096];
            match fs::read_file(&entry, &mut bytes) {
                Ok(read) => {
                    let right = read == entry.size as usize
                        && bytes[..read]
                            .iter()
                            .enumerate()
                            .all(|(at, byte)| *byte == (at % 251) as u8);
                    if right {
                        let per_cluster =
                            fs::boot_sector().map_or(1, |boot| boot.cluster_bytes() as usize);
                        info!(
                            "HARLAN: {LONG} is {read} byte(s) across {} cluster(s), every one of them what it should be",
                            read.div_ceil(per_cluster)
                        );
                    } else {
                        error!("HARLAN: {LONG} read {read} byte(s) and they are not what is in it");
                    }
                }
                Err(err) => error!("HARLAN: {LONG} could not be read ({err:?})"),
            }
        }
        Err(err) => error!("HARLAN: {LONG} could not be looked up ({err:?})"),
    }
}

/// Reads the copy of the boot sector FAT32 keeps further in, and checks it
/// against the one already read.
///
/// A volume where the two differ is one where something has been written
/// to by two readers that disagree; saying so is cheap, and it is also the
/// only check in this boot that the sector number really travels — the
/// backup is at sector 6, and a driver that always read sector 0 would be
/// comparing a sector with itself.
fn check_backup_boot_sector(volume: &harlan_hal::fat::BootSector, first: &[u8; 512]) {
    let backup = u32::from(volume.backup_boot_sector);
    if backup == 0 {
        warn!("HARLAN: this volume keeps no backup boot sector");
        return;
    }
    let mut copy = [0u8; 512];
    match fs::read_sector(backup, &mut copy) {
        Ok(()) if copy == *first => {
            info!("HARLAN: sector {backup} holds the same boot sector as sector 0, byte for byte")
        }
        Ok(()) => error!("HARLAN: sector {backup} should be a copy of the boot sector and is not"),
        Err(err) => error!("HARLAN: the backup boot sector could not be read ({err:?})"),
    }
}
