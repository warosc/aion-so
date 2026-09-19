/// Description of a linear, 32-bit-per-pixel framebuffer that software can
/// write to directly. Firmware-agnostic: `boot` fills it from whatever the
/// firmware reported (UEFI GOP today) before boot services exit, and
/// consumers only ever see this plain data.
///
/// Deliberately carries no pixel *format* (RGB vs BGR): the only colors
/// used so far are white and black, whose 32-bit value is identical in
/// either byte order, so a format field would be dead weight. It must be
/// added the day anything draws a color that isn't byte-order symmetric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramebufferInfo {
    /// Address of the first pixel. Physical and virtual coincide (the
    /// firmware's identity mapping is still what's live).
    pub base_addr: u64,
    /// Visible width, in pixels.
    pub width: u32,
    pub height: u32,
    /// Pixels per scanline; can exceed `width` for alignment.
    pub stride: u32,
    /// Size of the whole framebuffer region in bytes, as reported by the
    /// firmware — lets consumers validate that `stride * height * 4`
    /// actually fits before writing anything.
    pub size_bytes: u64,
}
