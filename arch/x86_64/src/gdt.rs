//! Minimal 64-bit GDT + TSS. Hand-written, no external crate: in long mode
//! the base/limit of code and data segments are architecturally ignored, so
//! this only needs to get the access-byte and flags bits right per Intel SDM
//! Vol. 3, §3.4.5 ("Segment Descriptors") and §7.2.3 ("TSS Descriptor in
//! 64-bit mode").

use core::mem::size_of;

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const TSS_SELECTOR: u16 = 0x18;

/// Index into `TaskStateSegment::ist` (IST1) reserved for the double-fault
/// handler, so it always runs on a known-good stack even if the current
/// stack is the one that's corrupted or overflowed.
pub const DOUBLE_FAULT_IST_INDEX: u8 = 1;

const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

/// Builds a flat (base=0, limit=max) code or data segment descriptor.
///
/// `executable` selects code vs. data (Intel SDM Table 3-1 "Code- and
/// Data-Segment Types"). `writable` is the Read/Write bit: for code
/// segments this means "readable" (never used as writable code), for data
/// segments it means "writable". `long_mode` sets the L bit (only
/// meaningful for code segments; must be 0 whenever `size32` is 1, since
/// L and D/B are mutually exclusive per the SDM).
pub(crate) const fn flat_descriptor(executable: bool, writable: bool, long_mode: bool) -> u64 {
    let limit_low = 0xFFFFu64;
    let limit_high = 0xFu64;

    let accessed = 0u64;
    let type_bits = ((executable as u64) << 3) | ((writable as u64) << 1) | accessed;
    let descriptor_type_is_code_or_data = 1u64 << 4; // S bit
    let dpl_ring0 = 0u64 << 5;
    let present = 1u64 << 7;
    let access_byte = type_bits | descriptor_type_is_code_or_data | dpl_ring0 | present;

    let avl = 0u64;
    let l = (long_mode as u64) << 1;
    let size32 = (!long_mode as u64) << 2; // D/B: set for the data segment, clear for the 64-bit code segment
    let granularity_4k = 1u64 << 3;
    let flags_nibble = avl | l | size32 | granularity_4k;

    limit_low | (access_byte << 40) | (limit_high << 48) | (flags_nibble << 52)
}

/// Builds the two 8-byte halves of a 64-bit TSS descriptor (Intel SDM
/// Figure 7-4). Not `const fn`: the TSS's address is only known at runtime
/// (pointer-to-integer casts of `'static` addresses aren't foldable in
/// const-eval), so this runs once during `init()`.
fn tss_descriptor(tss_addr: u64, tss_size: u64) -> (u64, u64) {
    let limit = tss_size - 1;
    let base_low16 = tss_addr & 0xFFFF;
    let base_mid8 = (tss_addr >> 16) & 0xFF;
    let base_high8 = (tss_addr >> 24) & 0xFF;
    let base_upper32 = (tss_addr >> 32) & 0xFFFF_FFFF;

    let ty_available_64bit_tss = 0b1001u64;
    let system_descriptor = 0u64 << 4; // S bit = 0 for system descriptors
    let dpl_ring0 = 0u64 << 5;
    let present = 1u64 << 7;
    let access_byte = ty_available_64bit_tss | system_descriptor | dpl_ring0 | present;

    let limit_low = limit & 0xFFFF;
    let limit_high = (limit >> 16) & 0xF;
    let flags_nibble = 0u64; // AVL=0, G=0: our TSS is far smaller than 4 KiB

    let low = limit_low
        | (base_low16 << 16)
        | (base_mid8 << 32)
        | (access_byte << 40)
        | (limit_high << 48)
        | (flags_nibble << 52)
        | (base_high8 << 56);
    let high = base_upper32;
    (low, high)
}

/// 64-bit Task State Segment (Intel SDM Figure 7-11). Only `ist[0]` (IST1)
/// is populated; `rsp0`/`rsp1`/`rsp2` and the I/O permission bitmap are
/// unused in Fase 2 (no ring transitions, no port-level I/O restrictions
/// yet).
#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// Points past the end of the structure: "no I/O permission bitmap
    /// present", per Intel SDM §7.2.1.
    iomap_base: u16,
}

const _TSS_SIZE_IS_104_BYTES: () = assert!(size_of::<TaskStateSegment>() == 104);

#[repr(C, align(16))]
struct DoubleFaultStack([u8; DOUBLE_FAULT_STACK_SIZE]);

static mut DOUBLE_FAULT_STACK: DoubleFaultStack = DoubleFaultStack([0; DOUBLE_FAULT_STACK_SIZE]);

static mut TSS: TaskStateSegment = TaskStateSegment {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: size_of::<TaskStateSegment>() as u16,
};

#[repr(C)]
struct Gdt {
    null: u64,
    kernel_code: u64,
    kernel_data: u64,
    tss_low: u64,
    tss_high: u64,
}

