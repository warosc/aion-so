//! The console, where a syscall can reach it
//! (docs/adr/0029-fase4-console-and-list-abi.md).
//!
//! The second device this kernel shares between contexts, after the disk,
//! and for the same reason: a shell in ring 3 has to be able to print its
//! prompt and read a key, and both of those live behind a `Console` that
//! until now only `kmain` and the kernel's own shell ever touched.
//!
//! `log` (syscall 0) is **not** this. That goes to the kernel's record,
//! which leaves by the debugcon device; this goes to the framebuffer. They
//! are two channels on purpose (ADR 0001 said so about the shell marker),
//! and a program picks: what the person reads goes here, what should be in
//! the boot record goes to `log`.

use harlan_hal::{Console, ConsoleKey};

use crate::sync::IrqLock;

/// The console the kernel already owns, as a pointer.
///
/// A pointer rather than a `Box<dyn Console>` because of the constraint
/// `KernelContext` documents: **a trait object's vtable pointer is written
/// when the object is formed, and names the image as it was mapped then.**
/// The kernel moves to the higher half and the lower half becomes user
/// space (ADR 0012, ADR 0013), so an object formed before that move carries
/// a vtable address that stops existing. The context avoids `dyn` for
/// exactly this reason, and `run_shell` gets away with `&mut dyn Console`
/// because it is handed one formed afterwards.
///
/// So this is filled late — once, from `run`, after the move and just
/// before the first process can make a syscall — and the pointer it is
/// given is a `*mut dyn Console` formed at that point.
///
/// `None` before then, and `None` again once the kernel's own shell takes
/// it back (see `take`). A kernel without one still boots:
/// `HardwareConsole` already drops its output when the firmware gave it no
/// usable framebuffer, so "nothing to draw on" is a case the rest of the
/// kernel has always had to survive.
static CONSOLE: IrqLock<harlan_arch_x86_64::Cpu, Option<Held>> =
    IrqLock::new(harlan_arch_x86_64::Cpu, None);

/// The console, held as a pointer so that the vtable is formed late.
struct Held(*mut dyn Console);

// SAFETY: the pointer is to the console the kernel owns, which `kmain` took
// by value and leaked onto the heap, so it stays valid for as long as the
// kernel runs. It is reachable only through `CONSOLE`, whose guard is the
// only way to a `&mut` of it, so it is never followed from two places at
// once — which is what `&mut dyn Console` requires.
unsafe impl Send for Held {}

/// Hands the console to this module, where a syscall can reach it.
///
/// # Safety
///
/// `console` must point at a console that stays valid for as long as the
/// kernel runs, must be formed **after** the kernel has moved to the higher
/// half (so its vtable address is one that is still mapped), and nothing
/// else may use that console through any other reference from here on.
///
/// The last clause is why the kernel's own shell goes through `take`
/// instead of keeping the reference it could get from the context: two
/// paths to one `&mut` is the aliasing the compiler would be right to
/// forbid, and the two-copies-of-the-truth this kernel keeps running into.
pub unsafe fn adopt(console: *mut dyn Console) {
    *CONSOLE.lock() = Some(Held(console));
}

/// Takes the console back, for a caller that needs it for longer than one
/// operation.
///
/// The kernel's own shell is that caller: it loops for ever, and holding
/// the lock across that loop would leave interrupts disabled for ever — so
/// the keyboard IRQ would never fire and the shell would never get a key.
/// Taking it keeps one owner at a time, and from then on `write` and
/// `read_key` answer `None`.
///
/// That is correct rather than merely tolerable: the kernel's shell runs
/// only when nothing is runnable, and a syscall can only come from a
/// process that is running.
pub fn take() -> Option<*mut dyn Console> {
    CONSOLE.lock().take().map(|held| held.0)
}

/// Whether there is a console here to write to.
pub fn present() -> bool {
    CONSOLE.lock().is_some()
}

/// Runs `operation` over the console, holding its lock.
fn with<R>(operation: impl FnOnce(&mut dyn Console) -> R) -> Option<R> {
    let mut console = CONSOLE.lock();
    let held = console.as_mut()?;
    // SAFETY: the pointer satisfies `adopt`'s contract, and this guard is
    // the only way to reach it, so no other reference to the console exists
    // while this one does.
    let console: &mut dyn Console = unsafe { &mut *held.0 };
    Some(operation(console))
}

