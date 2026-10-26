//! The Rust side of HARLAN's syscall ABI, for programs that run in ring 3.
//!
//! One binding and not one per program. The numbers, the registers and the
//! error codes are a contract — ADR 0014 for how a call is made, ADR 0019
//! for the messages, ADR 0028 for the files, ADR 0029 for the console and
//! the directory — and a second copy of a contract is a copy that drifts.
//! `kernel/src/user.rs` is the other end of the same wire.
//!
//! Every call goes out the way ADR 0014 point 5 says: `rax` carries the
//! number, `rdi`, `rsi`, `rdx`, `r10` the arguments (`r10` and not `rcx`,
//! because `syscall` destroys `rcx`), and `rax` comes back with the result.
//! A negative result is an error.
//!
//! None of these is safe. Each one hands the kernel a pointer or a number
//! from ring 3, and the kernel checks every pointer against the memory this
//! process owns before reading a byte of it (ADR 0014, point 9) — so the
//! worst a mistake here can do is be refused. They are `unsafe` anyway
//! because `asm!` is, and because a caller should have to say it meant it.

#![cfg_attr(not(test), no_std)]

// ---------------------------------------------------------------------
// The numbers (ADR 0014 point 7, ADR 0019, ADR 0028 point 3, ADR 0029)
// ---------------------------------------------------------------------

pub const LOG: u64 = 0;
pub const EXIT: u64 = 1;
pub const YIELD: u64 = 2;
pub const SEND: u64 = 3;
pub const RECV: u64 = 4;
pub const OPEN: u64 = 5;
pub const READ: u64 = 6;
pub const CLOSE: u64 = 7;
pub const WRITE_FILE: u64 = 8;
pub const LIST: u64 = 9;
pub const CONSOLE_WRITE: u64 = 10;
pub const CONSOLE_READ: u64 = 11;

// ---------------------------------------------------------------------
// What a failure is called (ADR 0014 point 6, ADR 0019, ADR 0028 point 13)
// ---------------------------------------------------------------------

pub const ERR_UNKNOWN_CALL: i64 = -1;
pub const ERR_BAD_ARGUMENT: i64 = -2;
pub const ERR_NO_PERMISSION: i64 = -3;
pub const ERR_MAILBOX_FULL: i64 = -4;
pub const ERR_NO_SUCH_PROCESS: i64 = -5;
pub const ERR_WOULD_WAIT_FOR_EVER: i64 = -6;
pub const ERR_BAD_DESCRIPTOR: i64 = -7;
pub const ERR_TOO_MANY_OPEN: i64 = -8;
pub const ERR_NO_SUCH_FILE: i64 = -9;
pub const ERR_FILE_IS_OPEN: i64 = -10;
pub const ERR_VOLUME_FULL: i64 = -11;
pub const ERR_DISK: i64 = -12;

// ---------------------------------------------------------------------
// What a key is (ADR 0029, point 13)
// ---------------------------------------------------------------------

pub const KEY_NONE: i64 = 0;
pub const KEY_ENTER: i64 = 1;
pub const KEY_BACKSPACE: i64 = 2;
pub const KEY_UNKNOWN: i64 = 3;

// ---------------------------------------------------------------------
// The shape of things (ADR 0028, ADR 0029)
// ---------------------------------------------------------------------

/// One directory entry on the wire (ADR 0029, point 5): twelve bytes of
/// name padded with spaces, one of attributes, three reserved, and four of
/// size, little-endian.
pub const ENTRY_BYTES: usize = 20;
/// Where the name starts and how long its field is.
pub const ENTRY_NAME: core::ops::Range<usize> = 0..12;
/// The attributes byte, as FAT stores it.
pub const ENTRY_ATTRIBUTES: usize = 12;
/// The size, little-endian.
pub const ENTRY_SIZE: core::ops::Range<usize> = 16..20;
/// The bit in the attributes that says this is a directory.
pub const ATTR_DIRECTORY: u8 = 0x10;

/// The longest name this filesystem holds: eight, a dot and three.
pub const MAX_NAME: usize = 12;

// ---------------------------------------------------------------------
// Making a call
// ---------------------------------------------------------------------

