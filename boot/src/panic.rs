use core::panic::PanicInfo;

#[cfg(target_arch = "x86_64")]
use harlan_hal::CpuControl;
use harlan_hal::error;

/// HARLAN OS's own panic handler. Logs through the same `log`/debugcon pipeline
/// everything else in this codebase already uses, so a panic is visible to
/// both `cargo xtask run` (debugcon is echoed to stdio there) and
/// `cargo xtask boot-test` (debugcon is captured to a file there). The
/// `uefi` crate's own default panic handler instead writes only to the
/// visible UEFI console via `println!`/`system::with_stdout`, which the
/// debugcon-based test harness can't see at all — that's why the
/// `panic_handler` feature is disabled in Cargo.toml in favor of this one.
///
/// Invariants required of any `#[panic_handler]`:
/// - Must never itself panic (panic-in-panic is UB). This is why it does
///   NOT also try to write to the UEFI console: that call can itself fail
///   if boot services are in a bad state, which would be exactly the wrong
///   failure mode inside a panic handler.
/// - Must not assume any particular kernel state (may run mid-`kmain` with
///   arbitrary borrows live elsewhere) — it only touches process-global
///   facilities (the already-installed `log` logger and `Cpu`), nothing
///   borrowed from the caller.
/// - Never returns.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("HARLAN PANIC: {info}");

    #[cfg(target_arch = "x86_64")]
    {
        harlan_arch_x86_64::Cpu.halt_loop()
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        loop {}
    }
}
