//! Hardware `Console` backend: direct writes to the physical legacy VGA
//! text-mode buffer (`0xB8000`). Replaces `UefiConsole` (Boot-Services-only,
//! unusable after `ExitBootServices`) starting this increment.
//!
//! **Known, documented gap, confirmed by screenshot (not assumed):** on
//! this project's real target (OVMF + QEMU), the display is actually
//! driven by a GOP linear framebuffer in a graphics mode (1280x800
//! observed), not legacy VGA text mode — writing to `0xB8000` does not
//! appear on screen there. This module is a correct, harmless placeholder
//! (the writes land in valid, owned memory; nothing crashes or corrupts
//! anything) that keeps the `Console` trait boundary and the
//! row/column/scroll bookkeeping in place, but its output is **not**
//! confirmed visible on this target today. Real, visible pixel-font
//! rendering onto the actual GOP framebuffer (queried before
//! `ExitBootServices`, since the GOP protocol itself stops being usable
//! after that) is deferred to Incremento 4, bundled with completing
//! `Console` for the PS/2 keyboard — building the visible-text path once,
//! at the same time real keyboard input makes it actually exercisable,
//! rather than twice. See docs/fase2-notes.md. Every automated
//! verification in this increment (`cargo xtask boot-test`) goes through
//! debugcon, not this console, so none of it depends on this gap.
//!
//! `read_key` unconditionally returns `None` for now — that is this
//! trait's own documented meaning for "no key pending", which is honestly
//! true at this point (no keyboard driver exists yet). It is filled in by
//! Incremento 4's PS/2 IRQ handler, without any other change here.

use aion_hal::{Console, ConsoleKey};

const VGA_WIDTH: usize = 80;
const VGA_HEIGHT: usize = 25;
const VGA_BUFFER_ADDR: usize = 0xB8000;
/// White text on black background (Text-mode attribute byte format: high
/// nibble = background, low nibble = foreground).
const DEFAULT_ATTRIBUTE: u8 = 0x0F;

pub struct VgaConsole {
    row: usize,
    col: usize,
}

impl VgaConsole {
    pub const fn new() -> Self {
        Self { row: 0, col: 0 }
    }

    fn buffer() -> *mut u16 {
        VGA_BUFFER_ADDR as *mut u16
    }

    fn put_char_at(row: usize, col: usize, byte: u8) {
        let offset = row * VGA_WIDTH + col;
        let value = ((DEFAULT_ATTRIBUTE as u16) << 8) | byte as u16;
        // SAFETY: `0xB8000` is the architecturally fixed physical address
        // of the VGA text-mode buffer on this platform, identity-mapped by
        // firmware (still true post-`ExitBootServices`: no page tables are
        // touched by exiting boot services). `offset` is always < 80*25
        // (`row`/`col` are kept in range by every caller below), so this
        // write always lands within the buffer's known 4000-byte extent.
        // `write_volatile` because this write's only effect is what
        // appears on screen — nothing else in this program reads it back
        // — so the compiler could otherwise elide or reorder it.
        unsafe {
            Self::buffer().add(offset).write_volatile(value);
        }
    }

    fn clear_row(row: usize) {
        for col in 0..VGA_WIDTH {
            Self::put_char_at(row, col, b' ');
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= VGA_HEIGHT {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn scroll(&mut self) {
        // SAFETY: both the source and destination ranges lie entirely
        // within the fixed, fully-owned 80x25 VGA buffer (source: rows
        // 1..HEIGHT, destination: rows 0..HEIGHT-1, both bounded by the
        // same buffer extent as `put_char_at`). `copy` (not
        // `copy_nonoverlapping`) is required and correct here because the
        // ranges overlap — this shifts the whole buffer up by one row.
        unsafe {
            let buf = Self::buffer();
            core::ptr::copy(buf.add(VGA_WIDTH), buf, VGA_WIDTH * (VGA_HEIGHT - 1));
        }
        Self::clear_row(VGA_HEIGHT - 1);
    }
}

impl Default for VgaConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl Console for VgaConsole {
    fn read_key(&mut self) -> Option<ConsoleKey> {
        None
    }

    fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                b'\n' => self.newline(),
                // Fase 1's own simplification carried over unchanged:
                // only ASCII bytes are meaningful here (this project's own
                // text is always ASCII); anything else just prints via the
                // VGA hardware charset rather than being filtered, since
                // there is no real consumer of non-ASCII output yet.
                byte => {
                    if self.col >= VGA_WIDTH {
                        self.newline();
                    }
                    Self::put_char_at(self.row, self.col, byte);
                    self.col += 1;
                }
            }
        }
    }

    fn clear(&mut self) {
        for row in 0..VGA_HEIGHT {
            Self::clear_row(row);
        }
        self.row = 0;
        self.col = 0;
    }
}
