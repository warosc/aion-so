//! PS/2 keyboard, via the legacy i8042 controller.
//!
//! Three pieces, deliberately separate:
//!
//! * `on_irq`, the IRQ1 handler body: reads raw scancodes off the controller
//!   and pushes them onto `SCANCODES`. Does nothing else — no decoding, no
//!   shared decoder state — so the code that runs with interrupts disabled
//!   stays tiny.
//! * `ScancodeQueue`, the single-producer (the ISR) / single-consumer
//!   (`Keyboard::read_key`) ring between them. Built on atomics, so it needs
//!   no `unsafe` and no interrupt masking.
//! * `Decoder`, a pure scancode-set-1 to `ConsoleKey` state machine, owned by
//!   the consumer side. Host-testable, unlike the two pieces above.
//!
//! Scope: US QWERTY, printable ASCII plus Enter/Backspace, Shift only. No
//! Caps Lock, Ctrl/Alt chords, keypad or extended keys (arrows, Delete,
//! ...): they all decode to `ConsoleKey::Unknown`, which is what the shell
//! already ignores. There is no layout system because there is no consumer
//! that needs one yet.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use harlan_hal::ConsoleKey;

use crate::port::{inb, outb};

const DATA_PORT: u16 = 0x60;
/// Read = status register, write = command register (same port number).
const STATUS_COMMAND_PORT: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
/// Set when the byte waiting in the output buffer came from the mouse port.
const STATUS_AUX_DATA: u8 = 1 << 5;

const COMMAND_READ_CONFIG: u8 = 0x20;
const COMMAND_WRITE_CONFIG: u8 = 0x60;

const CONFIG_KEYBOARD_IRQ_ENABLE: u8 = 1 << 0;
/// Set = the keyboard's clock line is held off (keyboard disabled).
const CONFIG_KEYBOARD_CLOCK_DISABLE: u8 = 1 << 4;
/// The controller converts the keyboard's native scancode set 2 into the
/// set 1 this module decodes.
const CONFIG_TRANSLATE: u8 = 1 << 6;

/// Keyboard command: start sending scancodes. Answered with `ACK`.
const KEYBOARD_ENABLE_SCANNING: u8 = 0xF4;
const KEYBOARD_ACK: u8 = 0xFA;

/// Upper bound on status-register polls before giving up on the controller.
/// Generous (each poll is one port read) but finite: a missing or wedged
/// i8042 must produce an error, not a boot hang.
const POLL_LIMIT: u32 = 200_000;
/// Upper bound on bytes drained from the output buffer in one go, for the
/// same reason: a stuck-full status bit must not trap the caller.
const DRAIN_LIMIT: usize = 16;

// ---------------------------------------------------------------------
// Scancode queue
// ---------------------------------------------------------------------

const QUEUE_CAPACITY: usize = 64;

/// Fixed-capacity single-producer / single-consumer ring of raw scancodes.
///
/// `push` may only be called from one context (the IRQ1 handler) and `pop`
/// from one other (the console reader). Under that contract it is correct
/// whether the two run on one core, interrupting each other, or on two. The
/// indices are free-running counters (masked to a slot on use) so
/// "empty" and "full" are unambiguous without sacrificing a slot.
pub struct ScancodeQueue {
    slots: [AtomicU8; QUEUE_CAPACITY],
    /// Next slot to read. Written only by the consumer.
    head: AtomicUsize,
    /// Next slot to write. Written only by the producer.
    tail: AtomicUsize,
}

// `% QUEUE_CAPACITY` on wrapping counters is only consistent across the
// wrap point of `usize` if the capacity divides `2^usize::BITS`.
const _: () = assert!(QUEUE_CAPACITY.is_power_of_two());

impl ScancodeQueue {
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicU8::new(0) }; QUEUE_CAPACITY],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer side. Returns `false`, storing nothing, if the queue is
    /// full: dropping the newest scancode is preferable to overwriting one
    /// the consumer may be in the middle of reading.
    #[must_use]
    pub fn push(&self, scancode: u8) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) == QUEUE_CAPACITY {
            return false;
        }
        self.slots[tail % QUEUE_CAPACITY].store(scancode, Ordering::Relaxed);
        // Release: publishes the slot write above to the consumer's
        // Acquire load of `tail`.
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side. `None` if nothing is pending.
    pub fn pop(&self) -> Option<u8> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let scancode = self.slots[head % QUEUE_CAPACITY].load(Ordering::Relaxed);
        // Release: tells the producer this slot may be reused only after
        // the read above is done.
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(scancode)
    }
}

