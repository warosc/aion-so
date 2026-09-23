//! The first thing that runs without privileges, and what it can ask for.
//!
//! Flat programs, assembled by hand and carried inside the kernel image,
//! each copied into a page of its own and run in ring 3
//! (docs/adr/0014-fase3-syscall-abi-v0.md). No loader, no format, no
//! relocations: what it takes to prove that the kernel can hand over the
//! CPU, take it back through `syscall`, and not be reachable in between.
//!
//! There are three of them now, run as four processes. Two processes run
//! the same talking program and take turns, which is what the scheduler
//! had to show (ADR 0018); the other two run a sender and a receiver and
//! exchange a message, which is what Fase 3 is for
//! (docs/adr/0019-fase3-ipc-v0.md). The message crosses from one address
//! space to another because the kernel copies it, and by no other route.

use harlan_arch_x86_64::syscall::SyscallFrame;
use harlan_hal::addr::VirtAddr;
use harlan_hal::{error, info};

use crate::ipc::{self, TakeError};
use crate::process::Process;
use crate::scheduler::{self, Running, SendError};

/// Where the program is mapped. Low, but clear of the first megabyte and
/// of anything the firmware kept.
pub const PROGRAM_BASE: VirtAddr = VirtAddr::new(0x0040_0000);
/// Its stack, one page, a megabyte above the code.
pub const STACK_BASE: VirtAddr = VirtAddr::new(0x0050_0000);
pub const STACK_TOP: VirtAddr = VirtAddr::new(0x0050_1000);

/// What a program asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Log = 0,
    Exit = 1,
    /// Gives the CPU to whoever is next, and comes back later. The
    /// timer does this too; this is the cooperative way, and it is what
    /// makes the test deterministic (ADR 0018).
    Yield = 2,
    /// Puts a message in another process's mailbox (ADR 0019). Does not
    /// wait: a full mailbox is an answer, not a pause.
    Send = 3,
    /// Takes the message waiting for this process, waiting until there is
    /// one.
    Recv = 4,
}

impl Call {
    fn from(number: u64) -> Option<Self> {
        match number {
            0 => Some(Call::Log),
            1 => Some(Call::Exit),
            2 => Some(Call::Yield),
            3 => Some(Call::Send),
            4 => Some(Call::Recv),
            _ => None,
        }
    }
}

/// Negative, because `rax` carries the result and a number is easier to
/// check than a bitfield (ADR 0014). `-3`, no permission, is reserved by
/// that ADR and has nothing to refuse yet.
pub const ERR_UNKNOWN_CALL: i64 = -1;
pub const ERR_BAD_ARGUMENT: i64 = -2;
/// The mailbox written to already holds a message nobody has read. The
/// sender decides what to do; the demonstration yields and tries again.
pub const ERR_MAILBOX_FULL: i64 = -4;
pub const ERR_NO_SUCH_PROCESS: i64 = -5;
/// Nothing is waiting and nobody is left who could ever send anything, so
/// waiting would be waiting for ever (ADR 0019, point 7).
pub const ERR_WOULD_WAIT_FOR_EVER: i64 = -6;

// ---------------------------------------------------------------------
// The programs
// ---------------------------------------------------------------------

/// Two processes saying who they are, taking turns:
///
/// ```text
///  0: b8 00 00 00 00        mov eax, 0           ; log
///  5: 48 8d 3d 2f 00 00 00  lea rdi, [rip+47]    ; the message
/// 12: be 1a 00 00 00        mov esi, 26          ; its length
/// 17: 0f 05                 syscall
/// 19: b8 02 00 00 00        mov eax, 2           ; yield
/// 24: 0f 05                 syscall
/// 26: b8 00 00 00 00        mov eax, 0           ; log, again
/// 31: 48 8d 3d 15 00 00 00  lea rdi, [rip+21]
/// 38: be 1a 00 00 00        mov esi, 26
/// 43: 0f 05                 syscall
/// 45: b8 01 00 00 00        mov eax, 1           ; exit
/// 50: bf 07 00 00 00        mov edi, 7           ; with 7
/// 55: 0f 05                 syscall
/// 57: 0f 0b                 ud2                  ; never reached
/// 59: "HARLAN: process _ speaking\n"
/// ```
///
/// The underscore is the badge: `talker_program` writes a digit there, so
/// that two processes running the same code can be told apart in the log.
pub const TALKER_LEN: usize = TALK_AT as usize + TALK.len();

