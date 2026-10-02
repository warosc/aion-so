//! What the CPU says it is (docs/adr/0035-fase5-hardware-inventory.md).
//!
//! `cpuid` is the one way to ask an x86 processor about itself, and the
//! answer decides things this kernel has so far assumed: whether pages can
//! be marked no-execute, whether a 1 GiB page exists, how many address bits
//! are real. On QEMU those answers have been the same every time. On a
//! machine nobody here has seen they are a question.
//!
//! The reading is one instruction; everything interesting is the decoding,
//! and the decoding is pure, so it is tested on the host against the bits
//! the manuals specify.

/// What one `cpuid` call answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Leaf {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// Asks the processor one question.
///
/// # Safety
///
/// `cpuid` is unprivileged and has no side effects, but a leaf the
/// processor does not implement answers with whatever the highest leaf it
/// does implement answers — so the caller has to have checked the maximum
/// first. That is why `read` is private and everything public here asks a
/// leaf it has already established exists.
unsafe fn cpuid(leaf: u32, sub_leaf: u32) -> Leaf {
    let (eax, ebx, ecx, edx);
    // SAFETY: `cpuid` touches no memory and no stack. `ebx` is reserved by
    // LLVM on some targets, so it is saved and restored by hand rather than
    // named as an operand.
    unsafe {
        core::arch::asm!(
            "mov {ebx_save:r}, rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "mov rbx, {ebx_save:r}",
            ebx_save = out(reg) _,
            ebx_out = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") sub_leaf => ecx,
            out("edx") edx,
            options(nostack, preserves_flags),
        );
    }
    Leaf { eax, ebx, ecx, edx }
}

/// The highest ordinary leaf the processor answers.
pub fn max_leaf() -> u32 {
    // SAFETY: leaf 0 exists on every processor that has `cpuid` at all, and
    // every x86_64 one does.
    unsafe { cpuid(0, 0) }.eax
}

/// The highest extended leaf, or zero when there are none.
pub fn max_extended_leaf() -> u32 {
    // SAFETY: leaf 0x8000_0000 answers its own availability: a processor
    // without extended leaves answers with something below 0x8000_0000.
    let answer = unsafe { cpuid(0x8000_0000, 0) }.eax;
    if answer > 0x8000_0000 { answer } else { 0 }
}

/// The twelve bytes of the vendor string, in the order the registers give
/// them: `ebx`, then `edx`, then `ecx`. Famously not alphabetical.
pub fn vendor() -> [u8; 12] {
    // SAFETY: leaf 0 always exists.
    let leaf = unsafe { cpuid(0, 0) };
    vendor_from(&leaf)
}

/// The vendor out of leaf 0, which is the part worth testing.
pub fn vendor_from(leaf: &Leaf) -> [u8; 12] {
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    bytes[4..8].copy_from_slice(&leaf.edx.to_le_bytes());
    bytes[8..12].copy_from_slice(&leaf.ecx.to_le_bytes());
    bytes
}

/// The brand string — the name a person would recognise — from the three
/// extended leaves that carry it.
///
/// All zero when the processor has no such leaves, which is its way of
/// saying it has no name to give.
pub fn brand() -> [u8; 48] {
    let mut bytes = [0u8; 48];
    if max_extended_leaf() < 0x8000_0004 {
        return bytes;
    }
    for (step, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        // SAFETY: the maximum extended leaf was just checked to reach this
        // one.
        let answer = unsafe { cpuid(leaf, 0) };
        let at = step * 16;
        bytes[at..at + 4].copy_from_slice(&answer.eax.to_le_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&answer.ebx.to_le_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&answer.ecx.to_le_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&answer.edx.to_le_bytes());
    }
    bytes
}

/// Family, model and stepping, which together name a specific processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    pub family: u16,
    pub model: u8,
    pub stepping: u8,
}

