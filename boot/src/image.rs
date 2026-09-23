//! Where the firmware loaded this image, asked while Boot Services are
//! still alive.
//!
//! The kernel needs it to keep its own code executable when it replaces the
//! firmware's identity map with one that marks everything else no-execute
//! (docs/adr/0008-fase2-write-xor-execute.md). Like the framebuffer, the
//! answer is captured as plain data before the exit.

use harlan_hal::addr::PhysAddr;
use harlan_hal::frame::PhysRange;
use harlan_hal::pe;
use uefi::boot;
use uefi::proto::loaded_image::LoadedImage;

/// Where this image is and, if its headers can be read, which parts of it
/// are code.
pub struct Image {
    pub range: PhysRange,
    /// The executable sections. `None` when the headers could not be
    /// parsed: the kernel then keeps the whole image executable, as it did
    /// before (ADR 0008).
    pub code: Option<pe::CodeRanges>,
}

/// Returns the loaded image's physical range, or `None` (after logging
/// why). Without it the kernel keeps the whole lower half executable
/// rather than fault on its own code.
pub fn query() -> Option<PhysRange> {
    let loaded = match boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()) {
        Ok(loaded) => loaded,
        Err(err) => {
            log::warn!("HARLAN: cannot ask where this image is loaded ({err})");
            return None;
        }
    };
    let (base, size) = loaded.info();
    let range = PhysRange::new(PhysAddr::new(base as usize as u64), size);
    log::info!(
        "HARLAN: kernel image at {:#x}..{:#x} ({} KiB)",
        range.start,
        range.end(),
        range.len / 1024
    );
    Some(range)
}

/// The image, with its code located if the PE headers allow it.
///
/// Only the sections the linker marked executable need to stay executable;
/// the rest of the image is data and can be no-execute, and the code
/// itself can be read-only (ADR 0011).
pub fn query_with_code() -> Option<Image> {
    let range = query()?;
    // SAFETY: the firmware loaded and relocated this image at `range`, and
    // it stays mapped and readable for as long as the kernel runs. Only
    // read, and only through this slice.
    let bytes = unsafe {
        core::slice::from_raw_parts(
            range.start.as_u64() as usize as *const u8,
            range.len as usize,
        )
    };
    let code = match pe::code_sections(bytes).map(|sections| sections.at(range.start)) {
        Ok(Some(code)) => {
            for section in code.iter() {
                log::info!(
                    "HARLAN: kernel code at {:#x}..{:#x} ({} KiB)",
                    section.start,
                    section.end(),
                    section.len / 1024
                );
            }
            Some(code)
        }
        Ok(None) => {
            log::warn!(
                "HARLAN: the image's code sections do not fit in memory; keeping it all executable"
            );
            None
        }
        Err(err) => {
            log::warn!(
                "HARLAN: cannot read this image's sections ({err:?}); keeping it all executable"
            );
            None
        }
    };
    Some(Image { range, code })
}