/// Writes text where the person can read it.
///
/// Answers how many bytes went out, or `None` if there is no console —
/// which a program is told as a count of zero rather than an error: text
/// that went nowhere because nothing can display it is not the program's
/// mistake, and a kernel with no framebuffer already drops its own output
/// the same way.
pub fn write(text: &str) -> Option<usize> {
    with(|console| {
        console.write_str(text);
        text.len()
    })
}

/// Takes one key if one is waiting, without waiting.
///
/// Never blocks, and that is a requirement rather than a convenience: the
/// syscall handler runs with interrupts off (ADR 0014, point 2) and a key
/// arrives on the keyboard's IRQ, so a call that waited would wait for an
/// interrupt that cannot arrive, with the clock stopped — the whole machine
/// rather than one process (ADR 0029, point 11).
pub fn read_key() -> Option<ConsoleKey> {
    with(|console| console.read_key()).flatten()
}

/// What a key is, as a number a program can be given
/// (ADR 0029, point 13).
///
/// The low numbers for what is not a character, and a character as its own
/// code: a program compares against `b' '` and `b'~'` and knows. Anything
/// that is not printable ASCII becomes `UNKNOWN` rather than its code
/// point, because the PS/2 keyboard here does not produce one and the 8x8
/// font could not draw it.
pub const KEY_NONE: u64 = 0;
pub const KEY_ENTER: u64 = 1;
pub const KEY_BACKSPACE: u64 = 2;
pub const KEY_UNKNOWN: u64 = 3;

/// Turns a key into the number the ABI gives a program.
///
/// A free function so that the encoding can be tested without a console —
/// the same reason `owned_by` is one.
pub fn encode(key: Option<ConsoleKey>) -> u64 {
    match key {
        None => KEY_NONE,
        Some(ConsoleKey::Enter) => KEY_ENTER,
        Some(ConsoleKey::Backspace) => KEY_BACKSPACE,
        Some(ConsoleKey::Char(c)) if c.is_ascii_graphic() || c == ' ' => c as u64,
        // Everything else, including a character this console could not
        // draw: one answer, which is what the table says.
        Some(ConsoleKey::Char(_) | ConsoleKey::Unknown) => KEY_UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoding is the ABI (ADR 0029, point 13), so it is asserted
    /// rather than derived: a program compiled against one of these numbers
    /// and given another reads a Backspace as a space.
    #[test]
    fn a_key_is_the_number_the_abi_says() {
        assert_eq!(encode(None), 0);
        assert_eq!(encode(Some(ConsoleKey::Enter)), 1);
        assert_eq!(encode(Some(ConsoleKey::Backspace)), 2);
        assert_eq!(encode(Some(ConsoleKey::Unknown)), 3);
        assert_eq!(encode(Some(ConsoleKey::Char('A'))), 0x41);
        assert_eq!(encode(Some(ConsoleKey::Char(' '))), 0x20);
        assert_eq!(encode(Some(ConsoleKey::Char('~'))), 0x7E);
    }

    /// Every printable ASCII character is its own code, and nothing else
    /// lands in that range. A character that fell into the low numbers
    /// would be read as Enter or Backspace; one of those appearing as a
    /// character would be typed into a line.
    #[test]
    fn printable_ascii_and_the_low_numbers_do_not_overlap() {
        for byte in 0x20u8..=0x7E {
            let c = byte as char;
            assert_eq!(
                encode(Some(ConsoleKey::Char(c))),
                u64::from(byte),
                "{c:?} is its own code"
            );
        }
        // And nothing outside that range lands inside it.
        for c in ['\n', '\t', '\r', '\0', '\u{7F}', 'á', '€', '\u{1F600}'] {
            let encoded = encode(Some(ConsoleKey::Char(c)));
            assert_eq!(
                encoded, KEY_UNKNOWN,
                "{c:?} is not a key this console names"
            );
        }
        // The three named keys are below every character.
        for named in [KEY_NONE, KEY_ENTER, KEY_BACKSPACE, KEY_UNKNOWN] {
            assert!(named < 0x20, "{named} would be read as a character");
        }
    }
}
