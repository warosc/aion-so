/// Deliberately separate from `Console`: resetting the machine is a
/// platform/firmware capability, not a text-I/O one.
pub trait PowerControl {
    fn reboot(&self) -> !;
    fn shutdown(&self) -> !;
}
