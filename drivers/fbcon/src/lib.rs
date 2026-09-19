//! Framebuffer text console: an 8x8 bitmap font, drawn 2x, onto a linear
//! 32-bit framebuffer, plus the cursor/newline/wrap/scroll logic on top.
//!
//! Split in two on purpose. `TextConsole` is the pure cursor logic (where
//! the next character goes, when to wrap, when to scroll) and only talks
//! to a `Surface`, so it is fully host-testable against a fake one.
//! `FramebufferSurface` is the only part that touches raw memory.
//!
//! Colors are white on black. Both are byte-order symmetric as a 32-bit
//! pixel (`0x00FFFFFF` / `0`), so RGB-versus-BGR framebuffers need no
//! distinction here — see `aion_hal::framebuffer::FramebufferInfo`.

#![cfg_attr(not(test), no_std)]

use aion_hal::framebuffer::FramebufferInfo;
use font8x8::legacy::BASIC_LEGACY;

const GLYPH_PIXELS: usize = 8;
const SCALE: usize = 2;
/// Side of one character cell, in framebuffer pixels.
pub const CELL_PIXELS: usize = GLYPH_PIXELS * SCALE;

const FOREGROUND: u32 = 0x00FF_FFFF;
const BACKGROUND: u32 = 0x0000_0000;

/// Something that can draw character cells on a grid. Kept tiny so the
/// cursor logic above it stays independent of any real memory.
pub trait Surface {
    fn cols(&self) -> usize;
    fn rows(&self) -> usize;
    /// Draws `byte` in the cell at (`col`, `row`), replacing whatever was
    /// there. Callers guarantee `col < cols()` and `row < rows()`.
    fn draw_glyph(&mut self, col: usize, row: usize, byte: u8);
    /// Moves every row up by one and blanks the last row.
    fn scroll_up_one_row(&mut self);
    fn clear(&mut self);
}

pub struct TextConsole<S: Surface> {
    surface: S,
    col: usize,
    row: usize,
}

impl<S: Surface> TextConsole<S> {
    pub fn new(mut surface: S) -> Self {
        surface.clear();
        Self {
            surface,
            col: 0,
            row: 0,
        }
    }

    pub fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            self.put_byte(byte);
        }
    }

    pub fn clear(&mut self) {
        self.surface.clear();
        self.col = 0;
        self.row = 0;
    }

    fn put_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            0x08 => self.backspace(),
            byte => {
                // A line exactly `cols` wide leaves the cursor one past the
                // end ("pending wrap"): the wrap happens only when the next
                // character actually needs a cell.
                if self.col >= self.surface.cols() {
                    self.newline();
                }
                self.surface.draw_glyph(self.col, self.row, byte);
                self.col += 1;
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.surface.rows() {
            self.surface.scroll_up_one_row();
        } else {
            self.row += 1;
        }
    }

    /// Moves the cursor one cell left without erasing (the shell erases
    /// with `BS, ' ', BS`). At column 0 it steps back onto the end of the
    /// previous row, so an input line that wrapped can still be erased
    /// through the wrap point; at the very first cell it does nothing.
    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.surface.cols() - 1;
        }
    }
}

fn glyph_for(byte: u8) -> [u8; 8] {
    // The table covers ASCII only. Anything else draws as '?', so an
    // unexpected byte is visibly wrong instead of silently blank.
    if byte < 128 {
        BASIC_LEGACY[byte as usize]
    } else {
        BASIC_LEGACY[b'?' as usize]
    }
}

/// Draws text cells straight into a linear framebuffer.
///
/// Holds a raw pointer because the pixels live in device memory at an
/// address only known at runtime (whatever the firmware reported); Rust has
/// no safe type for "this many bytes at this hardware address", so every
/// access goes through the validated-once `unsafe` in `new` and the
/// bounds-kept-in-range helpers below.
pub struct FramebufferSurface {
    base: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
    cols: usize,
    rows: usize,
}

