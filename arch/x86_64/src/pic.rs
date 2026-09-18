//! Legacy 8259 PIC.

use crate::port::outb;

const MASTER_PIC_CMD: u16 = 0x20;
const MASTER_PIC_DATA: u16 = 0x21;
const SLAVE_PIC_CMD: u16 = 0xA0;
const SLAVE_PIC_DATA: u16 = 0xA1;

const ICW1_INIT_EXPECT_ICW4: u8 = 0x11;
const ICW4_8086_MODE: u8 = 0x01;
/// Non-specific End-Of-Interrupt command.
const EOI: u8 = 0x20;

/// Masks every IRQ line on both the master and slave 8259 PICs.
///
/// # Safety
///
/// Writing `0xFF` to the PIC's data/mask ports only reduces which IRQ
/// lines can reach the CPU — it is a self-contained hardware write with no
/// addressable-memory side effect, and cannot corrupt state regardless of
/// the PIC's prior configuration (masked, unmasked, or even not yet
/// remapped from its BIOS-default state). `0x21`/`0xA1` are fixed legacy
/// ISA addresses guaranteed present on the `pc` machine type `xtask`
/// already targets for its PIIX3 IDE controller.
pub unsafe fn mask_all() {
    unsafe {
        outb(MASTER_PIC_DATA, 0xFF);
        outb(SLAVE_PIC_DATA, 0xFF);
    }
}

/// Remaps both PICs off the CPU-exception vector range (0-31) and onto
/// 0x20-0x2F, then leaves only IRQ0 (the PIT timer) unmasked.
///
/// # Safety
///
/// The caller must hold interrupts disabled (`cli`) for the entire ICW1-4
/// sequence: an IRQ arriving mid-sequence would be routed through a
/// half-configured vector mapping. The caller must also have already
/// installed a real IDT handler for vector 0x20 (the timer) before this
/// returns and interrupts are next enabled — this function unmasks IRQ0
/// as its last step, and a masked-but-unhandled vector is far safer than
/// an unmasked-and-unhandled one.
pub unsafe fn remap() {
    unsafe {
        // ICW1: begin initialization on both PICs, ICW4 will follow.
        outb(MASTER_PIC_CMD, ICW1_INIT_EXPECT_ICW4);
        outb(SLAVE_PIC_CMD, ICW1_INIT_EXPECT_ICW4);
        // ICW2: vector offsets — IRQ0-7 -> 0x20-0x27, IRQ8-15 -> 0x28-0x2F.
        // This is the whole point of remapping: the legacy BIOS-default
        // mapping (IRQ0-7 -> 0x08-0x0F) collides with CPU exception
        // vectors (0x08 is #DF).
        outb(MASTER_PIC_DATA, 0x20);
        outb(SLAVE_PIC_DATA, 0x28);
        // ICW3: cascade wiring — tell the master a slave lives on IRQ2
        // (bit mask, 0x04 = bit 2), tell the slave its own cascade
        // identity (2, not a bit mask on this side).
        outb(MASTER_PIC_DATA, 0x04);
        outb(SLAVE_PIC_DATA, 0x02);
        // ICW4: 8086/88 mode on both.
        outb(MASTER_PIC_DATA, ICW4_8086_MODE);
        outb(SLAVE_PIC_DATA, ICW4_8086_MODE);
        // Final mask state (OCW1): unmask only IRQ0 on the master: every
        // other line, including the master's own IRQ2-to-slave cascade
        // line, stays masked — nothing has an IDT handler beyond the
        // timer yet. Fully mask the slave: no slave-routed IRQ is used in
        // Fase 2.
        outb(MASTER_PIC_DATA, 0b1111_1110);
        outb(SLAVE_PIC_DATA, 0xFF);
    }
}

/// Sends a non-specific End-Of-Interrupt to the master PIC.
///
/// # Safety
///
/// Must only be called from within a real, currently-executing interrupt
/// handler for an IRQ routed through the master PIC (IRQ0-7) — sending an
/// EOI with no interrupt actually in service can desynchronize the PIC's
/// internal priority state, causing future interrupts to be misrouted or
/// dropped.
pub unsafe fn send_eoi() {
    unsafe {
        outb(MASTER_PIC_CMD, EOI);
    }
}