/// Decodes `eax` of leaf 1.
///
/// Family and model are each split across two fields and have to be put
/// back together — and the rule for when the extended halves count is
/// different for the two of them, which is exactly the sort of thing that
/// is wrong in a kernel for years without anybody noticing.
pub const fn signature_from(eax: u32) -> Signature {
    let base_family = ((eax >> 8) & 0xF) as u16;
    let base_model = ((eax >> 4) & 0xF) as u8;
    let extended_family = ((eax >> 20) & 0xFF) as u16;
    let extended_model = ((eax >> 16) & 0xF) as u8;

    // The extended family is added only for family 15; the extended model
    // counts for families 6 and 15. Those are the manuals' rules, not a
    // simplification.
    let family = if base_family == 0xF {
        base_family + extended_family
    } else {
        base_family
    };
    let model = if base_family == 0xF || base_family == 0x6 {
        (extended_model << 4) | base_model
    } else {
        base_model
    };
    Signature {
        family,
        model,
        stepping: (eax & 0xF) as u8,
    }
}

/// The processor's family, model and stepping.
pub fn signature() -> Signature {
    // SAFETY: leaf 1 exists wherever leaf 0 reports at least 1, which every
    // x86_64 processor does.
    signature_from(unsafe { cpuid(1, 0) }.eax)
}

/// The features this kernel cares about, and why each one matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    /// Pages can be marked no-execute. Everything in `docs/memory-safety.md`
    /// about W^X rests on this.
    pub nx: bool,
    /// 1 GiB pages. The physical window would be cheaper with them.
    pub gigabyte_pages: bool,
    /// Long mode. If this were ever false, nothing here would be running.
    pub long_mode: bool,
    /// `syscall`/`sysret`, which is how ring 3 enters this kernel
    /// (ADR 0014, point 1).
    pub syscall: bool,
    /// An APIC on the chip. The kernel still uses the 8259 PIC; this says
    /// whether it has to.
    pub apic: bool,
    /// x2APIC, which is how a machine with many cores addresses them.
    pub x2apic: bool,
    /// An invariant TSC: a timer that does not change rate with power
    /// state, and therefore one a clock could be built on.
    pub invariant_tsc: bool,
    /// The hypervisor bit. Not a capability — a statement that this is not
    /// bare metal, which is worth knowing when reading an inventory.
    pub hypervisor: bool,
}

/// Decodes the feature bits out of the leaves that carry them.
///
/// Pure, so each bit can be checked against the manuals' numbering without
/// a processor — and a bit read from the wrong register is the kind of
/// mistake that makes a kernel refuse to use something it has.
pub const fn features_from(leaf_1: &Leaf, extended_1: &Leaf, power: &Leaf) -> Features {
    Features {
        nx: extended_1.edx & (1 << 20) != 0,
        gigabyte_pages: extended_1.edx & (1 << 26) != 0,
        long_mode: extended_1.edx & (1 << 29) != 0,
        syscall: extended_1.edx & (1 << 11) != 0,
        apic: leaf_1.edx & (1 << 9) != 0,
        x2apic: leaf_1.ecx & (1 << 21) != 0,
        hypervisor: leaf_1.ecx & (1 << 31) != 0,
        invariant_tsc: power.edx & (1 << 8) != 0,
    }
}

/// What this processor can do.
pub fn features() -> Features {
    // SAFETY: leaf 1 exists everywhere.
    let leaf_1 = unsafe { cpuid(1, 0) };
    let extended = max_extended_leaf();
    // SAFETY: asked only when the processor says it answers them.
    let extended_1 = if extended >= 0x8000_0001 {
        unsafe { cpuid(0x8000_0001, 0) }
    } else {
        Leaf::default()
    };
    // SAFETY: as above.
    let power = if extended >= 0x8000_0007 {
        unsafe { cpuid(0x8000_0007, 0) }
    } else {
        Leaf::default()
    };
    features_from(&leaf_1, &extended_1, &power)
}