/// Where in the message the badge goes.
const BADGE_AT: usize = TALK_AT as usize + 16;

const TALK: &str = "HARLAN: process _ speaking\n";
const TALK_LEN: u8 = TALK.len() as u8;
const TALK_AT: u8 = 59;

/// The talking program, with `badge` written into its message.
pub fn talker_program(badge: u8) -> [u8; TALKER_LEN] {
    let mut program = [0u8; TALKER_LEN];
    program[0] = 0xB8; // mov eax, imm32 (log)
    program[5] = 0x48; // lea rdi, [rip+disp32]
    program[6] = 0x8D;
    program[7] = 0x3D;
    program[8] = TALK_AT - 12;
    program[12] = 0xBE; // mov esi, imm32
    program[13] = TALK_LEN;
    program[17] = 0x0F; // syscall
    program[18] = 0x05;
    program[19] = 0xB8; // mov eax, imm32 (yield)
    program[20] = Call::Yield as u8;
    program[24] = 0x0F; // syscall
    program[25] = 0x05;
    program[26] = 0xB8; // mov eax, imm32 (log)
    program[31] = 0x48; // lea rdi, [rip+disp32]
    program[32] = 0x8D;
    program[33] = 0x3D;
    program[34] = TALK_AT - 38;
    program[38] = 0xBE; // mov esi, imm32
    program[39] = TALK_LEN;
    program[43] = 0x0F; // syscall
    program[44] = 0x05;
    program[45] = 0xB8; // mov eax, imm32 (exit)
    program[46] = Call::Exit as u8;
    program[50] = 0xBF; // mov edi, imm32
    program[51] = 7;
    program[55] = 0x0F; // syscall
    program[56] = 0x05;
    program[57] = 0x0F; // ud2
    program[58] = 0x0B;
    let message = TALK.as_bytes();
    let mut index = 0;
    while index < message.len() {
        program[TALK_AT as usize + index] = message[index];
        index += 1;
    }
    program[BADGE_AT] = badge;
    program
}

/// What the sender says. Under a mailbox's worth (`ipc::CAPACITY`), which
/// a test checks.
const MESSAGE: &str = "HARLAN: this crossed from one address space to another\n";
const MESSAGE_AT: u8 = 53;

/// The sender: one message, and the patience to wait for a mailbox that
/// somebody has not emptied yet.
///
/// ```text
///  0: b8 03 00 00 00        mov eax, 3           ; send
///  5: bf 02 00 00 00        mov edi, to          ; which slot
/// 10: 48 8d 35 24 00 00 00  lea rsi, [rip+36]    ; the message
/// 17: ba 37 00 00 00        mov edx, 55          ; its length
/// 22: 0f 05                 syscall
/// 24: 48 83 f8 fc           cmp rax, -4          ; was the mailbox full?
/// 28: 75 09                 jne +9               ; no: done
/// 30: b8 02 00 00 00        mov eax, 2           ; yield, and try again
/// 35: 0f 05                 syscall
/// 37: eb d9                 jmp -39              ; back to the top
/// 39: b8 01 00 00 00        mov eax, 1           ; exit
/// 44: bf 00 00 00 00        mov edi, 0
/// 49: 0f 05                 syscall
/// 51: 0f 0b                 ud2                  ; never reached
/// 53: "HARLAN: this crossed from one address space to another\n"
/// ```
pub const SENDER_LEN: usize = MESSAGE_AT as usize + MESSAGE.len();

