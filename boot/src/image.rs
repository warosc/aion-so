//! Where the firmware loaded this image, asked while Boot Services are
//! still alive.
//!
//! The kernel needs it to keep its own code executable when it replaces the
//! firmware's identity map with one that marks everything else no-execute
//! (docs/adr/0008-fase2-write-xor-execute.md). Like the framebuffer, the
//! answer is captured as plain data before the exit.

use harlan_hal::addr::PhysAddr;
use harlan_hal::frame::PhysRange;
use uefi::boot;
use uefi::proto::loaded_image::LoadedImage;

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
