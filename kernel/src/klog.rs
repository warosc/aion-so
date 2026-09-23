//! The kernel's log sink: port 0xE9, the debug console QEMU mirrors to a
//! file.
//!
//! Nothing but port writes — no allocation, no locks, no memory touched —
//! so it works from an interrupt handler and while the memory map is being
//! rebuilt underneath.
//!
//! `install` is called twice on purpose: once at boot, and again once the
//! kernel is running from its new address, because `sink` is code and code
//! moved (docs/adr/0013-fase3-physical-window.md).

use core::fmt::{self, Write};

use harlan_hal::klog::{self, Level};

const DEBUGCON: u16 = 0xE9;

struct Port;

impl Write for Port {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.as_bytes() {
            // SAFETY: 0xE9 is the debug console: a byte written there is
            // printed by the emulator and ignored by real hardware. No
            // memory is touched and no device state depends on it.
            unsafe { harlan_arch_x86_64::port::outb(DEBUGCON, *byte) };
        }
        Ok(())
    }
}

fn sink(level: Level, file: &str, line: u32, args: fmt::Arguments<'_>) {
    let _ = writeln!(Port, "[{}]: {file}@{line:03}: {args}", level.name());
}

/// Sends log lines to the debug port, from wherever this code lives now.
pub fn install() {
    klog::set_sink(sink);
}
