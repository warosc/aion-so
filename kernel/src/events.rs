//! The system event log (docs/adr/0031-fase4-system-event-log.md).
//!
//! Not `klog`. That is a stream of text to a debug port: it goes with the
//! machine, nothing in the system can read it, and it is free-form prose
//! written for whoever is debugging right now. This is the other thing
//! `ARCHITECTURE.md` asks for — *"las operaciones destructivas o de alto
//! impacto … deben dejar auditoría"* — which has to survive the machine
//! stopping, be readable by the system, and mean the same thing a year
//! later.
//!
//! A ring of fixed lines in `.bss`, nothing allocated. When it fills, the
//! oldest goes: stopping would hide what just happened, which is usually
//! what matters.
//!
//! Lines, not binary records, and that is a decision. The rule this project
//! verifies by (ADR 0025, point 5) is that what we write somebody else
//! reads: a text file is read by `cat` from the shell, extracted by 7-Zip,
//! and read by a person. A format of our own would be read only by our own
//! parser, which is exactly the position `EMPTY.BIN` taught us to distrust.

use crate::sync::IrqLock;

/// How many events are kept in memory before the oldest is dropped.
///
/// Fixed, so this costs `EVENTS × LINE` bytes of `.bss` and nothing else.
/// Enough for a boot's worth several times over: a boot records about
/// fifteen.
pub const EVENTS: usize = 128;

/// How long one line may be. Space-padded is not used — a line is as long
/// as it is — but it cannot grow past this.
pub const LINE: usize = 72;

/// The most the file on disk may hold, which is the writer's own limit
/// (ADR 0027): asking for more would be refused part way through a write.
pub const MAX_FILE: usize =
    harlan_hal::fat::MAX_FILE_CLUSTERS * harlan_hal::fat::SECTOR_BYTES as usize;

/// What a line says happened. Short, fixed words, so that two boots write
/// the same event the same way and comparing them is not reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Boot,
    Disk,
    NoDisk,
    Loaded,
    Started,
    Exited,
    Faulted,
    Wrote,
    Frames,
    Shell,
    ShellGone,
}

impl What {
    /// The word that goes in the line.
    ///
    /// Fixed width, so the detail column starts in the same place on every
    /// line and a person reading a thousand of them is reading columns
    /// rather than sentences.
    pub const fn word(self) -> &'static str {
        match self {
            What::Boot => "boot      ",
            What::Disk => "disk      ",
            What::NoDisk => "no-disk   ",
            What::Loaded => "loaded    ",
            What::Started => "started   ",
            What::Exited => "exited    ",
            What::Faulted => "faulted   ",
            What::Wrote => "wrote     ",
            What::Frames => "frames    ",
            What::Shell => "shell     ",
            What::ShellGone => "shell-gone",
        }
    }
}

/// One line, and how much of it is used.
#[derive(Clone, Copy)]
struct Line {
    bytes: [u8; LINE],
    len: usize,
}

impl Line {
    const fn empty() -> Self {
        Self {
            bytes: [0; LINE],
            len: 0,
        }
    }
}

/// The ring, and where it is.
struct Ring {
    lines: [Line; EVENTS],
    /// How many have ever been written. The position of the next one is
    /// this modulo `EVENTS`, and the number itself is how many were lost:
    /// anything above `EVENTS` is a line that is gone.
    written: usize,
    /// Which boot this is, as `BOOTS.TXT` counts them. Zero until the boot
    /// has been counted, which is early.
    boot: u32,
    /// How many have been handed to `copy_into` and written down.
    ///
    /// Without this a second flush in one boot writes the boot's lines a
    /// second time, and the log says everything happened twice. It did —
    /// ten boots came to exactly double the size one write per boot gave,
    /// which is how it was noticed.
    flushed: usize,
}

impl Ring {
    const fn new() -> Self {
        Self {
            lines: [Line::empty(); EVENTS],
            written: 0,
            boot: 0,
            flushed: 0,
        }
    }
}

static RING: IrqLock<harlan_arch_x86_64::Cpu, Ring> =
    IrqLock::new(harlan_arch_x86_64::Cpu, Ring::new());

