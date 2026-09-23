//! The first thing that runs without privileges.
//!
//! A flat program, assembled by hand and carried inside the kernel image,
//! copied into a page of its own and run in ring 3
//! (docs/adr/0014-fase3-syscall-abi-v0.md). No loader, no format, no
//! relocations: what it takes to prove that the kernel can hand over the
//! CPU, take it back through `syscall`, and not be reachable in between.
//!
//! One process, no scheduler, no address space of its own yet. Those are
//! the next increments; this one is about the boundary.

use harlan_arch_x86_64::syscall::SyscallFrame;
use harlan_hal::addr::VirtAddr;
use harlan_hal::{error, info};

use crate::process::Process;

/// Where the program is mapped. Low, but clear of the first megabyte and
/// of anything the firmware kept.
pub const PROGRAM_BASE: VirtAddr = VirtAddr::new(0x0040_0000);
/// Its stack, one page, a megabyte above the code.
pub const STACK_BASE: VirtAddr = VirtAddr::new(0x0050_0000);
pub const STACK_TOP: VirtAddr = VirtAddr::new(0x0050_1000);

/// What the program asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Log = 0,
    Exit = 1,
}

impl Call {
    fn from(number: u64) -> Option<Self> {
        match number {
            0 => Some(Call::Log),
            1 => Some(Call::Exit),
            _ => None,
        }
    }
}

/// Negative, because `rax` carries the result and a number is easier to
/// check than a bitfield (ADR 0014).
pub const ERR_UNKNOWN_CALL: i64 = -1;
pub const ERR_BAD_ARGUMENT: i64 = -2;

/// The program, assembled by hand:
///
/// ```text
///  0: b8 00 00 00 00     mov eax, 0          ; log
///  5: 48 8d 3d 15 00 00 00  lea rdi, [rip+21] ; the message
/// 12: be 1a 00 00 00     mov esi, 26         ; its length
/// 17: 0f 05              syscall
/// 19: b8 01 00 00 00     mov eax, 1          ; exit
/// 24: bf 07 00 00 00     mov edi, 7          ; with 7
/// 29: 0f 05              syscall
/// 31: 0f 0b              ud2                 ; never reached
/// 33: "HARLAN: hello from ring 3\n"
/// ```
pub static PROGRAM: [u8; 59] = {
    let mut program = [0u8; 59];
    program[0] = 0xB8; // mov eax, imm32
    program[5] = 0x48; // lea rdi, [rip+disp32]
    program[6] = 0x8D;
    program[7] = 0x3D;
    program[8] = MESSAGE_AT - 12; // from the end of this instruction
    program[12] = 0xBE; // mov esi, imm32
    program[13] = MESSAGE_LEN;
    program[17] = 0x0F; // syscall
    program[18] = 0x05;
    program[19] = 0xB8; // mov eax, imm32
    program[20] = 1;
    program[24] = 0xBF; // mov edi, imm32
    program[25] = 7;
    program[29] = 0x0F; // syscall
    program[30] = 0x05;
    program[31] = 0x0F; // ud2
    program[32] = 0x0B;
    let message = MESSAGE.as_bytes();
    let mut index = 0;
    while index < message.len() {
        program[MESSAGE_AT as usize + index] = message[index];
        index += 1;
    }
    program
};

const MESSAGE: &str = "HARLAN: hello from ring 3\n";
const MESSAGE_LEN: u8 = MESSAGE.len() as u8;
const MESSAGE_AT: u8 = 33;

/// The process that is running, for the handler to check its pointers
/// against, and where to continue once it exits. Written before ring 3 is
/// entered and read only from the syscall handler.
static mut CURRENT: Option<&'static Process> = None;
static mut ON_EXIT: Option<(extern "C" fn(*mut u8) -> !, *mut u8)> = None;

/// Handles one syscall. Runs on the kernel's syscall stack with
/// interrupts disabled (`FMASK`).
pub fn handle(frame: &mut SyscallFrame) {
    // SAFETY: single core, and this is only reachable from the syscall
    // stub, which cannot run before `enter` set these.
    let running = unsafe { CURRENT };
    let Some(running) = running else {
        error!("HARLAN: a syscall arrived with no process running");
        frame.rax = ERR_UNKNOWN_CALL as u64;
        return;
    };

    match Call::from(frame.rax) {
        Some(Call::Log) => {
            let (ptr, len) = (frame.rdi, frame.rsi);
            if !running.owns(ptr, len) || len > 4096 {
                error!("HARLAN: syscall log({ptr:#x}, {len}) is not this process's memory");
                frame.rax = ERR_BAD_ARGUMENT as u64;
                return;
            }
            // SAFETY: the range was just checked to be inside the pages
            // this process was given, which are mapped in the space that
            // is active and stay mapped while it runs.
            let bytes = unsafe {
                core::slice::from_raw_parts(VirtAddr::new(ptr).as_ptr::<u8>(), len as usize)
            };
            match core::str::from_utf8(bytes) {
                Ok(text) => {
                    info!("HARLAN: from ring 3: {}", text.trim_end());
                    frame.rax = len;
                }
                Err(_) => {
                    error!("HARLAN: syscall log() was handed something that is not text");
                    frame.rax = ERR_BAD_ARGUMENT as u64;
                }
            }
        }
        Some(Call::Exit) => {
            info!("HARLAN: the process exited with {}", frame.rdi);
            // SAFETY: as above; set before ring 3 was entered.
            let resume = unsafe { ON_EXIT };
            let Some((resume, argument)) = resume else {
                panic!("a program exited with nowhere for the kernel to go back to");
            };
            // The kernel goes on here, on the syscall stack, which is one
            // of its own with guard pages around it.
            resume(argument)
        }
        None => {
            error!("HARLAN: unknown syscall {}", frame.rax);
            frame.rax = ERR_UNKNOWN_CALL as u64;
        }
    }
}