/// A call with no arguments.
///
/// # Safety
///
/// `number` must be a call this kernel serves; an unknown one answers
/// `ERR_UNKNOWN_CALL` rather than doing anything, so the risk is in what
/// a *known* call does with arguments it was not given.
#[inline]
unsafe fn call0(number: u64) -> i64 {
    let result: i64;
    // SAFETY: `syscall` is how ring 3 asks the kernel for something. It
    // destroys `rcx` and `r11`, which the clobbers say, and the kernel
    // returns the result in `rax`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// A call with one argument.
///
/// # Safety
///
/// As `call0`, and `a` must be what that call expects — a pointer it
/// expects has to be inside this process's memory, which the kernel checks
/// before reading it.
#[inline]
unsafe fn call1(number: u64, a: u64) -> i64 {
    let result: i64;
    // SAFETY: as `call0`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// A call with two arguments.
///
/// # Safety
///
/// As `call1`.
#[inline]
unsafe fn call2(number: u64, a: u64, b: u64) -> i64 {
    let result: i64;
    // SAFETY: as `call0`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// A call with three arguments.
///
/// # Safety
///
/// As `call1`.
#[inline]
unsafe fn call3(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let result: i64;
    // SAFETY: as `call0`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// A call with four arguments.
///
/// The fourth goes in `r10` and not `rcx`, because `syscall` overwrites
/// `rcx` with the address to return to (ADR 0014, point 5).
///
/// # Safety
///
/// As `call1`.
#[inline]
unsafe fn call4(number: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let result: i64;
    // SAFETY: as `call0`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

// ---------------------------------------------------------------------
// The calls
// ---------------------------------------------------------------------

/// Writes text into the kernel's record, which leaves by the debugcon
/// device.
///
/// **Not** what the person sees: that is `console_write` (ADR 0029,
/// point 7). This is for what should survive in the boot log.
///
/// # Safety
///
/// `text` must be this process's own memory, which it is for anything
/// built from a `&str` this program holds.
#[inline]
pub unsafe fn log(text: &str) -> i64 {
    // SAFETY: forwarded; the pointer and length come from a live `&str`.
    unsafe { call2(LOG, text.as_ptr() as u64, text.len() as u64) }
}

/// Stops this process. Never returns.
///
/// # Safety
///
/// Nothing after it runs, which `noreturn` states.
#[inline]
pub unsafe fn exit(code: u64) -> ! {
    // SAFETY: the kernel gives the CPU to somebody else and never switches
    // back, so there is nowhere for an output to land.
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") EXIT,
            in("rdi") code,
            options(nostack, noreturn),
        );
    }
}

/// Gives the CPU to whoever is next, and comes back later.
///
/// How a program waits for anything, since nothing in this ABI blocks: it
/// asks, yields, and asks again.
///
/// # Safety
///
/// As `call0`.
#[inline]
pub unsafe fn yield_now() {
    // SAFETY: forwarded; the call answers nothing worth reading.
    unsafe {
        call0(YIELD);
    }
}

/// Opens a file in the root directory, answering a descriptor.
///
/// # Safety
///
/// As `log`.
#[inline]
pub unsafe fn open(name: &str) -> i64 {
    // SAFETY: forwarded; the pointer and length come from a live `&str`.
    unsafe { call2(OPEN, name.as_ptr() as u64, name.len() as u64) }
}

/// Reads from a descriptor into `into`, answering how many bytes landed
/// there. **Zero means the file ended** (ADR 0028, point 3).
///
/// # Safety
///
/// As `log`. The kernel checks that `into` is memory this process owns and
/// **may write** before filling it, so handing it the wrong thing is
/// refused rather than served.
#[inline]
pub unsafe fn read(descriptor: i64, into: &mut [u8]) -> i64 {
    // SAFETY: forwarded; the pointer and length come from a live slice.
    unsafe {
        call3(
            READ,
            descriptor as u64,
            into.as_mut_ptr() as u64,
            into.len() as u64,
        )
    }
}

/// Closes a descriptor.
///
/// # Safety
///
/// As `call1`.
#[inline]
pub unsafe fn close(descriptor: i64) -> i64 {
    // SAFETY: forwarded.
    unsafe { call1(CLOSE, descriptor as u64) }
}

/// Writes a whole file, creating or replacing it (ADR 0028, point 1).
///
/// # Safety
///
/// As `log`.
#[inline]
pub unsafe fn write_file(name: &str, contents: &[u8]) -> i64 {
    // SAFETY: forwarded; both pointers come from live borrows.
    unsafe {
        call4(
            WRITE_FILE,
            name.as_ptr() as u64,
            name.len() as u64,
            contents.as_ptr() as u64,
            contents.len() as u64,
        )
    }
}

/// Writes one directory entry into `into` — a 20-byte record
/// (ADR 0029, point 5). `ERR_NO_SUCH_FILE` once `index` is past the end,
/// which is what a listing loop stops on.
///
/// # Safety
///
/// As `read`: the kernel fills `into`, so it checks that this process may
/// write it.
#[inline]
pub unsafe fn list(index: u64, into: &mut [u8]) -> i64 {
    // SAFETY: forwarded; the pointer and length come from a live slice.
    unsafe { call3(LIST, index, into.as_mut_ptr() as u64, into.len() as u64) }
}

/// Writes text where the person reads it — the framebuffer, not the
/// kernel's record (ADR 0029, point 7).
///
/// Takes bytes rather than a `&str` so that a program can write one
/// character it has just read without building a string around it. The
/// kernel refuses anything that is not UTF-8, because the console draws
/// text.
///
/// # Safety
///
/// As `log`.
#[inline]
pub unsafe fn console_write(bytes: &[u8]) -> i64 {
    // SAFETY: forwarded; the pointer and length come from a live slice.
    unsafe { call2(CONSOLE_WRITE, bytes.as_ptr() as u64, bytes.len() as u64) }
}

/// Takes one key if one is waiting. `KEY_NONE` means none is, **not** end
/// (ADR 0029, point 10).
///
/// Never waits: waiting inside a syscall would wait for a keyboard
/// interrupt with the clock stopped, which stops the machine rather than
/// this process (ADR 0029, point 11). A program that wants to wait calls
/// `yield_now` and asks again.
///
/// # Safety
///
/// As `call0`.
#[inline]
pub unsafe fn console_read() -> i64 {
    // SAFETY: forwarded.
    unsafe { call0(CONSOLE_READ) }
}

// ---------------------------------------------------------------------
// Reading what a call gave back
// ---------------------------------------------------------------------

/// The name out of a directory record, without its padding.
///
/// `None` if the record is too short to hold one, or if the name is not
/// text — neither can happen from this kernel, and a program that trusted
/// it anyway would be trusting bytes it did not write.
pub fn entry_name(record: &[u8]) -> Option<&str> {
    let field = record.get(ENTRY_NAME)?;
    let end = field
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(field.len());
    core::str::from_utf8(field.get(..end)?).ok()
}

/// The size out of a directory record.
pub fn entry_size(record: &[u8]) -> Option<u32> {
    let field = record.get(ENTRY_SIZE)?;
    Some(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

/// Whether a directory record describes a directory.
pub fn entry_is_directory(record: &[u8]) -> bool {
    record
        .get(ENTRY_ATTRIBUTES)
        .is_some_and(|attributes| attributes & ATTR_DIRECTORY != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as the kernel writes it (`encode_entry` in
    /// `kernel/src/user.rs`), built here from the ADR's literal offsets
    /// rather than from that function: a decoder checked against its own
    /// encoder agrees with itself whatever either of them does.
    fn record(name: &str, size: u32, attributes: u8) -> [u8; ENTRY_BYTES] {
        let mut record = [0u8; ENTRY_BYTES];
        let bytes = name.as_bytes();
        record[0..bytes.len()].copy_from_slice(bytes);
        for byte in &mut record[bytes.len()..12] {
            *byte = b' ';
        }
        record[12] = attributes;
        record[16..20].copy_from_slice(&size.to_le_bytes());
        record
    }

    #[test]
    fn a_name_comes_back_without_its_padding() {
        assert_eq!(
            entry_name(&record("HELLO.TXT", 27, 0x20)),
            Some("HELLO.TXT")
        );
        assert_eq!(entry_name(&record("A", 1, 0x20)), Some("A"));
        // One that fills the field has no padding to strip.
        assert_eq!(
            entry_name(&record("ABCDEFGH.IJK", 1, 0x20)),
            Some("ABCDEFGH.IJK")
        );
    }

    #[test]
    fn a_size_comes_back_whole_and_the_right_way_round() {
        for size in [0u32, 1, 27, 2049, 0x0100_0000, u32::MAX] {
            assert_eq!(entry_size(&record("S.BIN", size, 0x20)), Some(size));
        }
    }

    #[test]
    fn a_directory_is_told_apart_from_a_file() {
        assert!(entry_is_directory(&record("SUBDIR", 0, ATTR_DIRECTORY)));
        assert!(!entry_is_directory(&record("FILE.TXT", 9, 0x20)));
        // Other attribute bits do not make it one.
        assert!(!entry_is_directory(&record("RO.TXT", 9, 0x01)));
        // And the directory bit alongside others still does.
        assert!(entry_is_directory(&record("D", 0, ATTR_DIRECTORY | 0x01)));
    }

    /// Nothing here reads past what it was given. A short buffer is a
    /// buffer the kernel did not fill, and guessing at what is in it would
    /// be a program trusting bytes it did not write.
    #[test]
    fn a_record_too_short_is_refused_rather_than_guessed_at() {
        for len in 0..ENTRY_BYTES {
            let short = &record("HELLO.TXT", 27, 0x20)[..len];
            if len < 12 {
                assert_eq!(entry_name(short), None, "{len} bytes");
            }
            if len < 20 {
                assert_eq!(entry_size(short), None, "{len} bytes");
            }
            if len < 13 {
                assert!(!entry_is_directory(short), "{len} bytes");
            }
        }
    }

    /// The offsets are the ABI (ADR 0029, point 5), so they are asserted
    /// as the literal numbers in its table. A program compiled against one
    /// layout and given another reads a size as a name.
    #[test]
    fn the_record_is_laid_out_where_the_abi_says() {
        assert_eq!(ENTRY_BYTES, 20);
        assert_eq!(ENTRY_NAME, 0..12);
        assert_eq!(ENTRY_ATTRIBUTES, 12);
        assert_eq!(ENTRY_SIZE, 16..20);
        assert_eq!(ATTR_DIRECTORY, 0x10);
    }

    /// The call numbers and the error numbers, which are the rest of the
    /// contract (ADR 0014 point 7, ADR 0019, ADR 0028 point 13, ADR 0029).
    #[test]
    fn the_numbers_are_the_numbers() {
        assert_eq!([LOG, EXIT, YIELD, SEND, RECV], [0, 1, 2, 3, 4]);
        assert_eq!([OPEN, READ, CLOSE, WRITE_FILE], [5, 6, 7, 8]);
        assert_eq!([LIST, CONSOLE_WRITE, CONSOLE_READ], [9, 10, 11]);

        let errors = [
            ERR_UNKNOWN_CALL,
            ERR_BAD_ARGUMENT,
            ERR_NO_PERMISSION,
            ERR_MAILBOX_FULL,
            ERR_NO_SUCH_PROCESS,
            ERR_WOULD_WAIT_FOR_EVER,
            ERR_BAD_DESCRIPTOR,
            ERR_TOO_MANY_OPEN,
            ERR_NO_SUCH_FILE,
            ERR_FILE_IS_OPEN,
            ERR_VOLUME_FULL,
            ERR_DISK,
        ];
        for (at, one) in errors.iter().enumerate() {
            assert!(*one < 0, "{one} would be read as a result");
            for other in &errors[at + 1..] {
                assert_ne!(one, other, "two errors share a number");
            }
        }

        // A key is a small closed set, and no named key can be mistaken
        // for a printable character (ADR 0029, point 13).
        for named in [KEY_NONE, KEY_ENTER, KEY_BACKSPACE, KEY_UNKNOWN] {
            assert!(named < 0x20, "{named} would be read as a character");
        }
    }
}