/// The sending program, aimed at the process in slot `to`. The slot is
/// written in by the kernel that spawns it: v0 has no way for a program
/// to ask who is out there (ADR 0019, point 4).
pub fn sender_program(to: u8) -> [u8; SENDER_LEN] {
    let mut program = [0u8; SENDER_LEN];
    program[0] = 0xB8; // mov eax, imm32 (send)
    program[1] = Call::Send as u8;
    program[5] = 0xBF; // mov edi, imm32 (the slot)
    program[6] = to;
    program[10] = 0x48; // lea rsi, [rip+disp32]
    program[11] = 0x8D;
    program[12] = 0x35;
    program[13] = MESSAGE_AT - 17;
    program[17] = 0xBA; // mov edx, imm32 (the length)
    program[18] = MESSAGE.len() as u8;
    program[22] = 0x0F; // syscall
    program[23] = 0x05;
    program[24] = 0x48; // cmp rax, imm8
    program[25] = 0x83;
    program[26] = 0xF8;
    program[27] = ERR_MAILBOX_FULL as i8 as u8;
    program[28] = 0x75; // jne, past the retry
    program[29] = 9;
    program[30] = 0xB8; // mov eax, imm32 (yield)
    program[31] = Call::Yield as u8;
    program[35] = 0x0F; // syscall
    program[36] = 0x05;
    program[37] = 0xEB; // jmp, back to the top
    program[38] = (-39i8) as u8;
    program[39] = 0xB8; // mov eax, imm32 (exit)
    program[40] = Call::Exit as u8;
    program[44] = 0xBF; // mov edi, imm32 (with 0)
    program[49] = 0x0F; // syscall
    program[50] = 0x05;
    program[51] = 0x0F; // ud2
    program[52] = 0x0B;
    let message = MESSAGE.as_bytes();
    let mut index = 0;
    while index < message.len() {
        program[MESSAGE_AT as usize + index] = message[index];
        index += 1;
    }
    program
}

/// The receiver: waits for a message, says what arrived, and leaves.
///
/// ```text
///  0: b8 04 00 00 00        mov eax, 4           ; recv
///  5: bf 00 00 50 00        mov edi, 0x500000    ; the foot of its stack
/// 10: be 40 00 00 00        mov esi, 64          ; a mailbox's worth
/// 15: 0f 05                 syscall              ; rax = length
/// 17: 48 89 c6              mov rsi, rax         ; log exactly that much
/// 20: b8 00 00 00 00        mov eax, 0           ; log
/// 25: bf 00 00 50 00        mov edi, 0x500000
/// 30: 0f 05                 syscall
/// 32: 48 89 d7              mov rdi, rdx         ; who sent it
/// 35: b8 01 00 00 00        mov eax, 1           ; exit, with that
/// 40: 0f 05                 syscall
/// 42: 0f 0b                 ud2                  ; never reached
/// ```
///
/// The buffer is in its **stack** page, not its code page: the kernel
/// writes the message there, and the code page is read-only (ADR 0011).
///
/// It exits with the slot that wrote to it, which is the only way to see
/// from outside the kernel that `rdx` carried the sender's name back into
/// ring 3 (ADR 0019, point 8).
pub const RECEIVER_LEN: usize = 44;

/// Where the receiver asks for the message to be put: the foot of its
/// stack page, four kilobytes below where `rsp` starts.
pub const RECEIVE_BUFFER: u32 = STACK_BASE.as_u64() as u32;

pub fn receiver_program() -> [u8; RECEIVER_LEN] {
    let mut program = [0u8; RECEIVER_LEN];
    let buffer = RECEIVE_BUFFER.to_le_bytes();
    program[0] = 0xB8; // mov eax, imm32 (recv)
    program[1] = Call::Recv as u8;
    program[5] = 0xBF; // mov edi, imm32 (the buffer)
    program[6..10].copy_from_slice(&buffer);
    program[10] = 0xBE; // mov esi, imm32 (how much it can hold)
    program[11] = ipc::CAPACITY as u8;
    program[15] = 0x0F; // syscall
    program[16] = 0x05;
    program[17] = 0x48; // mov rsi, rax
    program[18] = 0x89;
    program[19] = 0xC6;
    program[20] = 0xB8; // mov eax, imm32 (log)
    program[21] = Call::Log as u8;
    program[25] = 0xBF; // mov edi, imm32 (the buffer again)
    program[26..30].copy_from_slice(&buffer);
    program[30] = 0x0F; // syscall
    program[31] = 0x05;
    program[32] = 0x48; // mov rdi, rdx (exit with who sent it)
    program[33] = 0x89;
    program[34] = 0xD7;
    program[35] = 0xB8; // mov eax, imm32 (exit)
    program[36] = Call::Exit as u8;
    program[40] = 0x0F; // syscall
    program[41] = 0x05;
    program[42] = 0x0F; // ud2
    program[43] = 0x0B;
    program
}

// ---------------------------------------------------------------------
// The kernel's side of the boundary
// ---------------------------------------------------------------------