impl FramebufferSurface {
    /// Returns `None` if `info` is not internally consistent (zero size,
    /// `stride < width`, a region too small for `stride * height` pixels, a
    /// misaligned base, or no room for even one character cell) — a
    /// descriptor that lies about its own bounds must not become an
    /// out-of-bounds write.
    ///
    /// # Safety
    ///
    /// `info.base_addr` must be the address of memory that is valid for
    /// volatile reads and writes of `info.size_bytes` bytes, stays valid for
    /// as long as the returned surface is used, and is not concurrently
    /// accessed by anything else.
    pub unsafe fn new(info: &FramebufferInfo) -> Option<Self> {
        let width = info.width as usize;
        let height = info.height as usize;
        let stride = info.stride as usize;
        let needed_bytes = (stride as u64)
            .checked_mul(height as u64)?
            .checked_mul(size_of::<u32>() as u64)?;
        if width == 0
            || height == 0
            || stride < width
            || needed_bytes > info.size_bytes
            || !info.base_addr.is_multiple_of(size_of::<u32>() as u64)
        {
            return None;
        }
        let cols = width / CELL_PIXELS;
        let rows = height / CELL_PIXELS;
        if cols == 0 || rows == 0 {
            return None;
        }
        Some(Self {
            base: info.base_addr as *mut u32,
            width,
            height,
            stride,
            cols,
            rows,
        })
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: u32) {
        debug_assert!(x < self.width && y < self.height);
        // SAFETY: `new` validated that `stride * height` pixels fit in the
        // region the caller vouched for, and every caller of this private
        // helper keeps `x < width <= stride` and `y < height`, so the index
        // is always inside it. Volatile because the only consumer of this
        // memory is the display hardware — nothing else in the program
        // reads it back, so the compiler could otherwise drop the write.
        unsafe {
            self.base.add(y * self.stride + x).write_volatile(color);
        }
    }

    fn fill_pixel_rows(&mut self, first_y: usize, end_y: usize, color: u32) {
        for y in first_y..end_y {
            for x in 0..self.width {
                self.put_pixel(x, y, color);
            }
        }
    }
}

