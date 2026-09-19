/// Read-only view of a monotonic tick counter driven by a hardware timer
/// interrupt. The counter itself is only ever written by the timer's own
/// interrupt handler.
pub trait TickCounter {
    fn ticks(&self) -> u64;
}