/// Handles one syscall. Runs on the kernel stack of the process that made
/// it, with interrupts disabled (`FMASK`).
pub fn handle(frame: &mut SyscallFrame) {
    // Who is running is the scheduler's to say. It used to be kept here
    // as well, and a copy of it is a copy that goes stale on the next
    // switch: from then on the kernel would check one process's pointers
    // against another's memory.
    // SAFETY: this is only reachable from the syscall stub, on one core,
    // with interrupts off, while a process is running.
    let running = unsafe { scheduler::running() };
    let Some(running) = running else {
        error!("HARLAN: a syscall arrived with no process running");
        frame.rax = ERR_UNKNOWN_CALL as u64;
        return;
    };
    serve(frame, running);
}

/// The call itself, once it is known who asked. Split out from `handle`
/// so that what it refuses can be tested without a process.
fn serve(frame: &mut SyscallFrame, running: Running) {
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
            // Never comes back: the scheduler gives the CPU to whoever is
            // next, or to the kernel if nobody is.
            // SAFETY: this runs in the syscall handler, with interrupts
            // off, while this process is the one running.
            unsafe { crate::scheduler::exit_current(frame.rdi) };
        }
        Some(Call::Yield) => {
            // SAFETY: as above. Comes back when this process's turn
            // comes round again.
            unsafe { crate::scheduler::switch_to_next() };
            frame.rax = 0;
        }
        Some(Call::Send) => {
            let (to, ptr, len) = (frame.rdi, frame.rsi, frame.rdx);
            if !running.owns(ptr, len) || len as usize > ipc::CAPACITY {
                error!(
                    "HARLAN: syscall send({to}, {ptr:#x}, {len}) is not a message this process can send"
                );
                frame.rax = ERR_BAD_ARGUMENT as u64;
                return;
            }
            // SAFETY: the range is inside the pages this process was
            // given, mapped in the space that is active — which is why
            // the copy happens here, while the sender is the one running
            // (ADR 0019, point 2).
            let message = unsafe {
                core::slice::from_raw_parts(VirtAddr::new(ptr).as_ptr::<u8>(), len as usize)
            };
            // SAFETY: as above, and the scheduler is this kernel's, on
            // one core, with interrupts off.
            match unsafe { scheduler::deliver(to as usize, running.slot, message) } {
                Ok(delivery) => {
                    info!(
                        "HARLAN: {} byte(s) from slot {} are waiting in the mailbox of slot {to}",
                        delivery.len, running.slot
                    );
                    frame.rax = len;
                }
                Err(SendError::Busy) => {
                    // Said out loud: without it, a sender that is waiting
                    // its turn to try again looks like a sender doing
                    // nothing at all.
                    info!(
                        "HARLAN: the mailbox of slot {to} still holds a message, so slot {} was told to wait",
                        running.slot
                    );
                    frame.rax = ERR_MAILBOX_FULL as u64;
                }
                Err(SendError::NoSuchProcess) => {
                    error!("HARLAN: syscall send() names slot {to}, where there is no process");
                    frame.rax = ERR_NO_SUCH_PROCESS as u64;
                }
                Err(SendError::Rejected(why)) => {
                    error!("HARLAN: syscall send() was refused ({why:?})");
                    frame.rax = ERR_BAD_ARGUMENT as u64;
                }
            }
        }
        Some(Call::Recv) => {
            let (ptr, capacity) = (frame.rdi, frame.rsi);
            if !running.owns(ptr, capacity) {
                error!("HARLAN: syscall recv({ptr:#x}, {capacity}) is not this process's memory");
                frame.rax = ERR_BAD_ARGUMENT as u64;
                return;
            }
            loop {
                // Asked for again each time round: waiting gives the CPU
                // away, and what was true before is not what is true now.
                // SAFETY: the range was just checked to be inside the
                // pages this process was given, which are mapped writable
                // in the space that is active. The kernel writes into
                // user memory here, while its owner is the one running.
                let into = unsafe {
                    core::slice::from_raw_parts_mut(
                        VirtAddr::new(ptr).as_ptr::<u8>(),
                        capacity as usize,
                    )
                };
                // SAFETY: the scheduler is this kernel's, on one core,
                // with interrupts off, and this process is running.
                match unsafe { scheduler::take_message(into) } {
                    Ok(delivery) => {
                        info!(
                            "HARLAN: slot {} took {} byte(s) sent by slot {}",
                            running.slot, delivery.len, delivery.from
                        );
                        frame.rax = delivery.len as u64;
                        // The sender's slot, so that a message is never
                        // one of unknown origin (ADR 0019, point 8).
                        frame.rdx = delivery.from as u64;
                        return;
                    }
                    Err(TakeError::TooLong { len }) => {
                        error!(
                            "HARLAN: syscall recv() offered {capacity} byte(s) for a message of {len}"
                        );
                        frame.rax = ERR_BAD_ARGUMENT as u64;
                        return;
                    }
                    Err(TakeError::Nothing) => {
                        // SAFETY: as above. Comes back once somebody has
                        // written to this process's mailbox.
                        if !unsafe { scheduler::wait_for_message() } {
                            error!(
                                "HARLAN: slot {} is waiting for a message nobody could send",
                                running.slot
                            );
                            frame.rax = ERR_WOULD_WAIT_FOR_EVER as u64;
                            return;
                        }
                    }
                }
            }
        }
        None => {
            error!("HARLAN: unknown syscall {}", frame.rax);
            frame.rax = ERR_UNKNOWN_CALL as u64;
        }
    }
}

