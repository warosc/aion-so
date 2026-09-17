use aion_hal::{Console, ConsoleKey};
use core::fmt::Write;
use uefi::proto::console::text::Key;

/// Adapts UEFI's Simple Text Input/Output protocols (accessed through
/// `uefi::system::with_stdin`/`with_stdout`, both Boot-Services-era
/// facilities) to the arch/firmware-agnostic `Console` trait `kernel`
/// actually depends on.
pub struct UefiConsole;

impl Console for UefiConsole {
    fn read_key(&mut self) -> Option<ConsoleKey> {
        match uefi::system::with_stdin(|stdin| stdin.read_key()) {
            Ok(Some(Key::Printable(ch16))) => match char::from(ch16) {
                '\r' => Some(ConsoleKey::Enter),
                '\u{8}' => Some(ConsoleKey::Backspace),
                c => Some(ConsoleKey::Char(c)),
            },
            Ok(Some(Key::Special(_))) => Some(ConsoleKey::Unknown),
            // `NOT_READY` (no key yet) is the expected, silent case on every
            // poll where nobody has typed anything — not logged, or the
            // debugcon log would be pure noise.
            Ok(None) => None,
            // A real device error, unlike `NOT_READY`, is worth logging: an
            // unlogged `None` here would be indistinguishable from "nobody
            // has typed anything yet" and make a broken input device
            // undiagnosable from the boot-test log.
            Err(e) => {
                log::warn!("AION: UEFI stdin read_key error: {e:?}");
                None
            }
        }
    }

    fn write_str(&mut self, s: &str) {
        let _ = uefi::system::with_stdout(|out| out.write_str(s));
    }

    fn clear(&mut self) {
        let _ = uefi::system::with_stdout(|out| out.clear());
    }
}
