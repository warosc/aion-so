//! The first user program that is a program.
//!
//! Until now a user program was a table of bytes written by hand inside
//! the kernel, with its offsets computed on paper
//! (docs/adr/0014-fase3-syscall-abi-v0.md, point 10). This one is an
//! ordinary crate: the compiler lays it out, the linker places it, and the
//! kernel reads it off the disk as an ELF
//! (docs/adr/0026-fase4-elf-user-programs.md).
//!
//! It is `no_std` and it has no runtime. Nothing has set up a stack guard,
//! a heap or an unwinder, and nothing will: what the kernel gives it is a
//! page of stack and an entry point.

#![no_std]
#![no_main]

/// The system calls, as ADR 0014 numbers them.
const LOG: u64 = 0;
const EXIT: u64 = 1;

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
    exit(0)
}

/// Nothing catches a panic here: there is no unwinder and nowhere to
/// report to but the kernel, so a panic is the end of this process.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255)
}