/// Runs `process` in ring 3, in its own address space. Never returns: it
/// leaves through `exit`, which hands the CPU to whoever is next.
///
/// # Safety
///
/// `syscall::init` must have run with a stack of the kernel's own, and
/// `process` must be one `spawn` built, in the slot the scheduler is
/// running.
pub unsafe fn enter(process: &'static Process) -> ! {
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
    use harlan_hal::addr::PhysAddr;
    use harlan_hal::frame::PhysRange;

    /// Reads a `disp32` or `imm32` out of the program.
    fn four_bytes_at(program: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([
            program[at],
            program[at + 1],
            program[at + 2],
            program[at + 3],
        ])
    }

    /// The hand-assembled bytes have to mean what the comment says: both
    /// `lea`s have to land on the message, the lengths have to match it,
    /// and the badge has to be where the kernel writes it.
    #[test]
    fn the_talker_points_at_its_own_message() {
        let program = talker_program(b'7');
        // `lea rdi, [rip + disp]`, and rip is past the instruction.
        let target_of =
            |at: usize, end: usize| (end as u32).wrapping_add(four_bytes_at(&program, at));
        assert_eq!(target_of(8, 12), TALK_AT as u32, "the first log");
        assert_eq!(target_of(34, 38), TALK_AT as u32, "the second");
        for at in [13, 39] {
            assert_eq!(four_bytes_at(&program, at) as usize, TALK.len());
        }

        // The three syscalls, in the order the comment claims, and the
        // trap after them.
        assert_eq!(program[1], Call::Log as u8);
        assert_eq!(program[20], Call::Yield as u8);
        assert_eq!(program[27], Call::Log as u8);
        assert_eq!(program[46], Call::Exit as u8);
        for at in [17, 24, 43, 55] {
            assert_eq!(
                (program[at], program[at + 1]),
                (0x0F, 0x05),
                "syscall at {at}"
            );
        }
        assert_eq!((program[57], program[58]), (0x0F, 0x0B), "ud2");

        // The badge is inside the message, and nothing else moved.
        assert_eq!(program[BADGE_AT], b'7');
        let message = core::str::from_utf8(&program[TALK_AT as usize..]).unwrap();
        assert_eq!(
            message,
            "HARLAN: process 7 speaking
"
        );
        assert_ne!(
            talker_program(b'1')[BADGE_AT],
            talker_program(b'2')[BADGE_AT]
        );
    }

    /// The sender's message has to be one a mailbox can hold, its `lea`
    /// has to land on it, and the slot it names has to be the one it was
    /// built for.
    #[test]
    fn the_sender_points_at_a_message_a_mailbox_can_hold() {
        let program = sender_program(2);
        assert!(
            MESSAGE.len() <= ipc::CAPACITY,
            "a message longer than a mailbox would never be sent"
        );
        assert_eq!(program[1], Call::Send as u8);
        assert_eq!(program[6], 2, "the slot it was aimed at");
        assert_eq!(sender_program(5)[6], 5);

        // `lea rsi, [rip + disp]` at 10, four bytes of displacement at 13,
        // rip past the instruction at 17.
        assert_eq!(
            17 + four_bytes_at(&program, 13) as usize,
            MESSAGE_AT as usize
        );
        assert_eq!(four_bytes_at(&program, 18) as usize, MESSAGE.len());
        assert_eq!(
            core::str::from_utf8(&program[MESSAGE_AT as usize..]).unwrap(),
            MESSAGE
        );

        // The retry: what it compares against has to be the error the
        // kernel actually returns for a full mailbox, and the two jumps
        // have to land where the comment says.
        assert_eq!(&program[24..27], &[0x48, 0x83, 0xF8], "cmp rax, imm8");
        assert_eq!(program[27] as i8 as i64, ERR_MAILBOX_FULL);
        assert_eq!(program[28], 0x75, "jne");
        assert_eq!(30 + program[29] as usize, 39, "past the retry, to the exit");
        assert_eq!(program[31], Call::Yield as u8);
        assert_eq!(program[37], 0xEB, "jmp");
        assert_eq!(
            39i32 + program[38] as i8 as i32,
            0,
            "back to the top, to send again"
        );
        assert_eq!(program[40], Call::Exit as u8);
        assert_eq!((program[51], program[52]), (0x0F, 0x0B), "ud2");
    }

    /// The receiver has to ask for the message to be put in memory it
    /// owns and can be written to — its stack, never its read-only code.
    #[test]
    fn the_receiver_asks_for_the_message_in_its_own_stack() {
        let program = receiver_program();
        assert_eq!(program[1], Call::Recv as u8);
        assert_eq!(program[21], Call::Log as u8);
        assert_eq!(&program[32..35], &[0x48, 0x89, 0xD7], "mov rdi, rdx");
        assert_eq!(program[36], Call::Exit as u8);

        let buffer = four_bytes_at(&program, 6);
        assert_eq!(buffer, four_bytes_at(&program, 26), "the same both times");
        assert_eq!(u64::from(buffer), STACK_BASE.as_u64(), "its stack page");
        assert!(
            u64::from(buffer) + ipc::CAPACITY as u64 <= STACK_TOP.as_u64(),
            "and the whole buffer inside it"
        );
        assert_ne!(
            u64::from(buffer),
            PROGRAM_BASE.as_u64(),
            "the code page is read-only: the kernel could not write there"
        );

        // It asks for no more than a mailbox holds, and logs exactly what
        // arrived rather than the whole buffer.
        assert_eq!(four_bytes_at(&program, 11) as usize, ipc::CAPACITY);
        assert_eq!(&program[17..20], &[0x48, 0x89, 0xC6], "mov rsi, rax");
        for at in [15, 30, 40] {
            assert_eq!(
                (program[at], program[at + 1]),
                (0x0F, 0x05),
                "syscall at {at}"
            );
        }
        assert_eq!((program[42], program[43]), (0x0F, 0x0B), "ud2");
    }

    fn frame_for(call: Call, rdi: u64, rsi: u64, rdx: u64) -> SyscallFrame {
        SyscallFrame {
            rax: call as u64,
            rdi,
            rsi,
            rdx,
            r10: 0,
            r8: 0,
            r9: 0,
            user_rip: 0,
            user_rflags: 0,
        }
    }

    /// A process whose memory is this test's own, so that a pointer the
    /// kernel accepts can actually be read on the host.
    fn running_on(buffer: &mut [u8]) -> Running {
        Running {
            slot: 0,
            ranges: [
                PhysRange::new(PhysAddr::new(buffer.as_ptr() as u64), buffer.len() as u64),
                PhysRange::new(PhysAddr::new(buffer.as_ptr() as u64), buffer.len() as u64),
            ],
        }
    }

    /// The programs carry addresses; the mapping that makes those
    /// addresses real is decided in `process`. If the two ever disagreed,
    /// the receiver would ask the kernel to write where it has nothing.
    #[test]
    fn the_programs_and_the_mapping_agree_on_where_things_are() {
        assert_eq!(PROGRAM_BASE, crate::process::CODE_BASE);
        assert_eq!(STACK_BASE, crate::process::STACK_BASE);
        assert_eq!(STACK_TOP, crate::process::STACK_TOP);
        assert_eq!(u64::from(RECEIVE_BUFFER), STACK_BASE.as_u64());
    }

    /// With no process running, a syscall is refused rather than
    /// answered with something that reads as success.
    #[test]
    fn a_syscall_with_no_process_running_is_refused() {
        // The scheduler has handed the CPU to nobody, which is the state
        // a host test finds it in.
        let mut frame = frame_for(Call::Log, 0x1000, 8, 0);
        handle(&mut frame);
        assert_eq!(frame.rax as i64, ERR_UNKNOWN_CALL);
    }

    #[test]
    fn only_the_calls_of_this_abi_exist() {
        assert_eq!(Call::from(0), Some(Call::Log));
        assert_eq!(Call::from(1), Some(Call::Exit));
        assert_eq!(Call::from(2), Some(Call::Yield));
        assert_eq!(Call::from(3), Some(Call::Send));
        assert_eq!(Call::from(4), Some(Call::Recv));
        assert_eq!(Call::from(5), None);
        assert_eq!(Call::from(u64::MAX), None);
    }

    /// Every pointer that arrives from ring 3 is refused before a byte of
    /// it is read. The addresses here are not this test's memory, so a
    /// check that let them through would crash the test rather than pass
    /// it.
    #[test]
    fn a_pointer_that_is_not_the_process_s_own_is_refused_unread() {
        let mut buffer = *b"a message that is this test's own memory";
        let running = running_on(&mut buffer);
        let elsewhere = 0x0040_0000;

        for call in [Call::Log, Call::Recv] {
            let mut frame = frame_for(call, elsewhere, 8, 0);
            serve(&mut frame, running);
            assert_eq!(frame.rax as i64, ERR_BAD_ARGUMENT, "{call:?}");
        }
        let mut frame = frame_for(Call::Send, 1, elsewhere, 8);
        serve(&mut frame, running);
        assert_eq!(frame.rax as i64, ERR_BAD_ARGUMENT, "send");
    }

    /// A message longer than a mailbox is refused before anything is
    /// copied, and so is one of nothing at all.
    #[test]
    fn a_message_a_mailbox_could_not_hold_is_refused() {
        let mut buffer = [7u8; ipc::CAPACITY * 2];
        let running = running_on(&mut buffer);
        let mine = buffer.as_ptr() as u64;

        let mut frame = frame_for(Call::Send, 1, mine, ipc::CAPACITY as u64 + 1);
        serve(&mut frame, running);
        assert_eq!(frame.rax as i64, ERR_BAD_ARGUMENT, "longer than a mailbox");

        // A length of zero is not memory this process owns either, which
        // is the same refusal by a different route.
        let mut frame = frame_for(Call::Send, 1, mine, 0);
        serve(&mut frame, running);
        assert_eq!(frame.rax as i64, ERR_BAD_ARGUMENT, "nothing at all");

        // Exactly a mailbox's worth is not too long: it gets as far as
        // the slot, and is refused for the slot's sake instead.
        let mut frame = frame_for(Call::Send, 1, mine, ipc::CAPACITY as u64);
        serve(&mut frame, running);
        assert_eq!(
            frame.rax as i64, ERR_NO_SUCH_PROCESS,
            "exactly full is a message, and there is nobody in slot 1"
        );
    }

    /// Sending to a slot where there is no process is an error, not a
    /// message that goes nowhere quietly. With an empty scheduler, every
    /// slot is such a slot.
    #[test]
    fn sending_to_nobody_says_so() {
        let mut buffer = *b"a short message";
        let running = running_on(&mut buffer);
        let mine = buffer.as_ptr() as u64;

        for to in [0, 1, MAX_SLOT_TRIED] {
            let mut frame = frame_for(Call::Send, to, mine, buffer.len() as u64);
            serve(&mut frame, running);
            assert_eq!(frame.rax as i64, ERR_NO_SUCH_PROCESS, "slot {to}");
        }
    }

    /// Well past the last slot, to check that a number from ring 3 cannot
    /// reach past the array.
    const MAX_SLOT_TRIED: u64 = u64::MAX;

    /// Waiting for a message nobody could ever send is an error the
    /// process is told about, not a machine that stops.
    #[test]
    fn waiting_for_a_message_nobody_could_send_is_an_error() {
        let mut buffer = [0u8; ipc::CAPACITY];
        let running = running_on(&mut buffer);
        let mut frame = frame_for(Call::Recv, buffer.as_ptr() as u64, ipc::CAPACITY as u64, 0);
        serve(&mut frame, running);
        assert_eq!(frame.rax as i64, ERR_WOULD_WAIT_FOR_EVER);
    }
}
