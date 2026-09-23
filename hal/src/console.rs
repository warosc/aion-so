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

    /// The memory this console draws into can be reached at `base` now.
    ///
    /// The kernel moves the window it sees physical memory through
    /// (docs/adr/0013-fase3-physical-window.md), and a console that holds
    /// a pointer into the framebuffer has to be told. One that draws
    /// nowhere ignores it.
    ///
    /// # Safety
    ///
    /// `base` must be the first byte of the same framebuffer, mapped for
    /// volatile reads and writes of at least the size it was created with,
    /// and it must stay so for as long as the console lives.
    unsafe fn framebuffer_moved(&mut self, base: crate::addr::VirtAddr) {
        let _ = base;
    }
}
