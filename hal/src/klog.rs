//! Where the kernel's log lines go.
//!
//! Not the `log` crate: its logger is a `&'static dyn Log`, registered once
//! and never again, and both halves of that fat pointer name the image the
//! kernel is running from. The kernel moves its image
//! (docs/adr/0012-fase3-higher-half-kernel.md) and then stops mapping
//! where it used to be, which turns that pointer into a fault the first
//! time anything logs.
//!
//! So the sink here is a plain function pointer this kernel owns and can
//! set as many times as it needs — once at boot, and again from the new
//! address after the move. Until one is set, log lines are dropped rather
//! than buffered: there is nowhere to put them yet.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
}

impl Level {
    /// Right-aligned to five, like the lines the bootloader used to print,
    /// so old and new boot logs line up.
    pub const fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => " WARN",
            Level::Info => " INFO",
        }
    }
}

/// What a sink is handed: the level, where the line was written, and the
/// message itself.
pub type Sink = fn(Level, &str, u32, fmt::Arguments<'_>);

/// Null until someone sets one. A `usize` rather than an `AtomicPtr` so
/// that the only unsafe step is the one conversion back.
static SINK: AtomicUsize = AtomicUsize::new(0);

/// Sends log lines to `sink` from now on.
///
/// Safe to call again whenever the address of the code changes: that is
/// the whole point.
///
/// **With more than one core this is not enough.** `Release`/`Acquire`
/// keeps the pointer itself intact, but one core can load the old sink,
/// another publish a new one and unmap the code the first is about to
/// jump to. Stronger orderings do not fix that — it is a lifetime
/// problem, not an ordering one. Whoever moves the kernel's image will
/// have to stop the other cores first, or wait for a grace period in
/// which none of them is inside a sink. Raised by Codex reviewing
/// Incremento 17.
pub fn set_sink(sink: Sink) {
    SINK.store(sink as usize, Ordering::Release);
}

/// Drops log lines from now on.
pub fn clear_sink() {
    SINK.store(0, Ordering::Release);
}

#[doc(hidden)]
pub fn write(level: Level, file: &str, line: u32, args: fmt::Arguments<'_>) {
    let sink = SINK.load(Ordering::Acquire);
    if sink == 0 {
        return;
    }
    // SAFETY: `SINK` only ever holds what `set_sink` put there, which is a
    // `Sink` and nothing else, and zero means nobody has.
    let sink: Sink = unsafe { core::mem::transmute::<usize, Sink>(sink) };
    sink(level, file, line, args);
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::klog::write(
            $crate::klog::Level::Info,
            file!(),
            line!(),
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::klog::write(
            $crate::klog::Level::Warn,
            file!(),
            line!(),
            format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::klog::write(
            $crate::klog::Level::Error,
            file!(),
            line!(),
            format_args!($($arg)*),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SEEN: Mutex<std::vec::Vec<std::string::String>> = Mutex::new(std::vec::Vec::new());

    fn collect(level: Level, file: &str, line: u32, args: fmt::Arguments<'_>) {
        SEEN.lock()
            .unwrap()
            .push(std::format!("{}|{file}@{line}|{args}", level.name()));
    }

    fn other(_level: Level, _file: &str, _line: u32, args: fmt::Arguments<'_>) {
        SEEN.lock().unwrap().push(std::format!("other|{args}"));
    }

    /// One test, because the sink is global: the point is that it can be
    /// set, changed and cleared, which is what the move needs.
    #[test]
    fn lines_go_to_whichever_sink_is_set_and_nowhere_when_there_is_none() {
        clear_sink();
        crate::info!("dropped {}", 1);
        assert!(SEEN.lock().unwrap().is_empty());

        set_sink(collect);
        crate::info!("hello {}", "world");
        crate::warn!("careful");
        crate::error!("broken");

        // Changing it again is allowed, and that is the whole point: the
        // kernel does it once its code lives somewhere else.
        set_sink(other);
        crate::info!("after the move");

        clear_sink();
        crate::error!("dropped too");

        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(seen[0].starts_with(" INFO|"), "{seen:?}");
        assert!(seen[0].ends_with("|hello world"), "{seen:?}");
        assert!(seen[1].starts_with(" WARN|"), "{seen:?}");
        assert!(seen[2].starts_with("ERROR|"), "{seen:?}");
        assert_eq!(seen[3], "other|after the move");
    }
}
