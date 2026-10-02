//! The 16550 UART on COM1, as a place for the kernel's log to go on a
//! machine that is not an emulator
//! (docs/adr/0032-fase5-serial-console.md).
//!
//! Everything the kernel says today goes to port `0xE9`, the debug console.
//! That port exists because QEMU and Bochs watch it; **real hardware
//! ignores it entirely**. On the target PC the kernel would boot and say
//! nothing at all, which is the worst possible state to debug a first boot
//! from. A UART is what that machine has instead, and what a serial cable
//! or a USB adapter at the other end can read.
//!
//! Polled, not interrupt-driven: this runs from inside interrupt handlers
//! and while the memory map is being rebuilt underneath, so it must touch
//! no memory, take no locks and allocate nothing — the same contract the
//! debug port already keeps.

use crate::port;

/// COM1. The one every PC has had in the same place since the IBM PC, and
/// the one firmware consoles and BMCs use when there is one.
const COM1: u16 = 0x3F8;

// The register at each offset. Some change meaning when the divisor latch
// is open, which is why the names come in pairs.
const DATA: u16 = 0;
/// Interrupt enable, or the high byte of the divisor.
const INTERRUPTS: u16 = 1;
/// FIFO control (write) — and the interrupt identity (read).
const FIFO: u16 = 2;
/// Line control: word length, stop bits, parity, and the divisor latch.
const LINE: u16 = 3;
/// Modem control: DTR, RTS, and the loopback bit used to test it.
const MODEM: u16 = 4;
/// Line status: whether the transmitter will take another byte.
const STATUS: u16 = 5;
/// A byte of memory that does nothing, which is how a UART is detected.
const SCRATCH: u16 = 7;

/// Divisor latch access: the bit that turns registers 0 and 1 into the two
/// halves of the baud divisor.
const DIVISOR_LATCH: u8 = 0x80;
/// Eight bits, no parity, one stop bit.
const EIGHT_N_ONE: u8 = 0x03;
/// The transmitter holding register is empty: it will take another byte.
const READY_TO_SEND: u8 = 0x20;
/// Loopback, so what is sent comes straight back without a cable.
const LOOPBACK: u8 = 0x10;

/// 115 200 baud: the divisor is 1, because the clock is 115 200 × 16.
///
/// The fastest the part does without tricks, and what every terminal
/// defaults to. Slower would make the log cost more boot time than it is
/// worth: a line is about 60 bytes, and at 115 200 that is 5 ms.
const DIVISOR: u16 = 1;

/// How many times to ask whether the transmitter is ready before giving
/// up on the byte.
///
/// **The most important number in this file.** On a machine with no COM1 —
/// or one whose UART is wedged — the ready bit never sets, and a loop
/// without a bound would spin for ever inside the log path, which runs from
/// interrupt handlers. The kernel would hang on its first log line, on real
/// hardware, saying nothing. A dropped byte is a bad log; a spin is a dead
/// machine.
///
/// Generous beside how long a byte takes (about 87 µs at 115 200) and
/// nothing beside a boot.
const PATIENCE: u32 = 100_000;

/// Whether a UART answered when it was set up.
///
/// A plain `static mut` read without synchronisation, which is sound here
/// for the reason the whole module is: it is written once, by `init`,
/// before anything else runs, and only read afterwards.
static mut PRESENT: bool = false;

/// Sets COM1 up and answers whether there is one.
///
/// Detected rather than assumed: a modern machine often has no COM1 at all,
/// and writing the log into a port nothing answers would cost `PATIENCE`
/// spins per byte for nothing.
///
/// # Safety
///
/// Called once, early, before anything else uses the serial port. Writes to
/// the COM1 I/O ports, which on a machine where they belong to something
/// else would disturb it — there is no way to ask, which is why this is the
/// fixed, oldest address rather than one found by probing.
pub unsafe fn init() -> bool {
    // SAFETY: forwarded. Each write is to a COM1 register, in the order the
    // part requires: interrupts off before changing anything, the divisor
    // set with the latch open, then the latch closed again.
    let present = unsafe {
        // No interrupts from it: this is polled.
        port::outb(COM1 + INTERRUPTS, 0x00);
        // Open the divisor latch and set the baud rate.
        port::outb(COM1 + LINE, DIVISOR_LATCH);
        port::outb(COM1 + DATA, DIVISOR as u8);
        port::outb(COM1 + INTERRUPTS, (DIVISOR >> 8) as u8);
        // Close it again; from here registers 0 and 1 are data and
        // interrupt-enable once more.
        port::outb(COM1 + LINE, EIGHT_N_ONE);
        // FIFOs on and cleared, interrupting at fourteen bytes — which
        // nothing waits for, since this never asks for an interrupt.
        port::outb(COM1 + FIFO, 0xC7);
        // DTR and RTS up, so anything on the other end sees a live line.
        port::outb(COM1 + MODEM, 0x03);

        detect()
    };
    // SAFETY: written once, here, before anything reads it.
    unsafe { PRESENT = present };
    present
}

