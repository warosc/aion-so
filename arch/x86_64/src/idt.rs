//! Minimal 64-bit IDT. Hand-written per Intel SDM Vol. 3, Figure 6-8
//! ("64-Bit IDT Gate Descriptors").

use core::mem::size_of;

pub const GATE_TYPE_INTERRUPT: u8 = 0b1110;

/// One 16-byte interrupt/trap gate descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    /// Bits 0-2: IST index (0 = don't switch stacks). Bits 3-7: reserved, 0.
    ist: u8,
    /// Present | DPL(2) | 0 (S bit) | gate type(4).
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    pub(crate) const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// `ist`: 0 for "use the current stack" (no forced switch), 1-7 to force
    /// switching to `TaskStateSegment::ist[ist - 1]`.
    pub(crate) fn new(handler: u64, selector: u16, ist: u8, gate_type: u8) -> Self {
        let present = 1u8 << 7;
        let dpl_ring0 = 0u8 << 5;
        let type_attr = present | dpl_ring0 | (gate_type & 0b1111);
        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector,
            ist: ist & 0b111,
            type_attr,
            offset_mid: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: ((handler >> 32) & 0xFFFF_FFFF) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, align(16))]
pub struct Idt(pub [IdtEntry; 256]);

impl Idt {
    pub(crate) const fn new() -> Self {
        Self([IdtEntry::missing(); 256])
    }
}

const _IDT_ENTRY_IS_16_BYTES: () = assert!(size_of::<IdtEntry>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idt_entry_is_16_bytes() {
        assert_eq!(size_of::<IdtEntry>(), 16);
    }

    #[test]
    fn new_entry_splits_handler_address_across_three_fields() {
        let entry = IdtEntry::new(0x1122_3344_5566_7788, 0x08, 0, GATE_TYPE_INTERRUPT);
        assert_eq!(entry.offset_low, 0x7788);
        assert_eq!(entry.offset_mid, 0x5566);
        assert_eq!(entry.offset_high, 0x1122_3344);
        assert_eq!(entry.selector, 0x08);
    }

    #[test]
    fn new_entry_sets_present_bit() {
        let entry = IdtEntry::new(0, 0x08, 0, GATE_TYPE_INTERRUPT);
        assert_ne!(entry.type_attr & 0x80, 0);
    }

    #[test]
    fn new_entry_encodes_gate_type_in_low_nibble() {
        let entry = IdtEntry::new(0, 0x08, 0, GATE_TYPE_INTERRUPT);
        assert_eq!(entry.type_attr & 0x0F, GATE_TYPE_INTERRUPT);
    }

    #[test]
    fn new_entry_masks_ist_to_three_bits() {
        let entry = IdtEntry::new(0, 0x08, 0xFF, GATE_TYPE_INTERRUPT);
        assert_eq!(entry.ist, 0b111);
    }

    #[test]
    fn missing_entry_has_present_bit_clear() {
        let entry = IdtEntry::missing();
        assert_eq!(entry.type_attr & 0x80, 0);
    }
}
