#![cfg_attr(not(test), no_std)]

/// Architecture-independent CPU control surface. Intentionally minimal for
/// Fase 0; Fase 2 extends this once GDT/IDT/interrupt work begins.
pub trait CpuControl {
    /// Parks the core forever. Never returns.
    fn halt_loop(&self) -> !;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeCpu;

    impl CpuControl for FakeCpu {
        fn halt_loop(&self) -> ! {
            panic!("halt_loop reached");
        }
    }

    #[test]
    #[should_panic(expected = "halt_loop reached")]
    fn cpu_control_trait_object_is_callable() {
        let cpu: &dyn CpuControl = &FakeCpu;
        cpu.halt_loop();
    }
}
