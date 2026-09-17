#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleKey {
    Char(char),
    Enter,
    Backspace,
    /// Arrows, function keys, etc. Not needed by the Fase 1 shell; folded
    /// into one variant rather than modeled precisely.
    Unknown,
}

/// Text console abstraction. Key-level (not line-buffered) because the
/// shell needs to handle backspace and echo character by character; all
/// line-assembly logic lives in `kernel`, not here, so it stays
/// host-testable behind a fake implementation.
pub trait Console {
    /// Non-blocking poll for one pending keystroke. Returns `None`
    /// immediately if none is buffered — never blocks.
    fn read_key(&mut self) -> Option<ConsoleKey>;

    /// Writes UTF-8 text verbatim; implementations own newline translation.
    /// No implicit echo of input — callers echo typed characters themselves.
    fn write_str(&mut self, s: &str);

    fn clear(&mut self);
}
