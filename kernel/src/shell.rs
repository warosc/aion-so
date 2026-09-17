use aion_hal::{Console, ConsoleKey, PowerControl};

/// Logged once the shell starts polling for input. Grepped by
/// `cargo xtask boot-test`. Distinct from the visible `AION> ` prompt: the
/// prompt goes through the UEFI console device, this marker goes through
/// the debugcon device — they are not the same channel, so the marker
/// can't just be the literal prompt text.
pub const SHELL_READY_MARKER: &str = "AION-PHASE1-SHELL-READY";

const PROMPT: &str = "AION> ";
/// Fixed stack buffer: no heap exists yet (that's Fase 2), and a line this
/// long is more than enough for the five known commands.
const LINE_MAX: usize = 128;

pub fn run_shell(console: &mut dyn Console, power: &dyn PowerControl) -> ! {
    log::info!("{SHELL_READY_MARKER}");
    let mut buf = [0u8; LINE_MAX];
    loop {
        console.write_str(PROMPT);
        let line = read_line(console, &mut buf);
        dispatch(console, power, line);
    }
}

fn read_line<'a>(console: &mut dyn Console, buf: &'a mut [u8; LINE_MAX]) -> &'a str {
    let mut len = 0usize;
    loop {
        match console.read_key() {
            Some(ConsoleKey::Enter) => {
                console.write_str("\n");
                break;
            }
            Some(ConsoleKey::Backspace) => {
                if len > 0 {
                    len -= 1;
                    console.write_str("\u{8} \u{8}");
                }
            }
            Some(ConsoleKey::Char(c)) => {
                // Fase 1 simplification: printable ASCII (plus space) only,
                // silently dropped once the fixed buffer is full. Excludes
                // C0 control characters (e.g. Tab, 0x09) — those are ASCII
                // too, but aren't meant to become part of a command line or
                // be echoed as a glyph.
                if (c.is_ascii_graphic() || c == ' ') && len < buf.len() {
                    buf[len] = c as u8;
                    len += 1;
                    console.write_str(core::str::from_utf8(&buf[len - 1..len]).unwrap_or(""));
                }
            }
            Some(ConsoleKey::Unknown) => {}
            None => idle_once(),
        }
    }
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

fn dispatch(console: &mut dyn Console, power: &dyn PowerControl, cmd: &str) {
    match cmd {
        "help" => console.write_str("help clear version reboot shutdown\n"),
        "clear" => console.clear(),
        "version" => console.write_str(concat!("AION OS v", env!("CARGO_PKG_VERSION"), "\n")),
        "reboot" => power.reboot(),
        "shutdown" => power.shutdown(),
        "" => {}
        other => {
            console.write_str("unknown command: ");
            console.write_str(other);
            console.write_str("\n");
        }
    }
}

