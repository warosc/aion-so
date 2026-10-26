//! The parts of the shell that are only logic: taking a line apart and
//! writing a number.
//!
//! Separated from `main.rs` so that they can be tested on the host. Nothing
//! here makes a syscall, touches a pointer or needs a machine — which is
//! exactly why it is worth testing, because it is where an off-by-one
//! hides quietly instead of faulting.

#![cfg_attr(not(test), no_std)]

/// The longest name this filesystem holds: eight, a dot and three.
pub const MAX_NAME: usize = 12;

/// The line without the spaces at either end.
pub fn trimmed(line: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < line.len() && line[start] == b' ' {
        start += 1;
    }
    let mut end = line.len();
    while end > start && line[end - 1] == b' ' {
        end -= 1;
    }
    &line[start..end]
}

/// The first word, and everything after the spaces that follow it.
///
/// Both halves trimmed, so `write  a.txt   hello ` gives `write` and
/// `a.txt   hello` — and the second half keeps its inner spaces, because
/// what somebody typed into a file is what goes into the file.
pub fn split_once(line: &[u8]) -> (&[u8], &[u8]) {
    let line = trimmed(line);
    match line.iter().position(|byte| *byte == b' ') {
        Some(at) => (&line[..at], trimmed(&line[at..])),
        None => (line, &[]),
    }
}

/// A word as a name, if it could be one.
///
/// `None` for an empty word or one too long to be an 8.3 name — refused
/// here so the message says what was wrong, rather than handing it to the
/// kernel to come back as a bad argument.
pub fn as_name(word: &[u8]) -> Option<&str> {
    if word.is_empty() || word.len() > MAX_NAME {
        return None;
    }
    core::str::from_utf8(word).ok()
}

/// `number` as decimal, answering how many bytes it took.
///
/// There is no formatter in a `no_std` program that writes into a buffer
/// without allocating, and this is a dozen lines.
pub fn decimal(number: u32, into: &mut [u8; 10]) -> usize {
    let mut digits = [0u8; 10];
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
    for (at, digit) in digits[..count].iter().rev().enumerate() {
        into[at] = *digit;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_loses_the_spaces_at_its_ends_and_keeps_the_ones_inside() {
        assert_eq!(trimmed(b"  ls  "), b"ls");
        assert_eq!(trimmed(b"ls"), b"ls");
        assert_eq!(trimmed(b"echo  a  b"), b"echo  a  b");
        assert_eq!(trimmed(b"   "), b"");
        assert_eq!(trimmed(b""), b"");
    }

    #[test]
    fn a_line_splits_into_a_command_and_the_rest() {
        assert_eq!(split_once(b"ls"), (b"ls".as_slice(), b"".as_slice()));
        assert_eq!(
            split_once(b"cat hello.txt"),
            (b"cat".as_slice(), b"hello.txt".as_slice())
        );
        // Extra spaces between them are not part of either.
        assert_eq!(
            split_once(b"  cat   hello.txt  "),
            (b"cat".as_slice(), b"hello.txt".as_slice())
        );
        // But spaces inside what is left are: they are what somebody typed
        // into a file.
        assert_eq!(
            split_once(b"write a.txt one  two"),
            (b"write".as_slice(), b"a.txt one  two".as_slice())
        );
        assert_eq!(split_once(b""), (b"".as_slice(), b"".as_slice()));
        assert_eq!(split_once(b"    "), (b"".as_slice(), b"".as_slice()));
    }

    #[test]
    fn a_name_is_refused_before_the_kernel_has_to() {
        assert_eq!(as_name(b"HELLO.TXT"), Some("HELLO.TXT"));
        assert_eq!(as_name(b"ABCDEFGH.IJK"), Some("ABCDEFGH.IJK"));
        assert_eq!(as_name(b""), None, "nothing is not a name");
        assert_eq!(as_name(b"ABCDEFGHI.JKL"), None, "one byte too long");
        assert_eq!(as_name(&[0xFF, 0xFE]), None, "not text");
    }

    #[test]
    fn a_number_is_written_the_way_a_person_reads_it() {
        let mut into = [0u8; 10];
        for (number, expected) in [
            (0u32, "0".as_bytes()),
            (7, b"7"),
            (27, b"27"),
            (2049, b"2049"),
            (1_000_000, b"1000000"),
            (u32::MAX, b"4294967295"),
        ] {
            let written = decimal(number, &mut into);
            assert_eq!(&into[..written], expected, "{number}");
        }
    }

    /// The digits come out in the order a person reads them, not reversed.
    /// Written backwards, 2049 would print as 9402 and look plausible.
    #[test]
    fn the_digits_are_not_backwards() {
        let mut into = [0u8; 10];
        let written = decimal(1234, &mut into);
        assert_eq!(&into[..written], b"1234");
        assert_ne!(&into[..written], b"4321");
    }
}
