//! What this machine turned out to be
//! (docs/adr/0035-fase5-hardware-inventory.md).
//!
//! Fase 5 asks for an *inventario de hardware del equipo objetivo*, and
//! that cannot be written from here: nobody on this side of the screen has
//! seen the machine. What can be written is the thing that finds out.
//!
//! It runs in the bootloader, under UEFI, and that is the point: the
//! firmware's file services are still there, so the inventory can be
//! **written back onto the stick it booted from**. The person plugs the
//! stick into an ordinary computer afterwards and reads a text file,
//! instead of photographing a screen and transcribing it.
//!
//! Nothing here allocates. The kernel's heap is not initialised during the
//! boot phase — it is the only allocator linked in — so a `String` would
//! be a null pointer. Everything goes into one fixed buffer.

use core::fmt::{self, Write};

use uefi::boot;
use uefi::cstr16;
use uefi::mem::memory_map::MemoryMap;
use uefi::proto::media::file::{File, FileAttribute, FileMode, FileType};

/// Where it goes on the stick.
///
/// In the root of the EFI System Partition, beside the `EFI` directory,
/// because that is the one place a person can find without being told
/// where to look.
const FILE: &uefi::CStr16 = cstr16!("INVENTORY.TXT");

/// How much the report may be.
///
/// A machine with forty PCI functions writes about four kilobytes; this is
/// generous and still small enough to be a static rather than a stack
/// frame, which matters because the firmware's stack is not ours to spend.
const ROOM: usize = 16 * 1024;

/// The report, built once.
///
/// A `static mut` read and written from one place, before anything else
/// runs and on one core, which is the whole of its safety argument.
static mut TEXT: Report = Report::new();

/// Text in a fixed buffer.
///
/// Truncates rather than growing or panicking: a report cut short still
/// answers most of what it was asked, and a panic in the bootloader answers
/// nothing at all.
struct Report {
    bytes: [u8; ROOM],
    len: usize,
    /// Whether anything was dropped, so the report can say so about itself
    /// rather than ending mid-sentence and leaving the reader to wonder.
    truncated: bool,
}

