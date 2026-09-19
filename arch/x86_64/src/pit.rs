//! Intel 8253/8254 Programmable Interval Timer, channel 0 only.

use crate::port::outb;

const CHANNEL_0_DATA: u16 = 0x40;
const COMMAND: u16 = 0x43;
/// Channel 0 (bits 7-6 = 00), lobyte/hibyte access (bits 5-4 = 11),
/// mode 3 "square wave generator" (bits 3-1 = 011), binary not BCD
/// (bit 0 = 0).
const COMMAND_CHANNEL0_MODE3: u8 = 0b0011_0110;
/// The PIT's fixed input clock frequency (Intel 8253/8254 datasheet).
const PIT_INPUT_FREQUENCY_HZ: u32 = 1_193_182;

/// Programs PIT channel 0 to fire at approximately `frequency_hz`.
///
/// # Safety
///
/// Must be called with interrupts disabled, and only after the timer's
/// IDT vector (0x20) is already installed and the PIC has been remapped
/// so that vector is actually reachable — otherwise the first tick, which
/// can arrive as soon as interrupts are next enabled, has nowhere correct
/// to go.
pub unsafe fn init(frequency_hz: u32) {
    let divisor = (PIT_INPUT_FREQUENCY_HZ / frequency_hz) as u16;
    // SAFETY: delegated to this function's own contract above. Writing
    // the command byte first, then both divisor bytes in the
    // lobyte/hibyte order the command byte itself selected, is the
    // documented, only-valid programming sequence for this chip.
    unsafe {
        outb(COMMAND, COMMAND_CHANNEL0_MODE3);
        outb(CHANNEL_0_DATA, (divisor & 0xFF) as u8);
        outb(CHANNEL_0_DATA, (divisor >> 8) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisor_for_100hz_matches_hand_computed_value() {
        // 1_193_182 / 100 = 11931.82, truncated by integer division.
        assert_eq!(PIT_INPUT_FREQUENCY_HZ / 100, 11931);
    }
}
