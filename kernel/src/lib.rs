#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod identity;
mod memory;
mod shell;
mod sync;

use harlan_hal::frame::FRAME_SIZE;
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
}

pub fn kmain(boot_info: &BootInfo, console: &mut dyn Console, power: &dyn PowerControl) -> ! {
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
    let mut frames = BitmapFrameAllocator::new(&mut frame_bitmap, &boot_info.memory_map);
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
    memory::self_test(&mut frames, core::ptr::addr_of!(stack_probe) as u64);

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
        if heap_bytes > 0 {
            log::info!(
                "HARLAN: heap = {} KiB at {heap_start:#x}; {} frame(s) left",
                heap_bytes / 1024,
                frames.free_frames()
            );
            memory::heap::self_test(heap_start, heap_bytes);
        } else {
            log::error!("HARLAN: no kernel heap: every allocation will panic");
        }
    }

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

    shell::run_shell(console, power)
}
