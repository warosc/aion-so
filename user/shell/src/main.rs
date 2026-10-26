//! The shell, in ring 3 (docs/adr/0030-fase4-shell-in-ring-3.md).
//!
//! The one in the kernel has been there since Fase 1. This one does the
//! same job from the other side of the privilege boundary: it reads keys
//! and prints through `console_read`/`console_write` (ADR 0029), and it
//! reaches files through `open`/`read`/`close`/`write_file`/`list`
//! (ADR 0028, ADR 0029). Nothing here is done for it by the kernel that is
//! not done for any other program.
//!
//! That is the point. A shell is the first program a person drives, and a
//! shell that needed privileges nobody else gets would mean the boundary
//! has a hole shaped like a shell.
//!
//! `no_std`, no runtime, no heap: one page of stack and an entry point.
//! Every buffer here is a fixed array, and none of them is large, because
//! that page is all there is.

// Freestanding when it is the program, ordinary when it is a test target.
// `cargo test -p harlan-shell` builds this binary too, and a `no_std`
// binary with its own panic handler collides with the one std brings.
#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

use harlan_shell::{as_name, decimal, split_once, trimmed};
use harlan_user_abi as abi;

/// Logged once the shell is reading keys. `cargo xtask boot-test` greps for
/// it, and it is **not** the kernel shell's marker: a boot where the
/// program failed to load and the kernel's own shell came up instead would
/// otherwise look exactly like a boot where this one did.
const READY_MARKER: &str = "HARLAN-RING3-SHELL-READY";

/// What the person types at. Through the console, so it is seen; the
/// marker above goes to the log, which is a different device.
const PROMPT: &[u8] = b"harlan$ ";

/// The longest line it will take. Fixed, because there is no heap, and
/// generous beside the commands it knows.
const LINE_MAX: usize = 128;

/// How much of a file `cat` moves at a time. A descriptor exists so that a
/// file does not have to fit in a buffer (ADR 0028, point 1), so this is
/// deliberately far smaller than the files on the disk.
const PIECE: usize = 64;

/// Where the kernel jumps. `no_mangle` because the entry point is found by
/// name in the ELF, and `extern "C"` because nothing calls it from Rust.
///
/// # Safety
///
/// Called once, by the kernel, with a stack of its own and nothing else set
/// up: no heap, no unwinder, no guard but the page the kernel left unmapped
/// below the stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    // SAFETY: this is the process the kernel just started; every pointer
    // handed over below is this program's own, and the kernel checks each
    // one before reading it.
    unsafe {
        say(b"HARLAN OS shell, in ring 3.\n");
        say(b"Type `help` for what it knows.\n");
        // To the log, not the console: this is for the boot record and for
        // the test that greps it.
        abi::log(READY_MARKER);
        abi::log("\n");
    }

    let mut line = [0u8; LINE_MAX];
    loop {
        // SAFETY: as above.
        unsafe {
            say(PROMPT);
            let len = read_line(&mut line);
            run(&line[..len]);
        }
    }
}

/// Writes to the console, dropping the answer: there is nothing useful to
/// do about a console that could not take it, and a shell that stopped
/// working because the screen did would be worse than one that carries on.
///
/// # Safety
///
/// `text` must be this program's own memory.
unsafe fn say(text: &[u8]) {
    // SAFETY: forwarded.
    unsafe {
        abi::console_write(text);
    }
}

/// Collects one line, echoing as it goes, until Enter.
///
/// Waiting is ask-yield-ask, because `console_read` does not block
/// (ADR 0029, point 11). That makes this a busy wait while nothing else is
/// runnable, which is the cost ADR 0029 names and Fase 5 is meant to
/// remove.
///
/// # Safety
///
/// `line` must be this program's own memory.
unsafe fn read_line(line: &mut [u8; LINE_MAX]) -> usize {
    let mut len = 0;
    loop {
        // SAFETY: forwarded.
        let key = unsafe { abi::console_read() };
        match key {
            abi::KEY_NONE => {
                // SAFETY: forwarded.
                unsafe { abi::yield_now() };
            }
            abi::KEY_ENTER => {
                // SAFETY: forwarded.
                unsafe { say(b"\n") };
                return len;
            }
            abi::KEY_BACKSPACE => {
                if len > 0 {
                    len -= 1;
                    // Back, over, back: how a character is unwritten on a
                    // console that only moves forward.
                    // SAFETY: forwarded.
                    unsafe { say(b"\x08 \x08") };
                }
            }
            abi::KEY_UNKNOWN => {}
            other => {
                // Printable ASCII, which is the only thing `console_read`
                // gives back as a character (ADR 0029, point 13). Dropped
                // once the line is full, rather than wrapping into
                // something the person did not type.
                if (0x20..=0x7E).contains(&other) && len < line.len() {
                    line[len] = other as u8;
                    len += 1;
                    // SAFETY: forwarded; the slice is this line's own byte.
                    unsafe { say(&line[len - 1..len]) };
                }
            }
        }
    }
}