impl Default for ScancodeQueue {
    fn default() -> Self {
        Self::new()
    }
}

static SCANCODES: ScancodeQueue = ScancodeQueue::new();

// ---------------------------------------------------------------------
// Scancode set 1 decoding
// ---------------------------------------------------------------------

const PREFIX_EXTENDED: u8 = 0xE0;
const BREAK_BIT: u8 = 0x80;

const SC_BACKSPACE: u8 = 0x0E;
const SC_ENTER: u8 = 0x1C;
const SC_LEFT_SHIFT: u8 = 0x2A;
const SC_RIGHT_SHIFT: u8 = 0x36;

/// US QWERTY, indexed by set-1 make code: `(unshifted, shifted)` ASCII, or
/// `(0, 0)` for a key that produces no printable character here (Esc, Tab,
/// Ctrl, Alt, Caps Lock, keypad, ...). Backspace and Enter are handled
/// before this table is consulted, so their entries are unused.
const KEY_TABLE: [(u8, u8); 0x3A] = [
    (0, 0),        // 0x00
    (0, 0),        // 0x01 Esc
    (b'1', b'!'),  // 0x02
    (b'2', b'@'),  // 0x03
    (b'3', b'#'),  // 0x04
    (b'4', b'$'),  // 0x05
    (b'5', b'%'),  // 0x06
    (b'6', b'^'),  // 0x07
    (b'7', b'&'),  // 0x08
    (b'8', b'*'),  // 0x09
    (b'9', b'('),  // 0x0A
    (b'0', b')'),  // 0x0B
    (b'-', b'_'),  // 0x0C
    (b'=', b'+'),  // 0x0D
    (0, 0),        // 0x0E Backspace
    (0, 0),        // 0x0F Tab
    (b'q', b'Q'),  // 0x10
    (b'w', b'W'),  // 0x11
    (b'e', b'E'),  // 0x12
    (b'r', b'R'),  // 0x13
    (b't', b'T'),  // 0x14
    (b'y', b'Y'),  // 0x15
    (b'u', b'U'),  // 0x16
    (b'i', b'I'),  // 0x17
    (b'o', b'O'),  // 0x18
    (b'p', b'P'),  // 0x19
    (b'[', b'{'),  // 0x1A
    (b']', b'}'),  // 0x1B
    (0, 0),        // 0x1C Enter
    (0, 0),        // 0x1D Left Ctrl
    (b'a', b'A'),  // 0x1E
    (b's', b'S'),  // 0x1F
    (b'd', b'D'),  // 0x20
    (b'f', b'F'),  // 0x21
    (b'g', b'G'),  // 0x22
    (b'h', b'H'),  // 0x23
    (b'j', b'J'),  // 0x24
    (b'k', b'K'),  // 0x25
    (b'l', b'L'),  // 0x26
    (b';', b':'),  // 0x27
    (b'\'', b'"'), // 0x28
    (b'`', b'~'),  // 0x29
    (0, 0),        // 0x2A Left Shift
    (b'\\', b'|'), // 0x2B
    (b'z', b'Z'),  // 0x2C
    (b'x', b'X'),  // 0x2D
    (b'c', b'C'),  // 0x2E
    (b'v', b'V'),  // 0x2F
    (b'b', b'B'),  // 0x30
    (b'n', b'N'),  // 0x31
    (b'm', b'M'),  // 0x32
    (b',', b'<'),  // 0x33
    (b'.', b'>'),  // 0x34
    (b'/', b'?'),  // 0x35
    (0, 0),        // 0x36 Right Shift
    (0, 0),        // 0x37 keypad *
    (0, 0),        // 0x38 Left Alt
    (b' ', b' '),  // 0x39 Space
];