/// Says which boot this is, so that every line after it can say so.
///
/// Without a wall clock there is no date, and "boot 7" is the closest thing
/// to *when* this machine can say (ADR 0031, point 10).
pub fn this_boot(boot: u32) {
    let mut ring = RING.lock();
    ring.boot = boot;

    // Everything in the ring belongs to this boot — it is empty at every
    // start — and the lines written before the disk was mounted say boot
    // zero, because the number comes off the disk. Fill it in.
    //
    // Only when it fits the column: the field is five wide and overwriting
    // in place, so a wider number would run into the ticks. A boot past
    // 99 999 keeps its zeros, which is a column slipping rather than a
    // line meaning something else.
    if boot >= 100_000 {
        return;
    }
    let have = ring.written.min(EVENTS);
    let first = ring.written.saturating_sub(have);
    for step in 0..have {
        let at = (first + step) % EVENTS;
        let line = &mut ring.lines[at];
        put_number(&mut line.bytes, 0, u64::from(boot), 5);
    }
}

/// Records one event: what happened, and a number that says which one.
///
/// A number and not a message, because a message is prose and prose is what
/// `klog` is for. What the number means is decided by `what`: a slot, an
/// exit code, a count of bytes.
pub fn record(what: What, detail: u64) {
    write_line(what, detail, None, "");
}

/// Records one event with a **second** number — a slot and an exit code,
/// say.
///
/// Two columns and not one packed number: `exited 7 0` is a slot and a
/// code, and `exited 12884901888` is what packing them looked like in the
/// first log this wrote. A record nobody can read is not a record.
pub fn record_two(what: What, detail: u64, second: u64) {
    write_line(what, detail, Some(second), "");
}

/// Records one event with a name after the number — a file, a program.
pub fn record_with(what: What, detail: u64, name: &str) {
    write_line(what, detail, None, name);
}

fn write_line(what: What, detail: u64, second: Option<u64>, name: &str) {
    let ticks = {
        use harlan_hal::TickCounter;
        harlan_arch_x86_64::Cpu.ticks()
    };
    let mut ring = RING.lock();
    let boot = ring.boot;
    let at = ring.written % EVENTS;
    let line = &mut ring.lines[at];
    line.len = format_line(&mut line.bytes, boot, ticks, what, detail, second, name);
    ring.written += 1;
}

/// Lays out one line: `<boot> <ticks> <what> <detail> <name>`.
///
/// A free function over a buffer, so the layout can be tested without a
/// machine — which matters more here than usual, because the layout *is*
/// the format, and a format nobody can check is a format that drifts.
fn format_line(
    into: &mut [u8; LINE],
    boot: u32,
    ticks: u64,
    what: What,
    detail: u64,
    second: Option<u64>,
    name: &str,
) -> usize {
    let mut at = 0;
    // The boot, right-aligned in five, so the columns hold until boot
    // 99 999 and then move once rather than every time.
    at += put_number(into, at, u64::from(boot), 5);
    at += put(into, at, " ");
    // The ticks, right-aligned in eight. Not a time, and not offered as
    // one: they order events inside a boot (ADR 0031, point 11).
    at += put_number(into, at, ticks, 8);
    at += put(into, at, " ");
    at += put(into, at, what.word());
    at += put(into, at, " ");
    at += put_number(into, at, detail, 0);
    if let Some(second) = second {
        at += put(into, at, " ");
        at += put_number(into, at, second, 0);
    }
    if !name.is_empty() {
        at += put(into, at, " ");
        at += put(into, at, name);
    }
    at += put(into, at, "\n");
    at
}

/// Copies what fits of `text` at `at`, answering how much went in.
///
/// Truncating rather than wrapping or panicking: a line that would not fit
/// is a line cut short, which is still readable, and `LINE` is generous
/// beside what these events say.
fn put(into: &mut [u8; LINE], at: usize, text: &str) -> usize {
    let room = into.len().saturating_sub(at);
    let taking = text.len().min(room);
    into[at..at + taking].copy_from_slice(&text.as_bytes()[..taking]);
    taking
}