impl Surface for FramebufferSurface {
    fn cols(&self) -> usize {
        self.cols
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn draw_glyph(&mut self, col: usize, row: usize, byte: u8) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        let glyph = glyph_for(byte);
        let x0 = col * CELL_PIXELS;
        let y0 = row * CELL_PIXELS;
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..GLYPH_PIXELS {
                // font8x8: bit 0 is the leftmost pixel of the row.
                let color = if bits & (1 << gx) != 0 {
                    FOREGROUND
                } else {
                    BACKGROUND
                };
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        self.put_pixel(x0 + gx * SCALE + dx, y0 + gy * SCALE + dy, color);
                    }
                }
            }
        }
    }

    fn scroll_up_one_row(&mut self) {
        let text_height = self.rows * CELL_PIXELS;
        // SAFETY: source (pixel rows CELL..text_height) and destination
        // (0..text_height - CELL) both lie inside the `stride * height`
        // pixels `new` validated, since `text_height <= height`. `copy`
        // (memmove semantics) is required because the ranges overlap. It
        // is not a volatile copy, which is acceptable here: the framebuffer
        // is write-only from the program's point of view and the values
        // moved are never read back.
        unsafe {
            core::ptr::copy(
                self.base.add(CELL_PIXELS * self.stride),
                self.base,
                (text_height - CELL_PIXELS) * self.stride,
            );
        }
        self.fill_pixel_rows(text_height - CELL_PIXELS, text_height, BACKGROUND);
    }

    fn clear(&mut self) {
        self.fill_pixel_rows(0, self.height, BACKGROUND);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Op {
        Draw(usize, usize, u8),
        Scroll,
        Clear,
    }

    struct FakeSurface {
        cols: usize,
        rows: usize,
        ops: Vec<Op>,
    }

    impl FakeSurface {
        fn new(cols: usize, rows: usize) -> Self {
            Self {
                cols,
                rows,
                ops: Vec::new(),
            }
        }
    }

    impl Surface for FakeSurface {
        fn cols(&self) -> usize {
            self.cols
        }
        fn rows(&self) -> usize {
            self.rows
        }
        fn draw_glyph(&mut self, col: usize, row: usize, byte: u8) {
            self.ops.push(Op::Draw(col, row, byte));
        }
        fn scroll_up_one_row(&mut self) {
            self.ops.push(Op::Scroll);
        }
        fn clear(&mut self) {
            self.ops.push(Op::Clear);
        }
    }

    /// A console over a 4x3 fake surface, with the constructor's own
    /// initial `Clear` already discarded so tests see only what they cause.
    fn console(cols: usize, rows: usize) -> TextConsole<FakeSurface> {
        let mut c = TextConsole::new(FakeSurface::new(cols, rows));
        c.surface.ops.clear();
        c
    }

    #[test]
    fn new_clears_the_surface() {
        let c = TextConsole::new(FakeSurface::new(4, 3));
        assert_eq!(c.surface.ops, vec![Op::Clear]);
    }

    #[test]
    fn text_advances_one_column_per_character() {
        let mut c = console(4, 3);
        c.write_str("ab");
        assert_eq!(
            c.surface.ops,
            vec![Op::Draw(0, 0, b'a'), Op::Draw(1, 0, b'b')]
        );
    }

    #[test]
    fn newline_moves_to_column_zero_of_the_next_row() {
        let mut c = console(4, 3);
        c.write_str("a\nb");
        assert_eq!(
            c.surface.ops,
            vec![Op::Draw(0, 0, b'a'), Op::Draw(0, 1, b'b')]
        );
    }

    #[test]
    fn a_full_line_wraps_only_when_the_next_character_needs_a_cell() {
        let mut c = console(3, 3);
        c.write_str("abc");
        // Exactly one line: no wrap yet, nothing scrolled.
        assert_eq!(
            c.surface.ops,
            vec![
                Op::Draw(0, 0, b'a'),
                Op::Draw(1, 0, b'b'),
                Op::Draw(2, 0, b'c')
            ]
        );
        c.surface.ops.clear();
        c.write_str("d");
        assert_eq!(c.surface.ops, vec![Op::Draw(0, 1, b'd')]);
    }

    #[test]
    fn newline_on_the_last_row_scrolls_instead_of_moving_down() {
        let mut c = console(4, 2);
        c.write_str("a\nb\nc");
        assert_eq!(
            c.surface.ops,
            vec![
                Op::Draw(0, 0, b'a'),
                Op::Draw(0, 1, b'b'),
                Op::Scroll,
                Op::Draw(0, 1, b'c'),
            ]
        );
    }

    #[test]
    fn wrap_on_the_last_row_scrolls_too() {
        let mut c = console(2, 1);
        c.write_str("abc");
        assert_eq!(
            c.surface.ops,
            vec![
                Op::Draw(0, 0, b'a'),
                Op::Draw(1, 0, b'b'),
                Op::Scroll,
                Op::Draw(0, 0, b'c'),
            ]
        );
    }

    #[test]
    fn backspace_moves_left_without_drawing() {
        let mut c = console(4, 3);
        c.write_str("ab\u{8}c");
        assert_eq!(
            c.surface.ops,
            vec![
                Op::Draw(0, 0, b'a'),
                Op::Draw(1, 0, b'b'),
                Op::Draw(1, 0, b'c'),
            ]
        );
    }

    #[test]
    fn the_shells_erase_sequence_blanks_the_last_character() {
        // The shell erases one typed character with BS, space, BS.
        let mut c = console(8, 3);
        c.write_str("abc");
        c.surface.ops.clear();
        c.write_str("\u{8} \u{8}");
        assert_eq!(c.surface.ops, vec![Op::Draw(2, 0, b' ')]);
        c.surface.ops.clear();
        c.write_str("x");
        assert_eq!(c.surface.ops, vec![Op::Draw(2, 0, b'x')]);
    }

    #[test]
    fn backspace_at_the_very_first_cell_does_nothing() {
        let mut c = console(4, 3);
        c.write_str("\u{8}a");
        assert_eq!(c.surface.ops, vec![Op::Draw(0, 0, b'a')]);
    }

    #[test]
    fn backspace_at_column_zero_steps_back_onto_the_previous_wrapped_row() {
        let mut c = console(3, 3);
        c.write_str("abcd"); // 'd' wrapped to (0, 1)
        c.surface.ops.clear();
        c.write_str("\u{8} \u{8}"); // erase 'd'
        assert_eq!(c.surface.ops, vec![Op::Draw(0, 1, b' ')]);
        c.surface.ops.clear();
        c.write_str("\u{8} \u{8}"); // erase 'c' across the wrap point
        assert_eq!(c.surface.ops, vec![Op::Draw(2, 0, b' ')]);
        c.surface.ops.clear();
        c.write_str("z");
        assert_eq!(c.surface.ops, vec![Op::Draw(2, 0, b'z')]);
    }

    #[test]
    fn carriage_return_returns_to_column_zero_without_changing_row() {
        let mut c = console(4, 3);
        c.write_str("ab\rc");
        assert_eq!(
            c.surface.ops,
            vec![
                Op::Draw(0, 0, b'a'),
                Op::Draw(1, 0, b'b'),
                Op::Draw(0, 0, b'c'),
            ]
        );
    }

    #[test]
    fn clear_blanks_the_surface_and_homes_the_cursor() {
        let mut c = console(4, 3);
        c.write_str("a\nb");
        c.surface.ops.clear();
        c.clear();
        c.write_str("c");
        assert_eq!(c.surface.ops, vec![Op::Clear, Op::Draw(0, 0, b'c')]);
    }

    // ---- FramebufferSurface, over a real (host) pixel buffer ----------

    const W: usize = 64; // 4 columns
    const H: usize = 48; // 3 rows
    const STRIDE: usize = 70; // deliberately wider than the visible width

    fn info_for(buf: &mut [u32]) -> FramebufferInfo {
        FramebufferInfo {
            base_addr: buf.as_mut_ptr() as u64,
            width: W as u32,
            height: H as u32,
            stride: STRIDE as u32,
            size_bytes: (buf.len() * 4) as u64,
        }
    }

    fn surface(buf: &mut [u32]) -> FramebufferSurface {
        let info = info_for(buf);
        // SAFETY: `buf` outlives the surface within each test and nothing
        // else touches it while the surface is in use.
        unsafe { FramebufferSurface::new(&info) }.expect("consistent info")
    }

    fn expected_a_at_cell(x: usize, y: usize, col: usize, row: usize) -> u32 {
        let (x0, y0) = (col * CELL_PIXELS, row * CELL_PIXELS);
        if x < x0 || x >= x0 + CELL_PIXELS || y < y0 || y >= y0 + CELL_PIXELS {
            return 0;
        }
        let gx = (x - x0) / SCALE;
        let gy = (y - y0) / SCALE;
        if BASIC_LEGACY[b'A' as usize][gy] & (1 << gx) != 0 {
            FOREGROUND
        } else {
            BACKGROUND
        }
    }

    #[test]
    fn a_glyph_lands_in_exactly_its_own_cell_scaled_and_never_in_stride_padding() {
        let mut buf = vec![0u32; STRIDE * H];
        let mut s = surface(&mut buf);
        s.draw_glyph(1, 1, b'A');
        for y in 0..H {
            for x in 0..STRIDE {
                assert_eq!(
                    buf[y * STRIDE + x],
                    expected_a_at_cell(x, y, 1, 1),
                    "pixel ({x},{y})"
                );
            }
        }
        assert!(
            buf.contains(&FOREGROUND),
            "the glyph must light some pixels"
        );
    }

    #[test]
    fn control_characters_draw_blank_and_non_ascii_draws_a_question_mark() {
        let mut buf = vec![0xDEAD_BEEFu32; STRIDE * H];
        let mut s = surface(&mut buf);
        s.draw_glyph(0, 0, 0x07);
        // Blank glyph still overwrites the cell (with background).
        for y in 0..CELL_PIXELS {
            for x in 0..CELL_PIXELS {
                assert_eq!(buf[y * STRIDE + x], BACKGROUND);
            }
        }
        let mut s = surface(&mut buf);
        s.draw_glyph(1, 0, 0xE9);
        let lit = (0..CELL_PIXELS)
            .flat_map(|y| (CELL_PIXELS..2 * CELL_PIXELS).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[y * STRIDE + x] == FOREGROUND)
            .count();
        assert!(lit > 0, "a non-ASCII byte must draw the visible '?' glyph");
    }

    #[test]
    fn scrolling_moves_a_row_up_and_blanks_the_bottom() {
        let mut buf = vec![0u32; STRIDE * H];
        let mut s = surface(&mut buf);
        s.draw_glyph(0, 1, b'A');
        s.scroll_up_one_row();
        for y in 0..H {
            for x in 0..STRIDE {
                assert_eq!(
                    buf[y * STRIDE + x],
                    expected_a_at_cell(x, y, 0, 0),
                    "pixel ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn clear_blanks_every_visible_pixel() {
        let mut buf = vec![0xFFFF_FFFFu32; STRIDE * H];
        let mut s = surface(&mut buf);
        s.clear();
        for y in 0..H {
            for x in 0..W {
                assert_eq!(buf[y * STRIDE + x], BACKGROUND);
            }
        }
    }

    #[test]
    fn new_rejects_a_descriptor_that_lies_about_its_bounds() {
        let mut buf = vec![0u32; STRIDE * H];
        let good = info_for(&mut buf);
        // SAFETY (all cases): `new` returns None before touching memory.
        let too_small = FramebufferInfo {
            size_bytes: good.size_bytes - 4,
            ..good
        };
        assert!(unsafe { FramebufferSurface::new(&too_small) }.is_none());
        let narrow_stride = FramebufferInfo {
            stride: (W - 1) as u32,
            ..good
        };
        assert!(unsafe { FramebufferSurface::new(&narrow_stride) }.is_none());
        let misaligned = FramebufferInfo {
            base_addr: good.base_addr + 1,
            ..good
        };
        assert!(unsafe { FramebufferSurface::new(&misaligned) }.is_none());
        let zero_width = FramebufferInfo { width: 0, ..good };
        assert!(unsafe { FramebufferSurface::new(&zero_width) }.is_none());
        let no_room_for_a_cell = FramebufferInfo {
            width: (CELL_PIXELS - 1) as u32,
            ..good
        };
        assert!(unsafe { FramebufferSurface::new(&no_room_for_a_cell) }.is_none());
        let overflow = FramebufferInfo {
            stride: u32::MAX,
            height: u32::MAX,
            ..good
        };
        assert!(unsafe { FramebufferSurface::new(&overflow) }.is_none());
    }
}
