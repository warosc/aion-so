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
            // Fase 1 simplification: `NOT_READY` (no key yet) and any device
            // error are both treated as "no key this poll" — the shell just
            // tries again next iteration.
            Ok(None) | Err(_) => None,
        }
    }

    fn write_str(&mut self, s: &str) {
        let _ = uefi::system::with_stdout(|out| out.write_str(s));
    }

    fn clear(&mut self) {
        let _ = uefi::system::with_stdout(|out| out.clear());
    }
}