/// Runs `process` in ring 3, in its own address space. Never returns: it
/// leaves through `exit`, which continues the kernel at `resume`.
///
/// # Safety
///
/// `syscall::init` must have run with a stack of the kernel's own,
/// `process` must be one `spawn` built, and `resume` must be safe to call
/// on that syscall stack, in the kernel's own space.
pub unsafe fn enter(
    process: &'static Process,
    resume: extern "C" fn(*mut u8) -> !,
    argument: *mut u8,
) -> ! {
    // SAFETY: single core, before any user code exists.
    unsafe {
        CURRENT = Some(process);
        ON_EXIT = Some((resume, argument));
    }
    harlan_arch_x86_64::syscall::set_handler(handle);
    info!(
        "HARLAN: entering ring 3 at {:#x} with a stack at {:#x}, in the space at {:#x}",
        process.entry(),
        process.stack_top(),
        process.space.root()
    );
    // SAFETY: the kernel runs in the higher half, which this space shares,
    // and holds no pointer into the lower half of the one it is leaving.
    unsafe { process.activate() };
    // SAFETY: the pages are mapped for ring 3 in the space just made
    // active, the stack top is page-aligned, and the syscall path is set
    // up (the caller's contract).
    unsafe {
        harlan_arch_x86_64::user::enter(process.entry().as_u64(), process.stack_top().as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-assembled bytes have to mean what the comment says: the
    /// `lea` has to land on the message and the length has to match it.
    #[test]
    fn the_program_points_at_its_own_message() {
        let displacement = i32::from_le_bytes([PROGRAM[8], PROGRAM[9], PROGRAM[10], PROGRAM[11]]);
        // `lea rdi, [rip + disp]`, and rip is the end of that instruction.
        let target = 12 + displacement;
        assert_eq!(target as usize, MESSAGE_AT as usize);
        let length = u32::from_le_bytes([PROGRAM[13], PROGRAM[14], PROGRAM[15], PROGRAM[16]]);
        assert_eq!(length as usize, MESSAGE.len());
        assert_eq!(
            &PROGRAM[MESSAGE_AT as usize..MESSAGE_AT as usize + MESSAGE.len()],
            MESSAGE.as_bytes()
        );
        // Both syscalls are there, and the trap after them.
        assert_eq!((PROGRAM[17], PROGRAM[18]), (0x0F, 0x05));
        assert_eq!((PROGRAM[29], PROGRAM[30]), (0x0F, 0x05));
        assert_eq!((PROGRAM[31], PROGRAM[32]), (0x0F, 0x0B));
        assert_eq!(PROGRAM[20], Call::Exit as u8);
        assert_eq!(PROGRAM[1], Call::Log as u8);
    }

    fn frame_for(call: Call, ptr: u64, len: u64) -> SyscallFrame {
        SyscallFrame {
            rax: call as u64,
            rdi: ptr,
            rsi: len,
            rdx: 0,
            r10: 0,
            r8: 0,
            r9: 0,
            user_rip: 0,
            user_rflags: 0,
        }
    }

    /// With no process running, a syscall is refused rather than
    /// answered with something that reads as success.
    #[test]
    fn a_syscall_with_no_process_running_is_refused() {
        // SAFETY: single-threaded test; nothing else reads this.
        unsafe { CURRENT = None };
        let mut frame = frame_for(Call::Log, 0x1000, 8);
        handle(&mut frame);
        assert_eq!(frame.rax as i64, ERR_UNKNOWN_CALL);
    }

    #[test]
    fn only_the_calls_of_this_abi_exist() {
        assert_eq!(Call::from(0), Some(Call::Log));
        assert_eq!(Call::from(1), Some(Call::Exit));
        assert_eq!(Call::from(2), None);
        assert_eq!(Call::from(u64::MAX), None);
    }
}
