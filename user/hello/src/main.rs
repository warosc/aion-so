//! The first user program that is a program, and the first that uses the
//! disk.
//!
//! Until now a user program was a table of bytes written by hand inside
//! the kernel, with its offsets computed on paper
//! (docs/adr/0014-fase3-syscall-abi-v0.md, point 10). This one is an
//! ordinary crate: the compiler lays it out, the linker places it, and the
//! kernel reads it off the disk as an ELF
//! (docs/adr/0026-fase4-elf-user-programs.md).
//!
//! It now also reads and writes files through the ABI of
//! docs/adr/0028-fase4-file-abi-v0.md — descriptors for reading, the whole
//! file for writing — and tries four things it should not be allowed to
//! do. A program that only does what it is allowed to proves half of an
//! isolation boundary.
//!
//! It is `no_std` and it has no runtime. Nothing has set up a stack guard,
//! a heap or an unwinder, and nothing will: what the kernel gives it is a
//! page of stack and an entry point.

#![no_std]
#![no_main]

use harlan_user_abi as abi;

/// `read` with the buffer as a raw address, for the probe that hands the
/// kernel somewhere it must not fill.
///
/// Local, and not in `abi`, for the reason it exists: the typed wrapper
/// takes a `&mut [u8]`, and there is no such slice for "this program's own
/// code". A binding that made this easy would be a binding that made the
/// mistake easy.
///
/// # Safety
///
/// The kernel refuses an address that is not this program's writable
/// memory, so nothing is written; a kernel that did not check would fault
/// in ring 0, which is what this probe exists to rule out.
unsafe fn read_raw(descriptor: i64, at: *mut u8, len: usize) -> i64 {
    let result: i64;
    // SAFETY: `syscall` destroys `rcx` and `r11`, which the clobbers say.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") abi::READ => result,
            in("rdi") descriptor,
            in("rsi") at,
            in("rdx") len,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// `list` with the buffer as a raw address, for the same probe and the same
/// reason.
///
/// # Safety
///
/// As `read_raw`.
unsafe fn list_raw(index: u64, at: *mut u8, len: usize) -> i64 {
    let result: i64;
    // SAFETY: as `read_raw`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") abi::LIST => result,
            in("rdi") index,
            in("rsi") at,
            in("rdx") len,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Says whether a check passed, in **one** line, which is what makes the
/// log readable and a `grep` for a failure reliable.
///
/// Built in a buffer rather than logged in three calls: three calls are
/// three lines in the kernel's log, and a verdict split from what it is a
/// verdict about is a verdict nobody can read. There is no allocator here,
/// so the buffer is on the stack and the copy is a loop — `copy_from_slice`
/// panics on a length mismatch, and a panic in a `no_std` program drags in
/// the formatting machinery that builds its message.
///
/// # Safety
///
/// As `log`.
unsafe fn say(passed: bool, what: &str) -> bool {
    const OK: &str = "HARLAN: ring3-file OK: ";
    const FAILED: &str = "HARLAN: ring3-file FAILED: ";
    let verdict = if passed { OK } else { FAILED };

    // The newline goes in the buffer too: logged separately it would be a
    // line of its own.
    let mut line = [0u8; 128];
    let mut at = 0;
    for byte in verdict
        .as_bytes()
        .iter()
        .chain(what.as_bytes())
        .chain(b"\n")
    {
        match line.get_mut(at) {
            Some(slot) => *slot = *byte,
            // Truncated rather than wrapped or panicking. Everything here
            // is an ASCII literal, so a cut cannot land inside a character
            // and what is left is still text.
            None => break,
        }
        at += 1;
    }

    // SAFETY: forwarded from this function's contract.
    unsafe {
        abi::log(core::str::from_utf8(line.get(..at).unwrap_or(&[])).unwrap_or("HARLAN: ?\n"));
    }
    passed
}

/// What this program writes, and then reads back to see that it is there.
const WRITTEN: &str = "written from ring 3\n";

/// Reads a whole file through a descriptor, in pieces, and answers whether
/// the bytes are `expected`.
///
/// Deliberately a small buffer: the point of a descriptor is that a file
/// does not have to fit in one, so a program that reads in eight-byte
/// pieces is exercising what the descriptor is for (ADR 0028, point 1).
///
/// # Safety
///
/// As `log`.
unsafe fn reads_back(name: &str, expected: &str) -> bool {
    // SAFETY: forwarded.
    let descriptor = unsafe { abi::open(name) };
    if descriptor < 0 {
        return false;
    }
    let mut piece = [0u8; 8];
    let mut at = 0;
    let mut same = true;
    let want = expected.as_bytes();
    loop {
        // SAFETY: forwarded. The buffer is this program's stack, which is
        // its own and writable.
        let got = unsafe { abi::read(descriptor, &mut piece) };
        if got <= 0 {
            break;
        }
        // Nothing here indexes a slice or takes a subslice. Either can
        // panic, and a panic in a `no_std` program drags in the formatting
        // machinery that builds its message: the first version of this
        // function made the compiled program 757 KB instead of 7 KB, which
        // the kernel then refused as too big to load. `iter().zip()` and
        // `get()` cannot be out of bounds, so no panic path is reachable
        // and none is emitted.
        for (step, byte) in piece.iter().enumerate() {
            if step as i64 >= got {
                break;
            }
            if want.get(at) != Some(byte) {
                same = false;
            }
            at += 1;
        }
    }
    // SAFETY: forwarded.
    unsafe { abi::close(descriptor) };
    same && at == expected.len()
}

/// Where the kernel jumps. `no_mangle` because the entry point is found by
/// name in the ELF, and `extern "C"` because nothing calls it from Rust.
///
/// # Safety
///
/// Called once, by the kernel, with a stack of its own and nothing else
/// set up.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    // SAFETY: this is the process the kernel just started, and the text is
    // this program's own.
    unsafe {
        abi::log("HARLAN: hello from a program that came off the disk\n");
        abi::log("HARLAN: compiled by the toolchain, loaded as ELF\n");
    }

    let mut all = true;

    // What it is allowed to do: read a file the disk came with, in pieces
    // smaller than the file.
    // SAFETY: as above, for every call in this function.
    all &= unsafe {
        say(
            reads_back("HELLO.TXT", "HARLAN reads its own disk.\n"),
            "read a file off the disk in eight-byte pieces",
        )
    };

    // Write one of its own, and find it again.
    let written = unsafe { abi::write_file("RING3.TXT", WRITTEN.as_bytes()) };
    all &= unsafe { say(written == WRITTEN.len() as i64, "wrote a file from ring 3") };
    all &= unsafe {
        say(
            reads_back("RING3.TXT", WRITTEN),
            "read back what it had written",
        )
    };

    // And four things it must not be allowed to do.
    //
    // A descriptor it never opened.
    all &= unsafe {
        let mut buffer = [0u8; 4];
        say(
            abi::read(3, &mut buffer) == abi::ERR_BAD_DESCRIPTOR,
            "refused a descriptor it never opened",
        )
    };
    // A file that is not there.
    all &= unsafe {
        say(
            abi::open("NOPE.TXT") == abi::ERR_NO_SUCH_FILE,
            "refused a file that is not there",
        )
    };
    // A name the format cannot hold: a slash, which is also the only way
    // this ABI could be made to leave the root directory (ADR 0028,
    // point 10).
    all &= unsafe {
        say(
            abi::open("A/B.TXT") == abi::ERR_BAD_ARGUMENT,
            "refused a name with a slash in it",
        )
    };
    // And a buffer inside its own code, which is its memory and is not
    // writable. A kernel that checked only ownership would write it from
    // ring 0 and fault inside itself (ADR 0028, point 9).
    all &= unsafe {
        let descriptor = abi::open("HELLO.TXT");
        let refused = read_raw(descriptor, _start as *mut u8, 4) == abi::ERR_BAD_ARGUMENT;
        abi::close(descriptor);
        say(refused, "refused a buffer inside its own code")
    };

    // ---- the console and the directory (ADR 0029) ----

    // Listing: walk it until the kernel says there is no such index, and
    // check that a file the disk came with is in there with its real size.
    all &= unsafe {
        let mut record = [0u8; abi::ENTRY_BYTES];
        let mut seen = 0;
        let mut found_hello = false;
        let mut index = 0;
        loop {
            let got = abi::list(index, &mut record);
            if got == abi::ERR_NO_SUCH_FILE {
                break;
            }
            if got != abi::ENTRY_BYTES as i64 {
                seen = -1;
                break;
            }
            seen += 1;
            // The name is space-padded on the right; compare the start.
            if record.starts_with(b"HELLO.TXT") {
                // Bytes 16..20 are the size, little-endian. 27 is what
                // HELLO.TXT holds, and the directory bit must be clear.
                let size = u32::from_le_bytes([record[16], record[17], record[18], record[19]]);
                found_hello = size == 27 && record[12] & 0x10 == 0;
            }
            index += 1;
            if index > 64 {
                // A listing that never ends is a listing that is wrong.
                seen = -1;
                break;
            }
        }
        say(
            seen > 1 && found_hello,
            "listed the directory and found HELLO.TXT with its size",
        )
    };

    // A buffer one byte short of a record: refused, rather than given a
    // name without its size.
    all &= unsafe {
        let mut almost = [0u8; abi::ENTRY_BYTES - 1];
        say(
            abi::list(0, &mut almost) == abi::ERR_BAD_ARGUMENT,
            "refused a buffer too small for one entry",
        )
    };

    // And a record written into its own code, which is the same check as
    // for `read` and the same reason.
    all &= unsafe {
        say(
            list_raw(0, _start as *mut u8, abi::ENTRY_BYTES) == abi::ERR_BAD_ARGUMENT,
            "refused a listing into its own code",
        )
    };

    // Writing where the person reads, which is not where `log` goes.
    all &= unsafe {
        const SEEN: &[u8] = b"HARLAN: a program in ring 3 wrote this line.\n";
        say(
            abi::console_write(SEEN) == SEEN.len() as i64,
            "wrote to the console the person reads",
        )
    };

    // Bytes that are not text: the console draws text, so this is refused
    // rather than drawn as rubbish.
    all &= unsafe {
        say(
            abi::console_write(&[0xFF, 0xFE, 0xFD]) == abi::ERR_BAD_ARGUMENT,
            "refused console bytes that are not text",
        )
    };

    // Reading a key: on an unattended boot there is none, and the answer
    // must be zero **and must come back**. A call that waited here would
    // wait for a keyboard interrupt with the clock stopped, which is the
    // machine and not this process (ADR 0029, point 11).
    all &= unsafe {
        let key = abi::console_read();
        say(
            key == 0,
            "asked for a key, was told there is none, and came back",
        )
    };

    // There was a bounded wait for a **real** key here, to prove that a
    // keypress comes back through this call. The shell in ring 3 proves it
    // far better — it echoes everything typed and answers it — so this is
    // gone, and with it two costs:
    //
    //   - about a second of every boot and every CI run, because each try
    //     is a syscall and a yield;
    //   - **keys stolen from the shell.** The console is first come, first
    //     served among processes: there is one key queue and no owner, so
    //     a program polling for a key takes one meant for somebody else.
    //     This program ran before the shell and ate the first character of
    //     the first command typed at it (ADR 0030).

    // SAFETY: as above.
    unsafe {
        abi::log(if all {
            "HARLAN: ring3-file ALL OK\n"
        } else {
            "HARLAN: ring3-file SOMETHING FAILED\n"
        });
    }
    // SAFETY: as above; nothing after it runs.
    unsafe { abi::exit(if all { 0 } else { 1 }) }
}

/// Nothing catches a panic here: there is no unwinder and nowhere to
/// report to but the kernel, so a panic is the end of this process.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // SAFETY: the one thing left that is certainly safe to do.
    unsafe { abi::exit(255) }
}