static mut GDT: Gdt = Gdt {
    null: 0,
    kernel_code: flat_descriptor(true, true, true),
    kernel_data: flat_descriptor(false, true, false),
    tss_low: 0,
    tss_high: 0,
};

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Builds the GDT/TSS, loads them, and switches every segment register to
/// the new flat selectors.
///
/// # Safety
///
/// Must be called exactly once, before interrupts are re-enabled (the
/// caller, `interrupts::init`, disables interrupts first) and before any
/// code relies on `DOUBLE_FAULT_IST_INDEX` being wired up in the IDT. Not
/// safe to call concurrently from multiple cores — this is a single-core
/// kernel in Fase 2, so that does not apply yet.
pub unsafe fn init() {
    // SAFETY: single-threaded init, called exactly once before interrupts
    // are enabled; no concurrent access to these statics is possible.
    unsafe {
        let stack_top = core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK.0)
            .cast::<u8>()
            .add(DOUBLE_FAULT_STACK_SIZE) as u64;
        TSS.ist[(DOUBLE_FAULT_IST_INDEX - 1) as usize] = stack_top;

        let tss_addr = core::ptr::addr_of!(TSS) as u64;
        let (tss_low, tss_high) = tss_descriptor(tss_addr, size_of::<TaskStateSegment>() as u64);
        GDT.tss_low = tss_low;
        GDT.tss_high = tss_high;

        let gdt_ptr = DescriptorTablePointer {
            limit: (size_of::<Gdt>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
        };

        // SAFETY: `gdt_ptr` points to a `'static` GDT fully initialized
        // above, with a correctly computed limit. `lgdt` only loads the
        // GDTR; it does not itself validate selectors, so it cannot fault.
        // Reloading every segment register to the new flat selectors
        // immediately after is required because the CPU does not
        // implicitly reload CS/DS/etc. from the new table. The far
        // `retfq`-style reload of CS is done via a far `push`+`retfq`
        // trampoline, the standard technique for reloading CS in 64-bit
        // mode (there is no direct `mov cs, reg`/`ljmp` short form usable
        // from Rust inline asm here).
        core::arch::asm!(
            "lgdt [{gdt_ptr}]",
            "push {code_sel}",
            "lea {tmp}, [55f + rip]",
            "push {tmp}",
            "retfq",
            "55:",
            "mov ax, {data_sel:x}",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            "ltr {tss_sel:x}",
            gdt_ptr = in(reg) &gdt_ptr,
            code_sel = const KERNEL_CODE_SELECTOR as u64,
            data_sel = in(reg) KERNEL_DATA_SELECTOR,
            tss_sel = in(reg) TSS_SELECTOR,
            tmp = out(reg) _,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extracts byte `n` (0 = least significant) from a descriptor value,
    /// so field-level assertions don't depend on hand-transcribing a full
    /// 16-hex-digit literal (an easy way to introduce an untested typo).
    fn byte_at(value: u64, n: u32) -> u64 {
        (value >> (n * 8)) & 0xFF
    }

    #[test]
    fn code_segment_matches_known_good_reference_byte_by_byte() {
        // Reference layout for a flat, present, ring-0, 64-bit code segment
        // (readable, executable), per the well-known 0x00AF9A000000FFFF
        // used throughout OS-dev long-mode bring-up code: limit=0xFFFF,
        // base=0, access=0x9A, flags+limit_high=0xAF, base_high=0x00.
        let desc = flat_descriptor(true, true, true);
        assert_eq!(byte_at(desc, 0), 0xFF, "limit byte 0");
        assert_eq!(byte_at(desc, 1), 0xFF, "limit byte 1");
        assert_eq!(byte_at(desc, 2), 0x00, "base byte 0");
        assert_eq!(byte_at(desc, 3), 0x00, "base byte 1");
        assert_eq!(byte_at(desc, 4), 0x00, "base byte 2");
        assert_eq!(byte_at(desc, 5), 0x9A, "access byte");
        assert_eq!(byte_at(desc, 6), 0xAF, "flags nibble + limit high nibble");
        assert_eq!(byte_at(desc, 7), 0x00, "base byte 3");
    }

    #[test]
    fn data_segment_matches_known_good_reference_byte_by_byte() {
        // Reference: 0x00CF92000000FFFF (access=0x92, flags+limit_high=0xCF).
        let desc = flat_descriptor(false, true, false);
        assert_eq!(byte_at(desc, 0), 0xFF, "limit byte 0");
        assert_eq!(byte_at(desc, 1), 0xFF, "limit byte 1");
        assert_eq!(byte_at(desc, 5), 0x92, "access byte");
        assert_eq!(byte_at(desc, 6), 0xCF, "flags nibble + limit high nibble");
    }

    #[test]
    fn code_segment_present_bit_is_set() {
        let desc = flat_descriptor(true, true, true);
        assert_ne!(desc & (1 << 47), 0, "present bit must be set");
    }

    #[test]
    fn code_segment_long_mode_bit_is_set_and_size32_is_clear() {
        let desc = flat_descriptor(true, true, true);
        assert_ne!(
            desc & (1 << 53),
            0,
            "L bit must be set for the 64-bit code segment"
        );
        assert_eq!(desc & (1 << 54), 0, "D/B bit must be clear when L is set");
    }

    #[test]
    fn data_segment_long_mode_bit_is_clear() {
        let desc = flat_descriptor(false, true, false);
        assert_eq!(
            desc & (1 << 53),
            0,
            "L bit is meaningless for data segments and must be 0"
        );
    }

    #[test]
    fn tss_descriptor_encodes_address_and_limit() {
        let (low, high) = tss_descriptor(0x1122_3344_5566_7788, 104);
        assert_eq!(low & 0xFFFF, 103, "limit must be size - 1");
        assert_eq!((low >> 16) & 0xFFFF, 0x7788, "base bits 15:0");
        assert_eq!((low >> 32) & 0xFF, 0x66, "base bits 23:16");
        assert_eq!((low >> 56) & 0xFF, 0x55, "base bits 31:24");
        assert_eq!(high, 0x1122_3344, "base bits 63:32");
        assert_ne!(low & (1 << 47), 0, "present bit must be set");
        assert_eq!(
            (low >> 40) & 0xF,
            0b1001,
            "type must be 'available 64-bit TSS'"
        );
    }

    #[test]
    fn task_state_segment_is_104_bytes() {
        assert_eq!(size_of::<TaskStateSegment>(), 104);
    }
}
