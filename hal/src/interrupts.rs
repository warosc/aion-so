/// Architecture-independent critical-section primitive. Fase 2 introduces
/// real hardware interrupts; anything that must not be interrupted midway
/// (the kernel heap lock, the keyboard ring buffer) uses this instead of
/// inventing its own disable/enable pattern.
pub trait InterruptControl {
    /// Disables interrupts and returns whether they were enabled before this
    /// call, so callers can restore the prior state instead of
    /// unconditionally re-enabling (which would be wrong inside a nested
    /// critical section).
    fn disable(&self) -> bool;

    fn enable(&self);

    fn are_enabled(&self) -> bool;
}
