//! How ring 3 asks the kernel for something.
//!
//! `syscall` is fast because it does almost nothing: it loads `CS`/`SS`
//! from `STAR`, jumps to `LSTAR`, saves the return address in `RCX` and
//! the flags in `R11`, and masks the flags in `FMASK`. It does **not**
//! change the stack pointer. Until the entry stub has swapped in a kernel
//! stack, the kernel is running on memory the user chose, which is why
//! `FMASK` clears `IF`: nothing may be delivered in that window.
//!
//! The stack it swaps in is the **running process's own** (ADR 0018), so
//! the kernel can park a process inside a syscall and come back to it.
//!
//! It used to find that stack through `swapgs` and `KERNEL_GS_BASE`, the
//! way a multi-core kernel has to. That broke the moment processes could
//! be switched: a process can enter the kernel through a syscall — which
//! swaps `GS` — and leave through the timer's `iretq`, which does not
//! swap it back, and then the next `swapgs` leaves `GS` pointing at the
//! user's value and the stub writes through it. With one core the address
//! can simply be read from a static, RIP-relative. Bringing `swapgs` back
//! means teaching the interrupt path to swap too, and that belongs with
//! the rest of the multi-core work.
//!
//! The ABI is in docs/adr/0014-fase3-syscall-abi-v0.md.

use core::arch::naked_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::gdt;
use crate::msr;

const EFER_SCE: u64 = 1 << 0;
/// Cleared on entry: interrupts (no stack yet), the direction flag (so
/// string instructions in the kernel start forwards) and alignment checks.
const FMASK: u64 = (1 << 9) | (1 << 10) | (1 << 18);

/// The top of the kernel stack of whichever process is running. Written
/// by `set_kernel_stack` on every switch, read by the entry stub.
static mut KERNEL_STACK_TOP: u64 = 0;

/// Where the stub leaves the user's stack pointer for the two
/// instructions it takes to get onto the kernel's. Interrupts are off
/// throughout, and there is one core, so nothing can look in between.
static mut USER_STACK_SCRATCH: u64 = 0;

/// The registers a syscall arrives in, as the stub leaves them on the
/// kernel stack. `rax` carries the number in and the result out.
#[repr(C)]
#[derive(Debug)]
pub struct SyscallFrame {
    pub rax: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
    /// Where `sysretq` will return to, and with which flags. The
    /// instruction put them here; nothing else may.
    pub user_rip: u64,
    pub user_rflags: u64,
}

/// What the kernel does with a syscall. A function pointer, so the kernel
/// can set it again after moving its image.
pub type Handler = fn(&mut SyscallFrame);

static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Points the entry stub at the stack of whichever process is about to
/// run. Called on every switch, because that stack is the process's own
/// (docs/adr/0018-fase3-context-switch.md).
///
/// # Safety
///
/// `top` must be the top of a kernel stack that nothing else is using,
/// and no syscall may be in flight.
pub unsafe fn set_kernel_stack(top: u64) {
    // SAFETY: single core, and the caller says no syscall is in flight.
    unsafe { KERNEL_STACK_TOP = top };
}

/// Sends syscalls to `handler`.
pub fn set_handler(handler: Handler) {
    HANDLER.store(handler as usize, Ordering::Release);
}