/// Turns the raw scancode byte stream into `ConsoleKey`s.
#[derive(Debug, Default)]
pub struct Decoder {
    left_shift: bool,
    right_shift: bool,
    /// The previous byte was the `0xE0` prefix, so this one belongs to an
    /// extended key.
    expecting_extended: bool,
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            left_shift: false,
            right_shift: false,
            expecting_extended: false,
        }
    }

    /// Feeds one scancode byte. Returns the key it completes, if any: key
    /// *releases*, the `0xE0` prefix and Shift itself all return `None`
    /// (they only change decoder state).
    pub fn feed(&mut self, scancode: u8) -> Option<ConsoleKey> {
        if scancode == PREFIX_EXTENDED {
            self.expecting_extended = true;
            return None;
        }
        let extended = core::mem::take(&mut self.expecting_extended);
        let released = scancode & BREAK_BIT != 0;
        let make = scancode & !BREAK_BIT;

        if extended {
            // No extended key is mapped. Swallowing the whole pair here
            // also keeps the "fake Shift" (`E0 2A` / `E0 AA`) that some
            // keyboards wrap around navigation keys from being mistaken
            // for a real Shift press.
            return if released {
                None
            } else {
                Some(ConsoleKey::Unknown)
            };
        }

        match make {
            SC_LEFT_SHIFT => {
                self.left_shift = !released;
                None
            }
            SC_RIGHT_SHIFT => {
                self.right_shift = !released;
                None
            }
            _ if released => None,
            SC_BACKSPACE => Some(ConsoleKey::Backspace),
            SC_ENTER => Some(ConsoleKey::Enter),
            _ => {
                let shifted = self.left_shift || self.right_shift;
                Some(match KEY_TABLE.get(make as usize) {
                    Some(&(unshifted, shifted_char)) if unshifted != 0 => {
                        ConsoleKey::Char(if shifted { shifted_char } else { unshifted } as char)
                    }
                    _ => ConsoleKey::Unknown,
                })
            }
        }
    }
}

/// Consumer-side handle: drains the IRQ handler's queue and decodes it.
/// Hold exactly one — the queue is single-consumer.
#[derive(Debug, Default)]
pub struct Keyboard {
    decoder: Decoder,
}

impl Keyboard {
    pub const fn new() -> Self {
        Self {
            decoder: Decoder::new(),
        }
    }

    /// Non-blocking. Returns the next decoded key, or `None` if the queue
    /// holds no complete key right now.
    pub fn read_key(&mut self) -> Option<ConsoleKey> {
        self.read_key_from(&SCANCODES)
    }

