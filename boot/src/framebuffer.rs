//! Queries the firmware's Graphics Output Protocol for the linear
//! framebuffer, while Boot Services are still alive. The GOP *protocol*
//! stops being usable at `ExitBootServices`; the memory it describes keeps
//! working, which is why the answer is captured as plain data
//! (`FramebufferInfo`) beforehand.

use harlan_hal::addr::PhysAddr;
use harlan_hal::framebuffer::FramebufferInfo;
use harlan_hal::{info, warn};
use uefi::boot;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

/// Returns the current mode's framebuffer, or `None` (after logging why)
/// if there is no usable one — the kernel then runs headless, with input
/// and debugcon logging intact.
///
/// Must be called before `exit_boot_services`; the protocol handle it opens
/// is closed again before this returns.
pub fn query() -> Option<FramebufferInfo> {
    let handle = match boot::get_handle_for_protocol::<GraphicsOutput>() {
        Ok(handle) => handle,
        Err(err) => {
            warn!("HARLAN: no GOP handle ({err}); running without a display");
            return None;
        }
    };
    let mut gop = match boot::open_protocol_exclusive::<GraphicsOutput>(handle) {
        Ok(gop) => gop,
        Err(err) => {
            warn!("HARLAN: cannot open GOP ({err}); running without a display");
            return None;
        }
    };

    let mode = gop.current_mode_info();
    // Only the two 8-bit-per-channel formats: white and black are the same
    // 32-bit value in both (see `FramebufferInfo`). `Bitmask` puts channels
    // at firmware-chosen positions and `BltOnly` has no framebuffer at all
    // (asking for one panics inside `uefi`).
    match mode.pixel_format() {
        PixelFormat::Rgb | PixelFormat::Bgr => {}
        other => {
            warn!("HARLAN: unsupported GOP pixel format {other:?}; running without a display");
            return None;
        }
    }

    let (width, height) = mode.resolution();
    let stride = mode.stride();
    let mut frame_buffer = gop.frame_buffer();
    let info = FramebufferInfo {
        // The GOP reports the framebuffer at its physical address,
        // which the firmware also maps one-to-one.
        base_addr: PhysAddr::new(frame_buffer.as_mut_ptr() as u64),
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        stride: u32::try_from(stride).ok()?,
        size_bytes: frame_buffer.size() as u64,
    };
    info!(
        "HARLAN: framebuffer {}x{} stride={} at {:#x} ({} bytes)",
        info.width, info.height, info.stride, info.base_addr, info.size_bytes
    );
    Some(info)
}