/// Whether there is a UART at COM1.
///
/// Two questions, because either alone can be answered by an empty bus. A
/// port with nothing behind it reads back `0xFF`, which is a value the
/// scratch test would wrongly accept if it happened to write `0xFF`, and
/// the loopback test alone can pass on some emulated parts that ignore the
/// loopback bit.
///
/// # Safety
///
/// As `init`: COM1's registers are this kernel's to use.
unsafe fn detect() -> bool {
    // SAFETY: forwarded.
    unsafe {
        // The scratch register is a byte of memory that does nothing. If
        // what goes in comes back, something is there.
        port::outb(COM1 + SCRATCH, 0x5A);
        if port::inb(COM1 + SCRATCH) != 0x5A {
            return false;
        }
        port::outb(COM1 + SCRATCH, 0xA5);
        if port::inb(COM1 + SCRATCH) != 0xA5 {
            return false;
        }

        // And a byte sent round the part's own loopback has to come back
        // as itself. This catches a port that reads back whatever was last
        // written without being a UART.
        port::outb(COM1 + MODEM, LOOPBACK | 0x03);
        port::outb(COM1 + DATA, 0xAE);
        let mut waited = 0;
        while port::inb(COM1 + STATUS) & 0x01 == 0 {
            waited += 1;
            if waited == PATIENCE {
                // Nothing came back: put the line back the way it was and
                // say there is no port.
                port::outb(COM1 + MODEM, 0x03);
                return false;
            }
        }
        let came_back = port::inb(COM1 + DATA);
        port::outb(COM1 + MODEM, 0x03);
        came_back == 0xAE
    }
}

/// Sends one byte, giving up rather than waiting for ever.
///
/// Answers whether it went. A `false` is a byte of the log that nobody will
/// read, which is a worse log; waiting instead would be a dead machine
/// (see `PATIENCE`).
///
/// # Safety
///
/// `init` must have run and answered `true`.
unsafe fn send(byte: u8) -> bool {
    let mut waited = 0;
    // SAFETY: forwarded; reading the line status register has no side
    // effects beyond clearing bits this does not use.
    while unsafe { port::inb(COM1 + STATUS) } & READY_TO_SEND == 0 {
        waited += 1;
        if waited == PATIENCE {
            return false;
        }
    }
    // SAFETY: forwarded; the transmitter has said it will take a byte.
    unsafe { port::outb(COM1 + DATA, byte) };
    true
}

/// Writes text to COM1, if there is one.
///
/// A newline goes out as carriage return and newline, because a terminal at
/// the other end moves down without moving back otherwise and every line
/// after the first starts where the last one ended.
pub fn write_str(text: &str) {
    // SAFETY: `PRESENT` is written once by `init` before anything else
    // runs, and only read here.
    if !unsafe { PRESENT } {
        return;
    }
    for byte in text.as_bytes() {
        if *byte == b'\n' {
            // SAFETY: `init` ran and found a port.
            unsafe { send(b'\r') };
        }
        // SAFETY: as above. A byte that does not go is dropped: the next
        // one is tried anyway, because a wedged transmitter that recovers
        // should not cost the rest of the line.
        unsafe { send(*byte) };
    }
}

/// Whether the kernel's log is going out of a serial port.
pub fn present() -> bool {
    // SAFETY: as `write_str`.
    unsafe { PRESENT }
}
