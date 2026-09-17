/// Architecture-independent CPU control surface. Intentionally minimal for
/// Fase 0; Fase 2 extends this once GDT/IDT/interrupt work begins.
pub trait CpuControl {
    /// Parks the core forever. Never returns.
    fn halt_loop(&self) -> !;

    /// Parks the core until the next interrupt, then returns. Used by the
    /// Fase 1 shell to idle instead of busy-spinning while polling for
    /// keystrokes; superseded by real interrupt-driven input in Fase 2.
    fn halt_once(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCpu;

    impl CpuControl for FakeCpu {
        fn halt_loop(&self) -> ! {
            panic!("halt_loop reached");
        }

        fn halt_once(&self) {
            panic!("halt_once reached");
        }
    }

    #[test]
    #[should_panic(expected = "halt_loop reached")]
    fn cpu_control_trait_object_is_callable() {
        let cpu: &dyn CpuControl = &FakeCpu;
        cpu.halt_loop();
    }

    #[test]
    #[should_panic(expected = "halt_once reached")]
    fn halt_once_is_callable() {
        let cpu: &dyn CpuControl = &FakeCpu;
        cpu.halt_once();
    }
}
