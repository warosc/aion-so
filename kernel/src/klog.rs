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

use harlan_hal::InterruptControl;
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
        // And out of the serial port, if the machine has one
        // (docs/adr/0032-fase5-serial-console.md). Both, not one or the
        // other: every tool in this repo reads the debug port, and no real
        // machine does. A line that went to only one of them is a line
        // somebody cannot see.
        harlan_arch_x86_64::serial::write_str(s);
        Ok(())
    }
}

fn sink(level: Level, file: &str, line: u32, args: fmt::Arguments<'_>) {
    // A line goes out whole. The port takes one byte at a time, and since
    // the timer can hand the CPU to another process mid-line (ADR 0018),
    // two of them would otherwise write into each other — which is
    // exactly what the first run of the scheduler printed.
    let cpu = harlan_arch_x86_64::Cpu;
    let were_enabled = cpu.disable();
    let _ = writeln!(Port, "[{}]: {file}@{line:03}: {args}", level.name());
    if were_enabled {
        cpu.enable();
    }
}

/// Sends log lines to the debug port and the serial port, from wherever
/// this code lives now.
///
/// Called twice on purpose — once at boot and again once the kernel runs
/// from its new address — so the serial port is set up in both, which also
/// means it is set up again after the firmware has let go of it.
pub fn install() {
    // SAFETY: `serial::init` asks to be called early, before anything else
    // uses COM1, and for COM1 to be this kernel's to program. Both hold at
    // each of this function's two call sites: the first line of `efi_main`,
    // and the moment the kernel takes over its own image. Running it twice
    // is setting the same registers to the same values and asking the same
    // question again.
    let found = unsafe { harlan_arch_x86_64::serial::init() };
    klog::set_sink(sink);
    if found {
        harlan_hal::info!("HARLAN: a serial port at COM1; the log goes there too");
    }
}