impl Report {
    const fn new() -> Self {
        Self {
            bytes: [0; ROOM],
            len: 0,
            truncated: false,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for Report {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = self.bytes.len() - self.len;
        let taking = text.len().min(room);
        if taking < text.len() {
            self.truncated = true;
        }
        self.bytes[self.len..self.len + taking].copy_from_slice(&text.as_bytes()[..taking]);
        self.len += taking;
        Ok(())
    }
}

/// Gathers everything, writes it to the stick, and answers the report so
/// the caller can also put it on the screen and in the log.
///
/// # Safety
///
/// Called once, from the bootloader, while Boot Services are still up.
pub unsafe fn take(
    framebuffer: Option<&harlan_hal::framebuffer::FramebufferInfo>,
) -> &'static [u8] {
    // SAFETY: one caller, once, before anything else runs, on one core.
    let report = unsafe { &mut *&raw mut TEXT };
    gather(report, framebuffer);
    write_to_the_stick(report.as_bytes());
    report.as_bytes()
}

fn gather(out: &mut Report, framebuffer: Option<&harlan_hal::framebuffer::FramebufferInfo>) {
    let _ = writeln!(out, "HARLAN OS hardware inventory");
    let _ = writeln!(out, "============================");
    let _ = writeln!(out);

    firmware(out);
    processor(out);
    memory(out);
    display(out, framebuffer);
    bus(out);

    let _ = writeln!(out);
    if out.truncated {
        let _ = writeln!(
            out,
            "-- the report filled its {ROOM} bytes and was cut short here --"
        );
    } else {
        let _ = writeln!(out, "-- end of report --");
    }
}

fn firmware(out: &mut Report) {
    let _ = writeln!(out, "[firmware]");
    let revision = uefi::system::uefi_revision();
    let _ = writeln!(out, "  uefi      {}.{}", revision.major(), revision.minor());
    let vendor = uefi::system::firmware_vendor();
    let _ = write!(out, "  vendor    ");
    // `CStr16` is UTF-16; written a character at a time because there is no
    // allocator to build a `String` with.
    for unit in vendor.iter() {
        let _ = out.write_char(char::from(*unit));
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  firmware  {:#010x}",
        uefi::system::firmware_revision()
    );
    let _ = writeln!(out);
}

fn processor(out: &mut Report) {
    use harlan_arch_x86_64::cpuid;

    let _ = writeln!(out, "[processor]");
    let vendor = cpuid::vendor();
    let _ = write!(out, "  vendor    ");
    let _ = out.write_str(core::str::from_utf8(&vendor).unwrap_or("?"));
    let _ = writeln!(out);

    let brand = cpuid::brand();
    let name = core::str::from_utf8(&brand)
        .unwrap_or("")
        .trim_matches(|c: char| c == '\0' || c == ' ');
    if !name.is_empty() {
        let _ = writeln!(out, "  name      {name}");
    }

    let signature = cpuid::signature();
    let _ = writeln!(
        out,
        "  family {} model {:#x} stepping {}",
        signature.family, signature.model, signature.stepping
    );

    let (physical, virtual_bits) = cpuid::address_bits();
    let _ = writeln!(
        out,
        "  address   {physical} physical bit(s), {virtual_bits} virtual"
    );

    let f = cpuid::features();
    let _ = writeln!(
        out,
        "  features  nx={} 1g-pages={} syscall={} long-mode={}",
        yes(f.nx),
        yes(f.gigabyte_pages),
        yes(f.syscall),
        yes(f.long_mode)
    );
    let _ = writeln!(
        out,
        "            apic={} x2apic={} invariant-tsc={} hypervisor={}",
        yes(f.apic),
        yes(f.x2apic),
        yes(f.invariant_tsc),
        yes(f.hypervisor)
    );
    let _ = writeln!(
        out,
        "  leaves    max {:#x}, max extended {:#x}",
        cpuid::max_leaf(),
        cpuid::max_extended_leaf()
    );
    let _ = writeln!(out);
}

const fn yes(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

fn memory(out: &mut Report) {
    let _ = writeln!(out, "[memory]");
    let Ok(map) = boot::memory_map(boot::MemoryType::LOADER_DATA) else {
        let _ = writeln!(out, "  the firmware would not give its memory map");
        let _ = writeln!(out);
        return;
    };

    // Every type the firmware names, with what it adds up to.
    //
    // Two totals were tried first and both were misleading: a 512 MiB
    // machine came out with "twelve gigabytes reserved", which is the kind
    // of number that sends somebody looking for a fault that is not there.
    // Guessing which types are memory is the mistake — the firmware knows
    // what it means and the reader can see it, so this prints what it said.
    const KINDS: usize = 20;
    let mut kinds: [(u32, u64, usize); KINDS] = [(0, 0, 0); KINDS];
    let mut used = 0usize;
    let mut usable = 0u64;
    let mut largest = 0u64;
    let mut regions = 0usize;
    for entry in map.entries() {
        regions += 1;
        let bytes = entry.page_count * 4096;
        if matches!(
            entry.ty,
            boot::MemoryType::CONVENTIONAL
                | boot::MemoryType::BOOT_SERVICES_CODE
                | boot::MemoryType::BOOT_SERVICES_DATA
        ) {
            usable += bytes;
            largest = largest.max(bytes);
        }
        let kind = entry.ty.0;
        match kinds[..used].iter().position(|(ty, _, _)| *ty == kind) {
            Some(at) => {
                kinds[at].1 += bytes;
                kinds[at].2 += 1;
            }
            None if used < KINDS => {
                kinds[used] = (kind, bytes, 1);
                used += 1;
            }
            // More kinds than this holds. Said, not dropped quietly.
            None => {}
        }
    }
    let _ = writeln!(out, "  regions   {regions}");
    let _ = writeln!(
        out,
        "  usable    {} MiB, largest piece {} MiB",
        usable / (1024 * 1024),
        largest / (1024 * 1024)
    );
    let _ = writeln!(out, "  by kind (as the firmware names them):");
    for (kind, bytes, count) in &kinds[..used] {
        let _ = writeln!(
            out,
            "    {:<22} {:>8} MiB in {} region(s)",
            kind_name(*kind),
            bytes / (1024 * 1024),
            count
        );
    }
    if used == KINDS {
        let _ = writeln!(out, "    (and more kinds than this list holds)");
    }
    let _ = writeln!(out);
}

/// What the firmware's number for a memory type means.
///
/// The names of the UEFI specification, so that what this prints can be
/// looked up — `EfiReservedMemoryType` is searchable and `type 0` is not.
const fn kind_name(kind: u32) -> &'static str {
    match kind {
        0 => "reserved",
        1 => "loader code",
        2 => "loader data",
        3 => "boot services code",
        4 => "boot services data",
        5 => "runtime services code",
        6 => "runtime services data",
        7 => "conventional",
        8 => "unusable",
        9 => "acpi reclaimable",
        10 => "acpi nvs",
        11 => "memory-mapped i/o",
        12 => "mmio port space",
        13 => "pal code",
        14 => "persistent memory",
        15 => "unaccepted",
        _ => "vendor-specific",
    }
}

fn display(out: &mut Report, framebuffer: Option<&harlan_hal::framebuffer::FramebufferInfo>) {
    let _ = writeln!(out, "[display]");
    match framebuffer {
        Some(fb) => {
            let _ = writeln!(out, "  mode      {}x{}", fb.width, fb.height);
            let _ = writeln!(out, "  stride    {} pixel(s)", fb.stride);
            let _ = writeln!(
                out,
                "  at        {:#x}, {} byte(s)",
                fb.base_addr, fb.size_bytes
            );
        }
        None => {
            let _ = writeln!(out, "  the firmware offered no usable framebuffer");
        }
    }
    let _ = writeln!(out);
}

fn bus(out: &mut Report) {
    let _ = writeln!(out, "[pci]");
    // SAFETY: the configuration ports are the firmware's and ours; reading
    // them is what the firmware has already done to set the devices up
    // (ADR 0022).
    let devices = unsafe { harlan_arch_x86_64::pci::scan() };
    for function in devices.iter() {
        let _ = writeln!(
            out,
            "  {:02x}:{:02x}.{}  {:04x}:{:04x}  class {:02x}.{:02x}.{:02x}  {}",
            function.at.bus,
            function.at.device,
            function.at.function,
            function.header.vendor,
            function.header.device,
            function.header.class,
            function.header.subclass,
            function.header.prog_if,
            function.header.class_name(),
        );
    }
    let _ = writeln!(out, "  {} function(s)", devices.len());
    if devices.lost() > 0 {
        // Said loudly: an inventory that quietly dropped devices is an
        // inventory that will send somebody looking for a driver for a
        // thing that is not the thing they have.
        let _ = writeln!(
            out,
            "  WARNING: {} more function(s) were found and not kept; this list is incomplete",
            devices.lost()
        );
    }
    let _ = writeln!(out);
}

/// Writes the report into the root of the volume this program was loaded
/// from.
///
/// **That volume and no other.** The handle comes from the loaded-image
/// protocol, so it is the stick this was booted off — not "a disk", not
/// "the first one found". It is the same care ADR 0033 puts in the kernel,
/// by the only means available on this side of `ExitBootServices`.
///
/// A failure is reported and not fatal: the report also goes to the screen
/// and the serial port, and a machine whose firmware will not let us write
/// is still a machine worth inventorying.
fn write_to_the_stick(bytes: &[u8]) {
    let image = boot::image_handle();
    let mut fs = match boot::get_image_file_system(image) {
        Ok(fs) => fs,
        Err(err) => {
            harlan_hal::warn!("HARLAN: the volume this booted from cannot be opened ({err:?})");
            return;
        }
    };
    let mut root = match fs.open_volume() {
        Ok(root) => root,
        Err(err) => {
            harlan_hal::warn!("HARLAN: that volume has no root directory ({err:?})");
            return;
        }
    };
    let handle = match root.open(FILE, FileMode::CreateReadWrite, FileAttribute::empty()) {
        Ok(handle) => handle,
        Err(err) => {
            harlan_hal::warn!("HARLAN: INVENTORY.TXT could not be created ({err:?})");
            return;
        }
    };
    let Some(FileType::Regular(mut file)) = handle.into_type().ok() else {
        harlan_hal::warn!("HARLAN: INVENTORY.TXT is there and is not a file");
        return;
    };
    // Written from the start, so a second boot replaces the first report
    // rather than appending to it — two reports in one file, one of them
    // from a different machine, is worse than one.
    if let Err(err) = file.set_position(0) {
        harlan_hal::warn!("HARLAN: INVENTORY.TXT could not be rewound ({err:?})");
        return;
    }
    match file.write(bytes) {
        Ok(()) => {
            // The old one may have been longer. Without this, the tail of
            // the previous report stays on the end of the new one.
            let _ = file.set_position(bytes.len() as u64);
            if let Err(err) = file.flush() {
                harlan_hal::warn!("HARLAN: INVENTORY.TXT was not flushed ({err:?})");
            }
            harlan_hal::info!(
                "HARLAN: INVENTORY.TXT written to the volume this booted from, {} byte(s)",
                bytes.len()
            );
        }
        Err(err) => harlan_hal::warn!("HARLAN: INVENTORY.TXT could not be written ({err:?})"),
    }
}