/// Runs one line.
///
/// # Safety
///
/// `line` must be this program's own memory.
unsafe fn run(line: &[u8]) {
    let line = trimmed(line);
    let (command, rest) = split_once(line);
    // SAFETY: forwarded for every call below.
    unsafe {
        match command {
            b"" => {}
            b"help" => {
                say(b"help            what you are reading\n");
                say(b"ls              what is on the disk\n");
                say(b"cat NAME        print a file\n");
                say(b"write NAME TEXT write a file, replacing it\n");
                say(b"echo TEXT       print TEXT\n");
                say(b"exit            stop this shell\n");
            }
            b"ls" => list_files(),
            b"cat" => cat(rest),
            b"write" => write(rest),
            b"echo" => {
                say(rest);
                say(b"\n");
            }
            b"exit" => {
                say(b"bye\n");
                abi::log("HARLAN: the shell in ring 3 was told to exit\n");
                abi::exit(0);
            }
            other => {
                say(b"unknown command: ");
                say(other);
                say(b"\n");
            }
        }
    }
}

/// Lists the root directory: one call per entry, until the kernel says
/// there is no such index (ADR 0029, point 2).
///
/// # Safety
///
/// As `say`.
unsafe fn list_files() {
    let mut record = [0u8; abi::ENTRY_BYTES];
    let mut index = 0;
    let mut shown = 0;
    loop {
        // SAFETY: forwarded; `record` is this function's own.
        let got = unsafe { abi::list(index, &mut record) };
        if got < 0 {
            break;
        }
        let Some(name) = abi::entry_name(&record) else {
            break;
        };
        // SAFETY: forwarded.
        unsafe {
            say(name.as_bytes());
            // Pad out to a column, so sizes line up.
            let mut pad = name.len();
            while pad < abi::MAX_NAME + 2 {
                say(b" ");
                pad += 1;
            }
            if abi::entry_is_directory(&record) {
                say(b"   <dir>\n");
            } else {
                let mut digits = [0u8; 10];
                let written = decimal(abi::entry_size(&record).unwrap_or(0), &mut digits);
                say(&digits[..written]);
                say(b"\n");
            }
        }
        shown += 1;
        index += 1;
    }
    if shown == 0 {
        // SAFETY: forwarded.
        unsafe { say(b"nothing on the disk, or no disk\n") };
    }
}

/// Prints a file, in pieces much smaller than it: a descriptor exists so
/// that a file does not have to fit in a buffer.
///
/// # Safety
///
/// As `say`.
unsafe fn cat(rest: &[u8]) {
    let (name, _) = split_once(rest);
    let Some(name) = as_name(name) else {
        // SAFETY: forwarded.
        unsafe { say(b"cat: give it a name\n") };
        return;
    };
    // SAFETY: forwarded.
    let descriptor = unsafe { abi::open(name) };
    if descriptor < 0 {
        // SAFETY: forwarded.
        unsafe { complain(b"cat", name.as_bytes(), descriptor) };
        return;
    }
    let mut piece = [0u8; PIECE];
    loop {
        // SAFETY: forwarded; `piece` is this function's own.
        let got = unsafe { abi::read(descriptor, &mut piece) };
        if got <= 0 {
            break;
        }
        // SAFETY: forwarded; `got` is at most `piece.len()`.
        unsafe { say(&piece[..got as usize]) };
    }
    // SAFETY: forwarded.
    unsafe { abi::close(descriptor) };
}

/// Writes a whole file: the name, then everything after it
/// (ADR 0028, point 1 — there is no writing part of a file).
///
/// # Safety
///
/// As `say`.
unsafe fn write(rest: &[u8]) {
    let (name, text) = split_once(rest);
    let Some(name) = as_name(name) else {
        // SAFETY: forwarded.
        unsafe { say(b"write: give it a name and some text\n") };
        return;
    };
    // A line, because a file somebody will `cat` should end in one.
    let mut contents = [0u8; LINE_MAX + 1];
    let mut len = 0;
    for byte in text {
        if len == contents.len() - 1 {
            break;
        }
        contents[len] = *byte;
        len += 1;
    }
    contents[len] = b'\n';
    len += 1;

    // SAFETY: forwarded; both slices are this function's own.
    let written = unsafe { abi::write_file(name, &contents[..len]) };
    // SAFETY: forwarded.
    unsafe {
        if written < 0 {
            complain(b"write", name.as_bytes(), written);
        } else {
            say(b"wrote ");
            say(name.as_bytes());
            say(b"\n");
        }
    }
}

/// Says what went wrong in words, rather than printing the number.
///
/// # Safety
///
/// As `say`.
unsafe fn complain(what: &[u8], name: &[u8], err: i64) {
    // SAFETY: forwarded.
    unsafe {
        say(what);
        say(b": ");
        say(name);
        say(b": ");
        say(match err {
            abi::ERR_NO_SUCH_FILE => b"no such file\n".as_slice(),
            abi::ERR_BAD_ARGUMENT => b"not a name this disk can hold\n".as_slice(),
            abi::ERR_FILE_IS_OPEN => b"something has it open\n".as_slice(),
            abi::ERR_VOLUME_FULL => b"the disk is full\n".as_slice(),
            abi::ERR_TOO_MANY_OPEN => b"too many files open\n".as_slice(),
            abi::ERR_DISK => b"the disk would not answer\n".as_slice(),
            _ => b"refused\n".as_slice(),
        });
    }
}

/// Nothing catches a panic here: there is no unwinder and nowhere to report
/// to but the kernel, so a panic is the end of this process.
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // SAFETY: the one thing left that is certainly safe to do.
    unsafe { abi::exit(255) }
}