/// Turns `syscall` on and points it here.
///
/// # Safety
///
/// * `kernel_stack_top` must be the top of a stack of the kernel's own,
///   used by nothing else: the stub switches to it on every syscall and
///   overwrites whatever is below.
/// * The kernel must already be running from wherever it means to stay
///   (this writes the stub's address into an MSR), and a handler must be
///   set before any user code runs.
/// * Single core.
pub unsafe fn init(kernel_stack_top: u64) {
    // SAFETY: single-threaded init, before any user code exists.
    unsafe {
        KERNEL_STACK_TOP = kernel_stack_top;
        USER_STACK_SCRATCH = 0;

        // What `sysretq` will load out of `STAR`, spelled out where it
        // is programmed: this is the only thing tying the GDT's layout to
        // the instruction's arithmetic.
        debug_assert_eq!(
            gdt::USER_DATA_SELECTOR & !3,
            gdt::SYSRET_SELECTOR_BASE + 8,
            "sysretq loads SS from STAR[63:48] + 8"
        );
        debug_assert_eq!(
            gdt::USER_CODE_SELECTOR & !3,
            gdt::SYSRET_SELECTOR_BASE + 16,
            "sysretq loads CS from STAR[63:48] + 16"
        );
        let star = (u64::from(gdt::SYSRET_SELECTOR_BASE) << 48)
            | (u64::from(gdt::KERNEL_CODE_SELECTOR) << 32);
        msr::write(msr::IA32_STAR, star);
        msr::write(msr::IA32_LSTAR, syscall_entry as *const () as u64);
        msr::write(msr::IA32_FMASK, FMASK);
        msr::write(msr::IA32_EFER, msr::read(msr::IA32_EFER) | EFER_SCE);
    }
}

/// Called by the stub with the frame it just built on the kernel stack.
extern "C" fn dispatch(frame: &mut SyscallFrame) {
    let handler = HANDLER.load(Ordering::Acquire);
    if handler == 0 {
        // Nobody is listening yet. Saying so beats returning a number the
        // caller would read as success.
        frame.rax = (-1i64) as u64;
        return;
    }
    // SAFETY: `HANDLER` only ever holds what `set_handler` put there.
    let handler: Handler = unsafe { core::mem::transmute::<usize, Handler>(handler) };
    handler(frame);
}

/// Where `syscall` lands.
///
/// Builds `SyscallFrame` on the kernel's stack, calls `dispatch` with a
/// pointer to it, and returns to ring 3 with `sysretq`. Every push here is
/// one field of that struct, in order, so the two have to change together.
///
/// # Safety
///
/// Only the CPU calls this, through `LSTAR`. `init` must have run.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        // Onto the running process's kernel stack, keeping the user's
        // pointer on that same stack: a global would be overwritten by
        // whoever runs while this process is parked mid-syscall.
        "mov [rip + {scratch}], rsp",
        "mov rsp, [rip + {kernel_top}]",
        "push [rip + {scratch}]",
        // The frame, from the last field to the first.
        "push r11",
        "push rcx",
        "push r9",
        "push r8",
        "push r10",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rax",
        // Microsoft x64: first argument in rcx, 32 bytes of shadow space,
        // and rsp 16-byte aligned before the call. The stack top is page
        // aligned and ten pushes are 80 bytes, so it still is.
        "mov rcx, rsp",
        "sub rsp, 32",
        "call {dispatch}",
        "add rsp, 32",
        // Back the way it came in. `rax` carries the result.
        "pop rax",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop r10",
        "pop r8",
        "pop r9",
        "pop rcx",
        "pop r11",
        "pop rsp",
        "sysretq",
        dispatch = sym dispatch,
        scratch = sym USER_STACK_SCRATCH,
        kernel_top = sym KERNEL_STACK_TOP,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stub pushes registers in an order that has to match the struct
    /// it claims to be building.
    #[test]
    fn the_frame_is_laid_out_the_way_the_stub_pushes_it() {
        assert_eq!(size_of::<SyscallFrame>(), 9 * 8);
        let frame = SyscallFrame {
            rax: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            r10: 0,
            r8: 0,
            r9: 0,
            user_rip: 0,
            user_rflags: 0,
        };
        let base = &frame as *const _ as usize;
        let at = |field: *const u64| field as usize - base;
        assert_eq!(at(&frame.rax), 0);
        assert_eq!(at(&frame.rdi), 8);
        assert_eq!(at(&frame.rsi), 16);
        assert_eq!(at(&frame.rdx), 24);
        assert_eq!(at(&frame.r10), 32);
        assert_eq!(at(&frame.r8), 40);
        assert_eq!(at(&frame.r9), 48);
        assert_eq!(at(&frame.user_rip), 56);
        assert_eq!(at(&frame.user_rflags), 64);
    }

    #[test]
    fn the_masked_flags_are_interrupts_direction_and_alignment_check() {
        assert_eq!(FMASK, 0x4_0600);
    }
}
