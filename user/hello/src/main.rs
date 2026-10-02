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

/// The system calls. 0 and 1 are ADR 0014's, 5 to 8 are ADR 0028's.
const LOG: u64 = 0;
const EXIT: u64 = 1;
const OPEN: u64 = 5;
const READ: u64 = 6;
const CLOSE: u64 = 7;
const WRITE_FILE: u64 = 8;
const YIELD: u64 = 2;
const LIST: u64 = 9;
const CONSOLE_WRITE: u64 = 10;
const CONSOLE_READ: u64 = 11;

/// How big one directory entry is on the wire (ADR 0029, point 5).
const ENTRY_BYTES: usize = 20;

/// The errors this program expects to be given, by the numbers ADR 0028
/// point 13 fixes. Named here so that a probe says what it is checking
/// for rather than comparing against a bare `-2`.
const ERR_BAD_ARGUMENT: i64 = -2;
const ERR_BAD_DESCRIPTOR: i64 = -7;
const ERR_NO_SUCH_FILE: i64 = -9;

/// Writes `text` to the kernel's log.
///
/// # Safety
///
/// Only reachable from this program, running in ring 3 with the kernel's
/// syscall path set up. The kernel checks the pointer against this
/// process's own memory before reading a byte of it (ADR 0014, point 9),
/// so the worst a mistake here can do is be refused.
unsafe fn log(text: &str) -> i64 {
    let result: i64;
    // SAFETY: `syscall` is how ring 3 asks the kernel for something. It
    // destroys `rcx` and `r11`, which the clobbers say, and the kernel
    // returns the result in `rax`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") LOG => result,
            in("rdi") text.as_ptr(),
            in("rsi") text.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Opens a file in the root directory, answering a descriptor or an error.
///
/// # Safety
///
/// As `log`.
unsafe fn open(name: &str) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") OPEN => result,
            in("rdi") name.as_ptr(),
            in("rsi") name.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Reads from a descriptor into `into`, answering how many bytes landed
/// there. Zero means the file ended (ADR 0028, point 3).
///
/// # Safety
///
/// As `log`. The kernel checks that `into` is memory this program owns
/// **and may write** before filling it, so handing it the wrong thing is
/// refused rather than served.
unsafe fn read(descriptor: i64, into: &mut [u8]) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") READ => result,
            in("rdi") descriptor,
            in("rsi") into.as_mut_ptr(),
            in("rdx") into.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// The same as `read`, with the buffer given as a raw address and length.
///
/// For the probes, which hand the kernel somewhere this program must not
/// be allowed to have filled — its own code. There is no safe `&mut [u8]`
/// for that, which is the point.
///
/// # Safety
///
/// As `log`. The kernel refuses an address that is not this program's
/// writable memory, so nothing is written; a kernel that did not check
/// would fault in ring 0, which is what this probe exists to rule out.
unsafe fn read_raw(descriptor: i64, at: *mut u8, len: usize) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") READ => result,
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

/// Closes a descriptor.
///
/// # Safety
///
/// As `log`.
unsafe fn close(descriptor: i64) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") CLOSE => result,
            in("rdi") descriptor,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Writes a whole file, creating or replacing it (ADR 0028, point 1).
///
/// # Safety
///
/// As `log`.
unsafe fn write_file(name: &str, contents: &[u8]) -> i64 {
    let result: i64;
    // SAFETY: as `log`. `r10` carries the fourth argument, not `rcx`,
    // because `syscall` destroys `rcx` (ADR 0014, point 5).
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") WRITE_FILE => result,
            in("rdi") name.as_ptr(),
            in("rsi") name.len(),
            in("rdx") contents.as_ptr(),
            in("r10") contents.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Writes one directory entry into `into`, answering how many bytes it put
/// there. A 20-byte record (ADR 0029, point 5).
///
/// # Safety
///
/// As `log`. The kernel checks that `into` is memory this program owns and
/// may write before filling it.
unsafe fn list(index: u64, into: &mut [u8]) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") LIST => result,
            in("rdi") index,
            in("rsi") into.as_mut_ptr(),
            in("rdx") into.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// `list` with the buffer as a raw address, for the probe that hands the
/// kernel somewhere it must not fill.
///
/// # Safety
///
/// As `read_raw`: the kernel refuses an address that is not this program's
/// writable memory, so nothing is written.
unsafe fn list_raw(index: u64, at: *mut u8, len: usize) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") LIST => result,
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

/// Writes text where the person can read it — the framebuffer, not the
/// kernel's record. `log` is the other channel (ADR 0029, point 7).
///
/// # Safety
///
/// As `log`.
unsafe fn console_write(bytes: &[u8]) -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") CONSOLE_WRITE => result,
            in("rdi") bytes.as_ptr(),
            in("rsi") bytes.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Takes one key if one is waiting. Zero means none is, not end
/// (ADR 0029, point 10). Never waits — waiting would stop the machine.
///
/// # Safety
///
/// As `log`.
unsafe fn console_read() -> i64 {
    let result: i64;
    // SAFETY: as `log`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") CONSOLE_READ => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Gives the CPU to whoever is next and comes back when this process's turn
/// comes round again (ADR 0019).
///
/// How a program waits for something, since nothing in this ABI blocks: it
/// asks, yields, and asks again.
///
/// # Safety
///
/// As `log`.
unsafe fn yield_now() {
    // SAFETY: as `log`. Nothing is read back: the call answers nothing.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") YIELD => _,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
}

/// Stops this process. Never returns: the kernel gives the CPU to somebody
/// else and never switches back.
fn exit(code: u64) -> ! {
    // SAFETY: as `log`. `noreturn` says what is true of this one: the
    // kernel gives the CPU to somebody else and never switches back, so
    // nothing after it runs. It takes no outputs for that reason — there
    // is nowhere for them to land.
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") EXIT,
            in("rdi") code,
            options(nostack, noreturn),
        );
    }
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
        log(core::str::from_utf8(line.get(..at).unwrap_or(&[])).unwrap_or("HARLAN: ?\n"));
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
    let descriptor = unsafe { open(name) };
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
        let got = unsafe { read(descriptor, &mut piece) };
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
    unsafe { close(descriptor) };
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
        log("HARLAN: hello from a program that came off the disk\n");
        log("HARLAN: compiled by the toolchain, loaded as ELF\n");
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
    let written = unsafe { write_file("RING3.TXT", WRITTEN.as_bytes()) };
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
            read(3, &mut buffer) == ERR_BAD_DESCRIPTOR,
            "refused a descriptor it never opened",
        )
    };
    // A file that is not there.
    all &= unsafe {
        say(
            open("NOPE.TXT") == ERR_NO_SUCH_FILE,
            "refused a file that is not there",
        )
    };
    // A name the format cannot hold: a slash, which is also the only way
    // this ABI could be made to leave the root directory (ADR 0028,
    // point 10).
    all &= unsafe {
        say(
            open("A/B.TXT") == ERR_BAD_ARGUMENT,
            "refused a name with a slash in it",
        )
    };
    // And a buffer inside its own code, which is its memory and is not
    // writable. A kernel that checked only ownership would write it from
    // ring 0 and fault inside itself (ADR 0028, point 9).
    all &= unsafe {
        let descriptor = open("HELLO.TXT");
        let refused = read_raw(descriptor, _start as *mut u8, 4) == ERR_BAD_ARGUMENT;
        close(descriptor);
        say(refused, "refused a buffer inside its own code")
    };

    // ---- the console and the directory (ADR 0029) ----

    // Listing: walk it until the kernel says there is no such index, and
    // check that a file the disk came with is in there with its real size.
    all &= unsafe {
        let mut record = [0u8; ENTRY_BYTES];
        let mut seen = 0;
        let mut found_hello = false;
        let mut index = 0;
        loop {
            let got = list(index, &mut record);
            if got == ERR_NO_SUCH_FILE {
                break;
            }
            if got != ENTRY_BYTES as i64 {
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
        let mut almost = [0u8; ENTRY_BYTES - 1];
        say(
            list(0, &mut almost) == ERR_BAD_ARGUMENT,
            "refused a buffer too small for one entry",
        )
    };

    // And a record written into its own code, which is the same check as
    // for `read` and the same reason.
    all &= unsafe {
        say(
            list_raw(0, _start as *mut u8, ENTRY_BYTES) == ERR_BAD_ARGUMENT,
            "refused a listing into its own code",
        )
    };

    // Writing where the person reads, which is not where `log` goes.
    all &= unsafe {
        const SEEN: &[u8] = b"HARLAN: a program in ring 3 wrote this line.\n";
        say(
            console_write(SEEN) == SEEN.len() as i64,
            "wrote to the console the person reads",
        )
    };

    // Bytes that are not text: the console draws text, so this is refused
    // rather than drawn as rubbish.
    all &= unsafe {
        say(
            console_write(&[0xFF, 0xFE, 0xFD]) == ERR_BAD_ARGUMENT,
            "refused console bytes that are not text",
        )
    };

    // Reading a key: on an unattended boot there is none, and the answer
    // must be zero **and must come back**. A call that waited here would
    // wait for a keyboard interrupt with the clock stopped, which is the
    // machine and not this process (ADR 0029, point 11).
    all &= unsafe {
        let key = console_read();
        say(
            key == 0,
            "asked for a key, was told there is none, and came back",
        )
    };

    // And then a bounded wait for a real one, which is the half an
    // unattended boot cannot check: that a key pressed on the keyboard
    // comes back through this call.
    //
    // Waiting is `yield` and ask again, because the call does not block
    // (ADR 0029, point 12) — which is also what makes this a busy wait, so
    // it is bounded. Finding nothing is **not** a failure: nobody is
    // typing. What it proves when somebody is typing is driven by hand with
    // QEMU's monitor, and the line below is what says what arrived.
    unsafe {
        // Short on purpose. Each try is a syscall and a yield, so the
        // window costs real boot time on every boot and every CI run:
        // 400 000 tries added six seconds. This is about a second, which is
        // enough for a `sendkey` loop on QEMU's monitor to land in and
        // cheap enough to leave in.
        const TRIES: u32 = 60_000;
        let mut got = 0;
        let mut tries = 0;
        while tries < TRIES {
            let key = console_read();
            if key != 0 {
                got = key;
                break;
            }
            yield_now();
            tries += 1;
        }
        if got == 0 {
            log("HARLAN: ring3-key none arrived; nobody was typing\n");
        } else if (0x20..=0x7E).contains(&got) {
            // Echoed where the person reads it, so a screenshot shows the
            // character that was actually pressed.
            let typed = [got as u8];
            console_write(b"HARLAN: ring3-key got the character: ");
            console_write(&typed);
            console_write(b"\n");
            log("HARLAN: ring3-key a real keypress came back as a character\n");
        } else {
            console_write(b"HARLAN: ring3-key got a named key\n");
            log("HARLAN: ring3-key a real keypress came back as a named key\n");
        }
    }

    // SAFETY: as above.
    unsafe {
        log(if all {
            "HARLAN: ring3-file ALL OK\n"
        } else {
            "HARLAN: ring3-file SOMETHING FAILED\n"
        });
    }
    exit(if all { 0 } else { 1 })
}

/// Nothing catches a panic here: there is no unwinder and nowhere to
/// report to but the kernel, so a panic is the end of this process.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255)
}
