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
