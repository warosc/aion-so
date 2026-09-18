//! Naked-function exception handlers. Rust's `extern "x86-interrupt"` ABI is
//! still nightly-only (`rust-toolchain.toml` pins stable), so handlers are
//! hand-written trampolines using `#[unsafe(naked)]` + `core::arch::naked_asm!`
//! (stable since ~1.88) instead.
//!
//! Scope for Incremento 1: explicit exception handlers exist only for
//! `#DE`(0), `NMI`(2), `#BP`(3), `#DF`(8), `#GP`(13), `#PF`(14). Every
//! vector in 32-255 (the hardware-interrupt range) shares one silent
//! catch-all, `spurious_interrupt_stub`, installed for all of them. This
//! is load-bearing, not defensive-only padding: empirically, on real QEMU
//! and OVMF, UEFI's own periodic timer interrupt (observed at vector 0x20)
//! can still be "in flight" at the exact instant `cli` executes — `cli`
//! blocks the *next* admission check, it does not retroactively cancel an
//! interrupt whose delivery the CPU already committed to a cycle earlier.
//! Confirmed via QEMU's own `-d int` trace during bring-up: this raced
//! past `cli` and hit an absent gate (present=0, so a fault rather than a
//! handled interrupt), which cascaded into `#GP`. Vectors 0-31 outside the
//! six explicitly handled above stay absent: nothing here executes a
//! software `int n` targeting them, and unlike 32-255 they are not raced
//! by any asynchronous source. If one is ever hit by a real bug, it
//! cascades the same way (`#GP`/`#NP`, ultimately the IST-backed `#DF`
//! handler below) — a deliberate, documented safety net.

use core::mem::size_of;

use crate::gdt::{self, DOUBLE_FAULT_IST_INDEX};
use crate::idt::{GATE_TYPE_INTERRUPT, Idt, IdtEntry};
use aion_hal::{CpuControl, InterruptControl};

const VECTOR_DIVIDE_ERROR: u8 = 0;
const VECTOR_NMI: u8 = 2;
const VECTOR_BREAKPOINT: u8 = 3;
const VECTOR_DOUBLE_FAULT: u8 = 8;
const VECTOR_GENERAL_PROTECTION: u8 = 13;
const VECTOR_PAGE_FAULT: u8 = 14;

static mut IDT: Idt = Idt::new();

/// Registers saved by `common_trampoline`, in push order (last pushed is
/// lowest address, so `r15` sits at the base of this struct). Followed by
/// `vector`/`error_code` (pushed by the per-vector stub) and the
/// hardware-pushed frame. This layout is only valid for ring0-to-ring0
/// delivery on a gate with IST=0: the CPU does not push SS/RSP in that
/// case (Intel SDM Vol. 3 §6.12.1), so this struct deliberately has no
/// `rsp`/`ss` fields. `#DF` uses a separate, dedicated stub specifically
/// because it uses IST1 (a forced stack switch), which *does* push
/// SS/RSP — mixing the two layouts in one struct would be a real bug, not
/// just untidy, so they are kept fully separate instead.
#[repr(C)]
struct InterruptStackFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    /// The 128-byte red-zone reservation `common_trampoline` makes (`sub
    /// rsp, 128`) before its first `push`, so it doesn't write into the
    /// interrupted code's own red zone. This field's only job is to make
    /// that gap visible in the struct layout: without it, every field
    /// below would silently be read from the wrong offset.
    _red_zone_guard: [u64; 16],
    vector: u64,
    error_code: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
}

const _R15_AT_OFFSET_0: () = assert!(core::mem::offset_of!(InterruptStackFrame, r15) == 0);
const _RAX_AT_OFFSET_112: () = assert!(core::mem::offset_of!(InterruptStackFrame, rax) == 112);
const _VECTOR_AT_OFFSET_248: () =
    assert!(core::mem::offset_of!(InterruptStackFrame, vector) == 248);
const _ERROR_CODE_AT_OFFSET_256: () =
    assert!(core::mem::offset_of!(InterruptStackFrame, error_code) == 256);
const _RIP_AT_OFFSET_264: () = assert!(core::mem::offset_of!(InterruptStackFrame, rip) == 264);

macro_rules! stub_no_error_code {
    ($name:ident, $vector:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "push 0",
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym common_trampoline,
            )
        }
    };
}

macro_rules! stub_with_error_code {
    ($name:ident, $vector:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            core::arch::naked_asm!(
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym common_trampoline,
            )
        }
    };
}

stub_no_error_code!(divide_error_stub, VECTOR_DIVIDE_ERROR);
stub_no_error_code!(nmi_stub, VECTOR_NMI);
stub_no_error_code!(breakpoint_stub, VECTOR_BREAKPOINT);

