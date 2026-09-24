/// Reads a byte from an x86 I/O port.
///
/// # Safety
///
/// The caller must ensure reading `port` is safe in the current hardware
/// context: some ports have read side effects (reading the PS/2 data port
/// `0x60` pops the controller's output buffer, discarding whatever was in
/// it), and this function cannot check that a given read is appropriate.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: delegated to the caller's contract above; `in` itself has no
    // memory or stack effects. Not marked `pure`, so the compiler still
    // treats every call as having a side effect and neither drops nor
    // duplicates it — which matters for status-polling loops.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// Writes a byte to an x86 I/O port.
///
/// # Safety
///
/// The caller must ensure `port` is safe to write `value` to in the
/// current hardware context — writing to an arbitrary I/O port can have
/// arbitrary hardware side effects that this function cannot check.
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: delegated to the caller's contract above; `out` itself has
    // no memory or stack effects.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Reads a doubleword from an x86 I/O port.
///
/// # Safety
///
/// As `inb`: the caller must know that reading `port` is appropriate here.
/// The only user in this kernel is PCI configuration space, which is read
/// four bytes at a time by construction
/// (docs/adr/0022-fase4-pci-enumeration.md).
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: delegated to the caller's contract above; `in` itself has no
    // memory or stack effects, and is not marked `pure` for the same
    // reason as `inb`.
    unsafe {
        core::arch::asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// Writes a doubleword to an x86 I/O port.
///
/// # Safety
///
/// As `outb`.
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: delegated to the caller's contract above; `out` itself has
    // no memory or stack effects.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}