/// Writes `number` as decimal at `at`, padded on the left with spaces to
/// `width`. A number wider than `width` is not cut: a column that slipped
/// is better than a number that lies.
fn put_number(into: &mut [u8; LINE], at: usize, number: u64, width: usize) -> usize {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut left = number;
    loop {
        digits[count] = b'0' + (left % 10) as u8;
        count += 1;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    let mut put_here = at;
    for _ in count..width {
        put_here += put(into, put_here, " ");
    }
    for digit in digits[..count].iter().rev() {
        let room = into.len().saturating_sub(put_here);
        if room == 0 {
            break;
        }
        into[put_here] = *digit;
        put_here += 1;
    }
    put_here - at
}

/// Copies what the ring holds into `into`, oldest first, answering how many
/// bytes that was.
///
/// Stops when `into` is full rather than wrapping, because half a line is a
/// line that lies about the event it ends on.
pub fn copy_into(into: &mut [u8]) -> usize {
    let mut ring = RING.lock();
    let (first, count) = unflushed(ring.written, ring.flushed, EVENTS);
    let mut at = 0;
    let mut taken = 0;
    for step in 0..count {
        let line = &ring.lines[(first + step) % EVENTS];
        if at + line.len > into.len() {
            break;
        }
        into[at..at + line.len].copy_from_slice(&line.bytes[..line.len]);
        at += line.len;
        taken += 1;
    }
    // Only what actually went out is marked as gone: a line left behind
    // because the buffer filled is a line the next flush still owes.
    ring.flushed = first + taken;
    at
}

/// Which lines a flush should take, and from where.
///
/// `(first, count)`: the index of the oldest line not yet written down, and
/// how many there are. A free function over the three numbers so that the
/// wrapping can be tested without the ring, which is a static shared by
/// every test in the binary and therefore the one thing that cannot be set
/// up twice.
fn unflushed(written: usize, flushed: usize, capacity: usize) -> (usize, usize) {
    // Anything older than `written - capacity` has been overwritten, so a
    // flush that fell that far behind starts from the oldest line still
    // there rather than from one that is gone.
    let oldest = written.saturating_sub(capacity);
    let first = flushed.max(oldest);
    (first, written.saturating_sub(first))
}

/// How many events this boot recorded, and how many it had to drop.
pub fn counted() -> (usize, usize) {
    let ring = RING.lock();
    (
        ring.written.min(EVENTS),
        ring.written.saturating_sub(EVENTS),
    )
}

/// Keeps the last `limit` bytes of `old` **starting at a line boundary**,
/// then appends `new`, answering how much of `into` is used.
///
/// Dropping whole lines from the front and never half of one: a log cut
/// mid-line is a log that lies about the first event it keeps
/// (ADR 0031, point 7).
///
/// A free function over slices so the trimming can be tested without a
/// disk, which is the whole of what is interesting here.
pub fn join(old: &[u8], new: &[u8], limit: usize, into: &mut [u8]) -> usize {
    let limit = limit.min(into.len());
    // What is new goes first, because the newest events are the ones
    // somebody is looking for — but only as whole lines. The first version
    // of this took `limit` bytes of `new` outright, and with a limit of one
    // byte wrote a single letter as if it were a log. A test found it; a
    // disk would have kept it.
    let new_kept = newest_whole_lines(new, limit);
    let old_kept = newest_whole_lines(old, limit - new_kept.len());

    into[..old_kept.len()].copy_from_slice(old_kept);
    into[old_kept.len()..old_kept.len() + new_kept.len()].copy_from_slice(new_kept);
    old_kept.len() + new_kept.len()
}

/// The newest whole lines of `text` that fit in `room`.
///
/// Empty when not even the last line fits: nothing is better than a
/// fragment, because a log that starts mid-line lies about the first event
/// it keeps, and the next boot would then build on that lie.
fn newest_whole_lines(text: &[u8], room: usize) -> &[u8] {
    if text.len() <= room {
        return text;
    }
    let cut = text.len() - room;
    // When the cut already lands on the start of a line, what follows is
    // whole and nothing has to be skipped. Advancing anyway threw away a
    // complete line every time the room was an exact fit — caught by a test
    // that asked for exactly the last line and got nothing.
    if cut == 0 || text[cut - 1] == b'\n' {
        return &text[cut..];
    }
    // Otherwise forward to just after the next newline, so what is kept
    // starts where a line starts.
    match text[cut..].iter().position(|byte| *byte == b'\n') {
        Some(at) => &text[cut + at + 1..],
        // No line break in what would be kept: none of it is a whole line.
        None => &text[text.len()..],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_of(
        boot: u32,
        ticks: u64,
        what: What,
        detail: u64,
        name: &str,
    ) -> alloc::string::String {
        let mut bytes = [0u8; LINE];
        let len = format_line(&mut bytes, boot, ticks, what, detail, None, name);
        alloc::string::String::from_utf8(bytes[..len].to_vec()).expect("text")
    }

    /// The columns are the format (ADR 0031, point 4): two boots writing
    /// the same event have to write it identically, or comparing two boots
    /// is reading rather than looking.
    #[test]
    fn a_line_is_columns_and_ends_in_a_newline() {
        let line = line_of(7, 1234, What::Exited, 0, "");
        assert_eq!(line, "    7     1234 exited     0\n");
        // The word is fixed width, so the detail starts in the same place
        // whatever happened.
        let other = line_of(7, 1234, What::Boot, 0, "");
        assert_eq!(
            line.find(" 0\n"),
            other.find(" 0\n"),
            "the detail column moved"
        );
    }

    /// A name goes after the number, and only when there is one.
    #[test]
    fn a_name_is_the_last_field_and_optional() {
        assert_eq!(
            line_of(1, 2, What::Wrote, 20, "EVENTS.LOG"),
            "    1        2 wrote      20 EVENTS.LOG\n"
        );
        assert!(!line_of(1, 2, What::Wrote, 20, "").contains("  \n"));
    }

    /// Two numbers are two columns.
    ///
    /// The first log this wrote said `exited 12884901888`, because the slot
    /// and the exit code had been packed into one `u64`. It compiled, it
    /// round-tripped, and it meant nothing to anybody reading it — which is
    /// the entire reason this log is text.
    #[test]
    fn two_numbers_are_two_columns_not_one() {
        let mut bytes = [0u8; LINE];
        let len = format_line(&mut bytes, 2, 258, What::Exited, 7, Some(0), "");
        let line = core::str::from_utf8(&bytes[..len]).expect("text");
        assert_eq!(line, "    2      258 exited     7 0\n");
        // And the packed version is not what comes out.
        assert!(!line.contains("12884901888"));
    }

    /// Numbers wider than their column push the line out rather than being
    /// cut. A column that slipped is better than a number that lies.
    #[test]
    fn a_number_too_wide_for_its_column_is_not_cut() {
        let line = line_of(999_999, u64::MAX, What::Frames, 0, "");
        assert!(line.contains("999999"), "{line:?}");
        assert!(line.contains(&u64::MAX.to_string()), "{line:?}");
    }

    /// Every line ends in a newline, whatever it holds — including one long
    /// enough to be truncated, because a log of lines with a line that does
    /// not end is a log whose next line is unreadable.
    #[test]
    fn every_line_ends_where_a_line_ends() {
        for name in ["", "A", "ABCDEFGH.IJK"] {
            for detail in [0u64, 1, u64::MAX] {
                let line = line_of(1, 1, What::Started, detail, name);
                assert!(line.ends_with('\n'), "{line:?}");
                assert!(line.len() <= LINE, "{} bytes", line.len());
            }
        }
    }

    /// The boot number is filled in on lines written before it was known.
    ///
    /// It comes off the disk, so the disk has to be mounted first, so the
    /// earliest lines of every boot were written while it was still zero.
    /// The ring is empty at every start, so everything in it belongs to
    /// this boot and the number can be finished rather than guessed.
    #[test]
    fn the_boot_number_reaches_the_lines_written_before_it_was_known() {
        // The real ring is a static shared by every test in this binary,
        // so this works on lines directly: what is under test is that the
        // field is rewritten in place, within its column.
        let mut bytes = [0u8; LINE];
        let len = format_line(&mut bytes, 0, 253, What::Disk, 129_022, None, "");
        let before = core::str::from_utf8(&bytes[..len])
            .expect("text")
            .to_string();
        assert!(before.starts_with("    0 "), "{before:?}");

        put_number(&mut bytes, 0, 7, 5);
        let after = core::str::from_utf8(&bytes[..len]).expect("text");
        assert_eq!(
            after,
            "    7      253 disk       129022
"
        );
        assert_eq!(
            after.len(),
            before.len(),
            "the rewrite moved the other columns"
        );
    }

    // -----------------------------------------------------------------
    // What a flush takes
    // -----------------------------------------------------------------

    /// A second flush in one boot takes only what is new.
    ///
    /// Without this the whole ring went out again, the file got this
    /// boot's lines twice, and the log said everything happened twice.
    /// Nothing failed: ten boots simply came to double the bytes one write
    /// per boot had given, which is the only reason it was noticed.
    #[test]
    fn a_flush_takes_only_what_the_last_one_left() {
        // Nothing written, nothing to take.
        assert_eq!(unflushed(0, 0, 128), (0, 0));
        // Five written, none flushed: all five, from the start.
        assert_eq!(unflushed(5, 0, 128), (0, 5));
        // Flushed those five, then two more: the two, from five.
        assert_eq!(unflushed(7, 5, 128), (5, 2));
        // And a flush with nothing new takes nothing.
        assert_eq!(unflushed(7, 7, 128), (7, 0));
    }

    /// A flush that fell behind further than the ring is deep starts at
    /// the oldest line that is still there, not at one that is gone.
    #[test]
    fn a_flush_that_fell_behind_starts_where_the_ring_still_has_lines() {
        // 200 written into a ring of 128, 10 flushed: lines 0..72 are
        // overwritten, so it starts at 72 and takes the 128 that remain.
        assert_eq!(unflushed(200, 10, 128), (72, 128));
        // Flushed past the overwrite point: carry on from there.
        assert_eq!(unflushed(200, 150, 128), (150, 50));
        // Never more than the ring holds.
        let (_, count) = unflushed(10_000, 0, 128);
        assert_eq!(count, 128);
    }

    // -----------------------------------------------------------------
    // Joining this boot's lines onto what the disk already held
    // -----------------------------------------------------------------

    fn joined(old: &str, new: &str, limit: usize) -> alloc::string::String {
        let mut into = alloc::vec![0u8; limit + new.len() + old.len()];
        let len = join(old.as_bytes(), new.as_bytes(), limit, &mut into);
        alloc::string::String::from_utf8(into[..len].to_vec()).expect("text")
    }

    #[test]
    fn what_fits_is_kept_whole() {
        assert_eq!(joined("a\nb\n", "c\n", 100), "a\nb\nc\n");
        assert_eq!(joined("", "c\n", 100), "c\n");
        assert_eq!(joined("a\n", "", 100), "a\n");
    }

    /// When it does not fit, whole lines go from the front — never half of
    /// one. A log cut mid-line lies about the first event it keeps.
    #[test]
    fn the_oldest_whole_lines_go_first() {
        // Room for six bytes of old: "bbb\n" and "ccc\n" is eight, so only
        // "ccc\n" survives, and "bb" is not kept as a fragment.
        let out = joined("aaa\nbbb\nccc\n", "new\n", 10);
        assert_eq!(out, "ccc\nnew\n");
        assert!(!out.contains("bb"), "a fragment was kept: {out:?}");
        for line in out.lines() {
            assert!(
                ["ccc", "new"].contains(&line),
                "{line:?} is not a whole line"
            );
        }
    }

    /// The new lines go in before anything old is kept: the newest events
    /// are the ones somebody is looking for.
    #[test]
    fn the_new_lines_never_lose_to_the_old() {
        assert_eq!(joined("aaa\nbbb\n", "new\n", 4), "new\n");
        assert_eq!(joined("aaa\nbbb\n", "new\n", 5), "new\n");
    }

    /// And when not even a new line fits, nothing is written rather than a
    /// fragment of one.
    ///
    /// The first version of `join` took `limit` bytes of the new text
    /// outright, so a limit of one byte produced `"d"` — a log one letter
    /// long, which the next boot would then have appended to. Found by the
    /// loop above before any of it reached a disk.
    #[test]
    fn a_new_line_that_does_not_fit_is_not_written_in_pieces() {
        for limit in 0..3 {
            assert_eq!(joined("old\n", "dd\nee\n", limit), "", "limit {limit}");
        }
        // Three bytes is exactly "ee\n", the newest whole line.
        assert_eq!(joined("old\n", "dd\nee\n", 3), "ee\n");
        // Six is both of them, and nothing of the old.
        assert_eq!(joined("old\n", "dd\nee\n", 6), "dd\nee\n");
        // Ten is both plus the old one.
        assert_eq!(joined("old\n", "dd\nee\n", 10), "old\ndd\nee\n");
    }

    /// Old text with no newline in the part that would be kept is not kept
    /// at all: there is no whole line in it.
    #[test]
    fn a_tail_with_no_line_break_is_dropped_entirely() {
        assert_eq!(joined("no newline here", "new\n", 8), "new\n");
    }

    /// Whatever goes in, the result is only whole lines, so the next boot
    /// can do the same thing to it.
    #[test]
    fn the_result_is_always_whole_lines() {
        for limit in 1..40 {
            let out = joined("aaa\nbbbb\nccccc\n", "dd\nee\n", limit);
            if out.is_empty() {
                continue;
            }
            assert!(out.ends_with('\n'), "limit {limit}: {out:?}");
            for line in out.lines() {
                assert!(
                    ["aaa", "bbbb", "ccccc", "dd", "ee"].contains(&line),
                    "limit {limit}: {line:?} is not a whole line"
                );
            }
        }
    }
}