stub_with_error_code!(general_protection_stub, VECTOR_GENERAL_PROTECTION);
stub_with_error_code!(page_fault_stub, VECTOR_PAGE_FAULT);

/// Shared by every vector in 32-255. Deliberately silent (not routed
/// through `common_trampoline`, no logging): see the module-level doc
/// comment for why an interrupt here is an expected, benign race during
/// bring-up, not a bug to report. No EOI is sent — if the source turns
/// out to be 8259-routed, leaving that IRQ line marked "in service" is an
/// acceptable, conservative outcome before Incremento 3 sets up the PIC
/// for real; sending an EOI now, before we know the PIC's actual state,
/// would risk acknowledging something we never validated.
#[unsafe(naked)]
unsafe extern "C" fn spurious_interrupt_stub() {
    core::arch::naked_asm!("iretq")
}

#[unsafe(naked)]
unsafe extern "C" fn common_trampoline() {
    core::arch::naked_asm!(
        // Ring0-to-ring0 delivery with IST=0 reuses the interrupted code's
        // own stack without any hardware-provided gap. If that code was a
        // leaf function, the SysV ABI lets it use the 128-byte "red zone"
        // below its RSP for locals without adjusting RSP at all — exactly
        // the memory our own pushes would land on if we started writing
        // immediately. Reserving (and later releasing) 128 bytes first
        // keeps every push below this trampoline's own stack instead of
        // inside the interrupted code's red zone.
        "sub rsp, 128",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // `rust_interrupt_handler` is an ordinary (non-naked) function
        // compiled for the `x86_64-unknown-uefi` target, which uses the
        // Microsoft x64 calling convention for `extern "C"` (matching what
        // real UEFI firmware expects) — NOT SysV. That means: the first
        // integer argument goes in RCX, not RDI; RSP must be 16-byte
        // aligned at `call`; and the caller must additionally reserve 32
        // bytes of "shadow space" below the return address for the callee
        // to use. `rcx` is safe to clobber for the argument setup even
        // though it holds a saved register's value — its `pop rcx` below
        // restores it from the stack regardless of what this does to the
        // register in between — but the *restore-RSP* value must survive
        // across the whole `call`, so it cannot live in `rax`: RAX is
        // caller-saved (volatile) in both the SysV and Microsoft x64
        // conventions, meaning the callee is free to clobber it, and it
        // did (this was a real, confirmed bug: RSP ended up holding
        // whatever `rust_interrupt_handler` last left in RAX, corrupting
        // the stack on return). RBX is callee-saved in both conventions,
        // so anything the callee does with it, it must undo before
        // returning — safe to rely on across the call.
        "mov rcx, rsp",
        "mov rbx, rsp",
        "and rsp, -16",
        "sub rsp, 32",
        "call {handler}",
        "mov rsp, rbx",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "add rsp, 128", // release the red-zone reservation from function entry
        "add rsp, 16", // discard vector + error_code
        "iretq",
        handler = sym rust_interrupt_handler,
    )
}

extern "C" fn rust_interrupt_handler(frame: *mut InterruptStackFrame) {
    // SAFETY: `frame` points to the register block `common_trampoline` just
    // built on the current stack, immediately before this call. Nothing
    // else can be touching it: this is a single-core kernel and interrupts
    // stay disabled for the duration of this handler (interrupt gates
    // clear IF on entry; nothing here re-enables it).
    let frame = unsafe { &*frame };
    match frame.vector as u8 {
        VECTOR_DIVIDE_ERROR => {
            log::error!("AION: #DE divide error at rip={:#x}", frame.rip);
            halt();
        }
        VECTOR_NMI => {
            // Non-fatal: NMIs are not masked by `cli`, so one could in
            // principle arrive even during Incremento 1's cli-before-lidt
            // window. Nothing in this codebase intentionally raises one;
            // logging and returning is the safe default.
            log::warn!("AION: NMI received (non-fatal)");
        }
        VECTOR_BREAKPOINT => {
            log::info!("AION: breakpoint handler OK");
        }
        VECTOR_GENERAL_PROTECTION => {
            log::error!(
                "AION: #GP error_code={:#x} at rip={:#x}",
                frame.error_code,
                frame.rip
            );
            halt();
        }
        VECTOR_PAGE_FAULT => {
            let faulting_address = read_cr2();
            log::error!(
                "AION: #PF accessing {:#x}, error_code={:#x}, rip={:#x}",
                faulting_address,
                frame.error_code,
                frame.rip
            );
            halt();
        }
        other => {
            log::error!(
                "AION: unhandled exception vector={other} at rip={:#x}",
                frame.rip
            );
            halt();
        }
    }
}

fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: `mov` from CR2 is a plain register read with no side effects;
    // read as the very first thing after entering the #PF path (before any
    // further memory access that could itself fault and, per the Intel
    // SDM, clobber CR2 with a *different* faulting address).
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn halt() -> ! {
    crate::Cpu.halt_loop()
}

/// The double fault handler runs on its own dedicated IST1 stack (wired up
/// by `gdt::init`), so it is reachable even if the stack that faulted is
/// itself corrupted or exhausted. It is treated as unconditionally fatal:
/// no attempt is made to preserve registers or resume, matching standard
/// kernel practice (a double fault means something has already gone badly
/// wrong; ROADMAP's "sin panic inesperado" criterion is about avoiding
/// silent corruption, not about recovering from this).
#[unsafe(naked)]
unsafe extern "C" fn double_fault_stub() -> ! {
    core::arch::naked_asm!(
        // IST1's top is 16-byte aligned by construction (`gdt::init`) and
        // the hardware push for #DF (with its always-0 error code) is 48
        // bytes, a multiple of 16, so RSP is still 16-byte aligned here —
        // no explicit alignment needed. The Microsoft x64 ABI's mandatory
        // 32-byte shadow space is not free, though: reserve it before the
        // call, same as `common_trampoline` does.
        "sub rsp, 32",
        "call {handler}",
        "2:",
        "hlt",
        "jmp 2b",
        handler = sym double_fault_handler,
    )
}

extern "C" fn double_fault_handler() {
    log::error!("AION: #DF DOUBLE FAULT - halting");
}

/// # Safety
///
/// Must run before interrupts are ever enabled on this core, and after
/// `gdt::init` (the `#DF` gate's IST index depends on the TSS already
/// being loaded). Not safe to call from more than one core — single-core
/// kernel in Fase 2.
pub unsafe fn init() {
    let cpu = crate::Cpu;
    // Disable interrupts before touching the IDT at all: UEFI likely left
    // IF set, and until the PIC is remapped (Incremento 3) any hardware
    // IRQ delivered through our not-yet-fully-populated IDT could land on
    // an absent gate or, worse, alias onto a CPU-exception vector (the
    // classic legacy 8259 default mapping overlaps 0x08-0x0F with
    // reserved/exception vectors). Software-triggered exceptions (`int3`,
    // a real `#DE`/`#GP`) are unaffected by IF, so this does not prevent
    // this increment's own self-test below.
    cpu.disable();

    // SAFETY: called exactly once, before interrupts are enabled, per this
    // function's own contract.
    unsafe {
        gdt::init();

        // Install the catch-all first, so every slot has a valid, present
        // gate before anything else can possibly fire — including the
        // already-in-flight race described in the module doc comment.
        for vector in 32..=255usize {
            IDT.0[vector] = IdtEntry::new(
                spurious_interrupt_stub as *const () as u64,
                gdt::KERNEL_CODE_SELECTOR,
                0,
                GATE_TYPE_INTERRUPT,
            );
        }

        IDT.0[VECTOR_DIVIDE_ERROR as usize] = IdtEntry::new(
            divide_error_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
        IDT.0[VECTOR_NMI as usize] = IdtEntry::new(
            nmi_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
        IDT.0[VECTOR_BREAKPOINT as usize] = IdtEntry::new(
            breakpoint_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
        IDT.0[VECTOR_GENERAL_PROTECTION as usize] = IdtEntry::new(
            general_protection_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
        IDT.0[VECTOR_PAGE_FAULT as usize] = IdtEntry::new(
            page_fault_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
        IDT.0[VECTOR_DOUBLE_FAULT as usize] = IdtEntry::new(
            double_fault_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            DOUBLE_FAULT_IST_INDEX,
            GATE_TYPE_INTERRUPT,
        );

        #[repr(C, packed)]
        struct DescriptorTablePointer {
            limit: u16,
            base: u64,
        }
        let idt_ptr = DescriptorTablePointer {
            limit: (size_of::<Idt>() - 1) as u16,
            base: core::ptr::addr_of!(IDT) as u64,
        };
        // SAFETY: `idt_ptr` points at a `'static` IDT fully populated
        // above; `lidt` only loads the IDTR, it cannot itself fault.
        core::arch::asm!("lidt [{}]", in(reg) &idt_ptr, options(nostack));
    }

    // Self-test: prove the IDT is actually wired up before relying on it
    // for anything else. `int3` is a trap (not a fault): RIP saved on the
    // stack already points past this instruction, so execution resumes
    // normally right after it with no adjustment needed.
    unsafe {
        core::arch::asm!("int3", options(nostack));
    }
}
