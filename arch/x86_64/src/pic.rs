//! Legacy 8259 PIC. Incremento 2 only masks both PICs (a safety step
//! around the `ExitBootServices` transition); the full ICW1-4 remap
//! sequence and IRQ0 unmasking are Incremento 3's job, once the PIT timer
//! and its IDT vector exist to receive it.

use crate::port::outb;

const MASTER_PIC_DATA: u16 = 0x21;
const SLAVE_PIC_DATA: u16 = 0xA1;

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