fn idle_once() {
    #[cfg(target_arch = "x86_64")]
    {
        use aion_hal::CpuControl;
        aion_arch_x86_64::Cpu.halt_once();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeConsole {
        keys: VecDeque<ConsoleKey>,
        output: String,
    }

    impl FakeConsole {
        fn from_str(input: &str) -> Self {
            let mut keys = VecDeque::new();
            for c in input.chars() {
                keys.push_back(if c == '\n' {
                    ConsoleKey::Enter
                } else {
                    ConsoleKey::Char(c)
                });
            }
            Self {
                keys,
                output: String::new(),
            }
        }
    }

    impl Console for FakeConsole {
        fn read_key(&mut self) -> Option<ConsoleKey> {
            // A test that lets this queue run dry is a bug in the test, not
            // a real "no key yet" case: real `idle_once()` executes a
            // privileged `hlt` instruction that would fault in a host test
            // process. Fail loudly instead of silently spinning or faulting.
            Some(
                self.keys
                    .pop_front()
                    .expect("test queue exhausted: every test input must end in '\\n'"),
            )
        }

        fn write_str(&mut self, s: &str) {
            self.output.push_str(s);
        }

        fn clear(&mut self) {
            self.output.clear();
        }
    }

    struct FakePower;

    impl PowerControl for FakePower {
        fn reboot(&self) -> ! {
            panic!("FakePower::reboot called");
        }

        fn shutdown(&self) -> ! {
            panic!("FakePower::shutdown called");
        }
    }

    #[test]
    fn dispatch_help_lists_all_five_commands() {
        let mut console = FakeConsole::from_str("");
        dispatch(&mut console, &FakePower, "help");
        assert_eq!(console.output, "help clear version reboot shutdown\n");
    }

    #[test]
    fn dispatch_version_reports_crate_version() {
        let mut console = FakeConsole::from_str("");
        dispatch(&mut console, &FakePower, "version");
        assert_eq!(console.output, "AION OS v0.0.1\n");
    }

    #[test]
    fn dispatch_clear_clears_output() {
        let mut console = FakeConsole::from_str("");
        console.write_str("stale content");
        dispatch(&mut console, &FakePower, "clear");
        assert_eq!(console.output, "");
    }

    #[test]
    fn dispatch_empty_command_is_a_no_op() {
        let mut console = FakeConsole::from_str("");
        dispatch(&mut console, &FakePower, "");
        assert_eq!(console.output, "");
    }

    #[test]
    fn dispatch_unknown_command_reports_an_error_not_a_panic() {
        let mut console = FakeConsole::from_str("");
        dispatch(&mut console, &FakePower, "frobnicate");
        assert_eq!(console.output, "unknown command: frobnicate\n");
    }

    #[test]
    #[should_panic(expected = "FakePower::reboot called")]
    fn dispatch_reboot_calls_power_control() {
        let mut console = FakeConsole::from_str("");
        dispatch(&mut console, &FakePower, "reboot");
    }

    #[test]
    #[should_panic(expected = "FakePower::shutdown called")]
    fn dispatch_shutdown_calls_power_control() {
        let mut console = FakeConsole::from_str("");
        dispatch(&mut console, &FakePower, "shutdown");
    }

    #[test]
    fn read_line_assembles_chars_until_enter() {
        let mut console = FakeConsole::from_str("help\n");
        let mut buf = [0u8; LINE_MAX];
        let line = read_line(&mut console, &mut buf);
        assert_eq!(line, "help");
    }

    #[test]
    fn read_line_handles_backspace() {
        let mut console = FakeConsole::from_str("");
        console.keys.push_back(ConsoleKey::Char('h'));
        console.keys.push_back(ConsoleKey::Char('x'));
        console.keys.push_back(ConsoleKey::Backspace);
        console.keys.push_back(ConsoleKey::Char('e'));
        console.keys.push_back(ConsoleKey::Char('l'));
        console.keys.push_back(ConsoleKey::Char('p'));
        console.keys.push_back(ConsoleKey::Enter);
        let mut buf = [0u8; LINE_MAX];
        let line = read_line(&mut console, &mut buf);
        assert_eq!(line, "help");
    }

    #[test]
    fn read_line_backspace_on_empty_line_is_ignored() {
        let mut console = FakeConsole::from_str("");
        console.keys.push_back(ConsoleKey::Backspace);
        console.keys.push_back(ConsoleKey::Char('a'));
        console.keys.push_back(ConsoleKey::Enter);
        let mut buf = [0u8; LINE_MAX];
        let line = read_line(&mut console, &mut buf);
        assert_eq!(line, "a");
    }

    #[test]
    fn read_line_drops_control_characters_but_keeps_space() {
        let mut console = FakeConsole::from_str("");
        console.keys.push_back(ConsoleKey::Char('h'));
        console.keys.push_back(ConsoleKey::Char('i'));
        console.keys.push_back(ConsoleKey::Char('\t')); // dropped: control char
        console.keys.push_back(ConsoleKey::Char(' ')); // kept: printable space
        console.keys.push_back(ConsoleKey::Char('!'));
        console.keys.push_back(ConsoleKey::Enter);
        let mut buf = [0u8; LINE_MAX];
        let line = read_line(&mut console, &mut buf);
        assert_eq!(line, "hi !");
    }
}