    fn read_key_from(&mut self, queue: &ScancodeQueue) -> Option<ConsoleKey> {
        while let Some(scancode) = queue.pop() {
            if let Some(key) = self.decoder.feed(scancode) {
                return Some(key);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------
// i8042 controller
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// The controller never became ready within `POLL_LIMIT` status reads:
    /// absent, wedged, or not an i8042 at these ports.
    ControllerTimeout,
}

/// # Safety
///
/// Ports `0x60`/`0x64` must be a real i8042 (true on the `pc` machine type
/// `xtask` targets). Interrupts must be disabled and IRQ1 masked at the
/// PIC: this polls the very output buffer the IRQ1 handler consumes, so the
/// two must not run concurrently. Single-core, called once.
unsafe fn wait_input_ready() -> Result<(), InitError> {
    for _ in 0..POLL_LIMIT {
        // SAFETY: per this function's contract.
        if unsafe { inb(STATUS_COMMAND_PORT) } & STATUS_INPUT_FULL == 0 {
            return Ok(());
        }
    }
    Err(InitError::ControllerTimeout)
}

/// # Safety
///
/// Same contract as `wait_input_ready`.
unsafe fn wait_output_ready() -> Result<u8, InitError> {
    for _ in 0..POLL_LIMIT {
        // SAFETY: per this function's contract.
        if unsafe { inb(STATUS_COMMAND_PORT) } & STATUS_OUTPUT_FULL != 0 {
            // SAFETY: the status read just above says a byte is waiting.
            return Ok(unsafe { inb(DATA_PORT) });
        }
    }
    Err(InitError::ControllerTimeout)
}

/// Discards whatever the firmware (or a keypress before we took over) left
/// in the output buffer.
///
/// # Safety
///
/// Same contract as `wait_input_ready`.
unsafe fn drain_output_buffer() {
    for _ in 0..DRAIN_LIMIT {
        // SAFETY: per this function's contract.
        unsafe {
            if inb(STATUS_COMMAND_PORT) & STATUS_OUTPUT_FULL == 0 {
                return;
            }
            let _ = inb(DATA_PORT);
        }
    }
}

/// Puts the i8042 and the keyboard behind it into a known state: keyboard
/// interrupts enabled, keyboard clock on, scancode translation on (so the
/// stream is set 1, which `Decoder` expects), and scanning enabled.
///
/// Does not touch the PIC or the IDT — the caller unmasks IRQ1 afterwards.
///
/// # Safety
///
/// Same contract as `wait_input_ready`.
pub unsafe fn init_controller() -> Result<(), InitError> {
    // SAFETY: every block below relies on this function's own contract.
    unsafe {
        drain_output_buffer();

        wait_input_ready()?;
        outb(STATUS_COMMAND_PORT, COMMAND_READ_CONFIG);
        let config = wait_output_ready()?;

        let new_config = (config | CONFIG_KEYBOARD_IRQ_ENABLE | CONFIG_TRANSLATE)
            & !CONFIG_KEYBOARD_CLOCK_DISABLE;
        wait_input_ready()?;
        outb(STATUS_COMMAND_PORT, COMMAND_WRITE_CONFIG);
        wait_input_ready()?;
        outb(DATA_PORT, new_config);

        // Firmware normally leaves scanning enabled, but nothing
        // guarantees it, and the command is idempotent. A missing or odd
        // answer is only a warning: a keyboard that was already scanning
        // works fine without our help.
        wait_input_ready()?;
        outb(DATA_PORT, KEYBOARD_ENABLE_SCANNING);
        match wait_output_ready() {
            Ok(KEYBOARD_ACK) => {}
            Ok(other) => log::warn!("HARLAN: keyboard answered {other:#04x} to enable-scanning"),
            Err(_) => log::warn!("HARLAN: keyboard did not answer enable-scanning"),
        }

        // The ACK above, and any byte that raced in, would otherwise be
        // decoded as a key once IRQ1 is unmasked.
        drain_output_buffer();

        log::info!("HARLAN: PS/2 keyboard ready (i8042 config {config:#04x} -> {new_config:#04x})");
    }
    Ok(())
}

/// IRQ1 handler body: moves every scancode the controller has ready onto
/// `SCANCODES`.
///
/// Checks the status register instead of trusting that the IRQ meant "a
/// byte is waiting": an IRQ1 edge latched while the line was masked (the
/// enable-scanning ACK does exactly this) is delivered after unmasking with
/// nothing to read, and reading `0x60` anyway would return a stale byte —
/// a phantom keypress.
///
/// # Safety
///
/// Must only be called from the IRQ1 interrupt handler, and only one
/// instance may run at a time (true by construction: interrupt gates clear
/// IF, and this is a single-core kernel).
pub unsafe fn on_irq() {
    for _ in 0..DRAIN_LIMIT {
        // SAFETY: per this function's contract; `0x64`/`0x60` are the
        // i8042's fixed ports.
        let status = unsafe { inb(STATUS_COMMAND_PORT) };
        if status & STATUS_OUTPUT_FULL == 0 {
            return;
        }
        // SAFETY: the status read just above says a byte is waiting.
        let byte = unsafe { inb(DATA_PORT) };
        // Mouse bytes have no consumer (IRQ12 stays masked), but must
        // still be read out or they would block the keyboard's.
        if status & STATUS_AUX_DATA == 0 && !SCANCODES.push(byte) {
            log::warn!("HARLAN: keyboard queue full, dropped scancode {byte:#04x}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ScancodeQueue -------------------------------------------------

    #[test]
    fn empty_queue_pops_nothing() {
        assert_eq!(ScancodeQueue::new().pop(), None);
    }

    #[test]
    fn queue_is_first_in_first_out() {
        let q = ScancodeQueue::new();
        for byte in [1, 2, 3] {
            assert!(q.push(byte));
        }
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn full_queue_rejects_new_bytes_and_keeps_the_old_ones() {
        let q = ScancodeQueue::new();
        for i in 0..QUEUE_CAPACITY {
            assert!(q.push(i as u8));
        }
        assert!(!q.push(0xFF), "a full queue must refuse, not overwrite");
        for i in 0..QUEUE_CAPACITY {
            assert_eq!(q.pop(), Some(i as u8));
        }
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn queue_survives_many_wraps_of_its_slot_array() {
        let q = ScancodeQueue::new();
        // Keep it half full while cycling ~50x through the slot array.
        for i in 0..QUEUE_CAPACITY / 2 {
            assert!(q.push(i as u8));
        }
        for i in 0..QUEUE_CAPACITY * 50 {
            assert!(q.push((i + QUEUE_CAPACITY / 2) as u8));
            assert_eq!(q.pop(), Some(i as u8));
        }
    }

    #[test]
    fn a_slot_freed_by_pop_can_be_pushed_again_when_full() {
        let q = ScancodeQueue::new();
        for i in 0..QUEUE_CAPACITY {
            assert!(q.push(i as u8));
        }
        assert_eq!(q.pop(), Some(0));
        assert!(q.push(0xAA));
        assert!(!q.push(0xBB));
    }

    // ---- Decoder -------------------------------------------------------

    fn decode(bytes: &[u8]) -> Vec<ConsoleKey> {
        let mut d = Decoder::new();
        bytes.iter().filter_map(|&b| d.feed(b)).collect()
    }

    #[test]
    fn letters_digits_and_space_decode_unshifted() {
        // h e l l o SPACE 1
        assert_eq!(
            decode(&[0x23, 0x12, 0x26, 0x26, 0x18, 0x39, 0x02]),
            vec![
                ConsoleKey::Char('h'),
                ConsoleKey::Char('e'),
                ConsoleKey::Char('l'),
                ConsoleKey::Char('l'),
                ConsoleKey::Char('o'),
                ConsoleKey::Char(' '),
                ConsoleKey::Char('1'),
            ]
        );
    }

    #[test]
    fn enter_and_backspace_have_their_own_variants() {
        assert_eq!(
            decode(&[0x1C, 0x0E]),
            vec![ConsoleKey::Enter, ConsoleKey::Backspace]
        );
    }

    #[test]
    fn key_releases_produce_nothing() {
        // 'a' down, 'a' up (0x1E | 0x80)
        assert_eq!(decode(&[0x1E, 0x9E]), vec![ConsoleKey::Char('a')]);
    }

    #[test]
    fn shift_makes_uppercase_and_symbols_only_while_held() {
        // LShift down, 'a', '1', LShift up, 'a', '1'
        assert_eq!(
            decode(&[0x2A, 0x1E, 0x02, 0xAA, 0x1E, 0x02]),
            vec![
                ConsoleKey::Char('A'),
                ConsoleKey::Char('!'),
                ConsoleKey::Char('a'),
                ConsoleKey::Char('1'),
            ]
        );
    }

    #[test]
    fn right_shift_works_and_the_two_shifts_are_independent() {
        // RShift down, LShift down, LShift up, 'a' (RShift still held),
        // RShift up, 'a'
        assert_eq!(
            decode(&[0x36, 0x2A, 0xAA, 0x1E, 0xB6, 0x1E]),
            vec![ConsoleKey::Char('A'), ConsoleKey::Char('a')]
        );
    }

    /// The rows of a US keyboard, in scancode order, checked against the
    /// physical layout independently of how `KEY_TABLE` is written — so a
    /// table that is internally consistent but wrong (a swapped pair, an
    /// off-by-one row) cannot pass.
    #[test]
    fn keyboard_rows_decode_to_the_physical_us_layout() {
        let rows: [(u8, &str, &str); 6] = [
            (0x02, "1234567890-=", "!@#$%^&*()_+"),
            (0x10, "qwertyuiop[]", "QWERTYUIOP{}"),
            (0x1E, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
            (0x2B, "\\", "|"),
            (0x2C, "zxcvbnm,./", "ZXCVBNM<>?"),
            (0x39, " ", " "),
        ];
        for (first_code, unshifted, shifted) in rows {
            for (i, (lo, hi)) in unshifted.chars().zip(shifted.chars()).enumerate() {
                let code = first_code + i as u8;
                assert_eq!(
                    decode(&[code]),
                    vec![ConsoleKey::Char(lo)],
                    "code {code:#x}"
                );
                assert_eq!(
                    decode(&[SC_LEFT_SHIFT, code]),
                    vec![ConsoleKey::Char(hi)],
                    "code {code:#x} shifted"
                );
            }
        }
    }

    #[test]
    fn every_table_entry_is_shell_typable_ascii() {
        let mut printable = 0;
        for &(lo, hi) in KEY_TABLE.iter().filter(|&&(lo, _)| lo != 0) {
            printable += 1;
            assert!(lo.is_ascii_graphic() || lo == b' ', "{lo:#x}");
            assert!(hi.is_ascii_graphic() || hi == b' ', "{hi:#x}");
        }
        // 26 letters + 10 digits + 11 symbols + space.
        assert_eq!(printable, 48);
    }

    #[test]
    fn keys_without_a_mapping_are_unknown_not_silent() {
        // Esc, Tab, Left Ctrl, Left Alt, Caps Lock, F1
        assert_eq!(
            decode(&[0x01, 0x0F, 0x1D, 0x38, 0x3A, 0x3B]),
            vec![ConsoleKey::Unknown; 6]
        );
    }

    #[test]
    fn extended_keys_are_unknown_and_their_releases_are_silent() {
        // Up arrow: E0 48 (down), E0 C8 (up)
        assert_eq!(decode(&[0xE0, 0x48, 0xE0, 0xC8]), vec![ConsoleKey::Unknown]);
    }

    #[test]
    fn extended_prefix_does_not_let_the_next_byte_act_as_a_normal_key() {
        // E0 1C is keypad Enter, E0 2A a "fake shift": neither may be
        // decoded as the plain key/modifier with the same low byte.
        assert_eq!(decode(&[0xE0, 0x1C]), vec![ConsoleKey::Unknown]);
        assert_eq!(
            decode(&[0xE0, 0x2A, 0x1E, 0xE0, 0xAA]),
            vec![ConsoleKey::Unknown, ConsoleKey::Char('a')],
            "a fake shift must not capitalize the next key"
        );
    }

    #[test]
    fn extended_prefix_state_does_not_leak_past_one_byte() {
        // E0 48 consumed the prefix; 'a' afterwards is a plain 'a'.
        assert_eq!(
            decode(&[0xE0, 0x48, 0x1E]),
            vec![ConsoleKey::Unknown, ConsoleKey::Char('a')]
        );
    }

    #[test]
    fn out_of_table_codes_are_unknown() {
        assert_eq!(decode(&[0x7F]), vec![ConsoleKey::Unknown]);
    }

    // ---- Keyboard (queue + decoder together) ---------------------------

    #[test]
    fn keyboard_skips_non_key_bytes_and_returns_the_first_real_key() {
        let q = ScancodeQueue::new();
        // shift down, 'h', release, 'i'
        for byte in [0x2A, 0x23, 0xA3, 0x17] {
            assert!(q.push(byte));
        }
        let mut kb = Keyboard::new();
        assert_eq!(kb.read_key_from(&q), Some(ConsoleKey::Char('H')));
        assert_eq!(kb.read_key_from(&q), Some(ConsoleKey::Char('I')));
        assert_eq!(kb.read_key_from(&q), None);
    }

    #[test]
    fn keyboard_reports_none_when_only_modifier_bytes_are_pending() {
        let q = ScancodeQueue::new();
        assert!(q.push(0x2A)); // Shift down: state only
        let mut kb = Keyboard::new();
        assert_eq!(kb.read_key_from(&q), None);
        assert!(q.push(0x1E));
        assert_eq!(
            kb.read_key_from(&q),
            Some(ConsoleKey::Char('A')),
            "decoder state must persist across calls"
        );
    }
}