/// How many bits of physical and virtual address the processor really has.
///
/// `(physical, virtual)`, and `(36, 48)` when it will not say — the
/// smallest an x86_64 processor is allowed to have, which is the safe
/// answer to assume.
pub fn address_bits() -> (u8, u8) {
    if max_extended_leaf() < 0x8000_0008 {
        return (36, 48);
    }
    // SAFETY: asked only when the processor says it answers it.
    let leaf = unsafe { cpuid(0x8000_0008, 0) };
    (leaf.eax as u8, (leaf.eax >> 8) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendor bytes come out of `ebx`, `edx`, `ecx`, in that order.
    ///
    /// Not alphabetical, and getting it wrong gives a string that looks
    /// almost right — `GenuntelineI` — which is the kind of thing that gets
    /// read past.
    #[test]
    fn the_vendor_is_assembled_in_the_order_the_registers_give_it() {
        // "GenuineIntel" split the way a real processor splits it.
        let leaf = Leaf {
            eax: 0,
            ebx: u32::from_le_bytes(*b"Genu"),
            edx: u32::from_le_bytes(*b"ineI"),
            ecx: u32::from_le_bytes(*b"ntel"),
        };
        assert_eq!(&vendor_from(&leaf), b"GenuineIntel");

        // And AMD's.
        let leaf = Leaf {
            eax: 0,
            ebx: u32::from_le_bytes(*b"Auth"),
            edx: u32::from_le_bytes(*b"enti"),
            ecx: u32::from_le_bytes(*b"cAMD"),
        };
        assert_eq!(&vendor_from(&leaf), b"AuthenticAMD");
    }

    /// Family and model are each split in two, and the rule for putting
    /// them back together is different for each.
    #[test]
    fn the_signature_is_put_back_together_the_way_the_manuals_say() {
        // Family 6, model 0x3C (a Haswell), stepping 3. Family 6 takes the
        // extended model but not the extended family.
        let eax = (0x3 << 16) | (0x6 << 8) | (0xC << 4) | 0x3;
        assert_eq!(
            signature_from(eax),
            Signature {
                family: 6,
                model: 0x3C,
                stepping: 3
            }
        );

        // Family 15 takes both: base 0xF plus extended 0x8 is 23, and the
        // model halves join.
        let eax = (0x08 << 20) | (0x1 << 16) | (0xF << 8) | (0x1 << 4) | 0x2;
        assert_eq!(
            signature_from(eax),
            Signature {
                family: 23,
                model: 0x11,
                stepping: 2
            }
        );

        // And a family below 6 takes neither, so the extended fields are
        // ignored however they are set.
        let eax = (0xFF << 20) | (0xF << 16) | (0x4 << 8) | (0x7 << 4) | 0x1;
        assert_eq!(
            signature_from(eax),
            Signature {
                family: 4,
                model: 7,
                stepping: 1
            }
        );
    }

    /// Each feature comes from the register and bit the manuals name.
    ///
    /// Asserted one at a time, from a leaf with only that bit set, so a bit
    /// read from the wrong register cannot hide behind another that happens
    /// to be right.
    #[test]
    fn every_feature_is_the_bit_the_manuals_name() {
        let none = Leaf::default();
        let bit = |register: usize, bit: u32| {
            let mut leaf = Leaf::default();
            match register {
                2 => leaf.ecx = 1 << bit,
                _ => leaf.edx = 1 << bit,
            }
            leaf
        };

        assert!(features_from(&none, &bit(3, 20), &none).nx);
        assert!(features_from(&none, &bit(3, 26), &none).gigabyte_pages);
        assert!(features_from(&none, &bit(3, 29), &none).long_mode);
        assert!(features_from(&none, &bit(3, 11), &none).syscall);
        assert!(features_from(&bit(3, 9), &none, &none).apic);
        assert!(features_from(&bit(2, 21), &none, &none).x2apic);
        assert!(features_from(&bit(2, 31), &none, &none).hypervisor);
        assert!(features_from(&none, &none, &bit(3, 8)).invariant_tsc);

        // Nothing set is nothing claimed.
        assert_eq!(features_from(&none, &none, &none), Features::default());
    }

    /// The feature bits of leaf 1 and of extended leaf 1 are different
    /// registers on different leaves, and swapping them is the easy
    /// mistake: both are called "leaf 1" in conversation.
    #[test]
    fn the_two_leaf_ones_are_not_interchangeable() {
        let nx_in_extended = Leaf {
            edx: 1 << 20,
            ..Leaf::default()
        };
        // In the extended leaf it is NX.
        assert!(features_from(&Leaf::default(), &nx_in_extended, &Leaf::default()).nx);
        // In the ordinary leaf, bit 20 of edx is something else entirely,
        // and must not be read as NX.
        assert!(!features_from(&nx_in_extended, &Leaf::default(), &Leaf::default()).nx);
    }
}
