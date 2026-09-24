//! Naked-function exception handlers. Rust's `extern "x86-interrupt"` ABI is
//! still nightly-only (`rust-toolchain.toml` pins stable), so handlers are
//! hand-written trampolines using `#[unsafe(naked)]` + `core::arch::naked_asm!`
//! (stable since ~1.88) instead.
//!
//! Scope for Incremento 1: explicit exception handlers exist only for
//! `#DE`(0), `NMI`(2), `#BP`(3), `#DF`(8), `#GP`(13), `#PF`(14). Every
//! vector in 32-255 (the hardware-interrupt range) shares one silent
//! catch-all, `spurious_interrupt_stub`, installed for all of them; the
//! timer (0x20, Incremento 3) and keyboard (0x21, Incremento 4) later
//! overwrite their own slots with real handlers. This
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
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::gdt::{self, DOUBLE_FAULT_IST_INDEX};
use crate::idt::{GATE_TYPE_INTERRUPT, Idt, IdtEntry};
use harlan_hal::{CpuControl, InterruptControl};
use harlan_hal::{error, info, warn};

/// Written only by the timer ISR (`VECTOR_TIMER` below); read by
/// `ticks()`, `hal::TickCounter`'s sole consumer today.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// What the timer calls after counting, if anything. A function pointer,
/// so that the kernel can set it — and set it again after moving its
/// image (docs/adr/0018-fase3-context-switch.md).
static TICK_HANDLER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Calls `handler` on every tick, from inside the interrupt handler, with
/// interrupts off.
pub fn set_tick_handler(handler: unsafe fn()) {
    TICK_HANDLER.store(handler as usize, Ordering::Release);
}

/// Current tick count, as observed so far. `Relaxed` is sufficient: this
/// is a monotonic counter with no other data it needs to synchronize
/// with, single-writer (the ISR), any-reader.
pub fn ticks() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

const VECTOR_DIVIDE_ERROR: u8 = 0;
const VECTOR_NMI: u8 = 2;
const VECTOR_BREAKPOINT: u8 = 3;
const VECTOR_DOUBLE_FAULT: u8 = 8;
const VECTOR_GENERAL_PROTECTION: u8 = 13;
const VECTOR_PAGE_FAULT: u8 = 14;
/// PIT timer, IRQ0 remapped here by `pic::remap` (Incremento 3). The same
/// numeric value UEFI's own timer happened to use pre-remap (see the
/// module doc comment) — coincidental, not a conflict: the PIC is masked
/// throughout the gap between UEFI's config and ours, and `pic::remap`
/// itself reprograms the mapping before this vector is ever unmasked.
const VECTOR_TIMER: u8 = 0x20;
/// PS/2 keyboard, IRQ1 remapped here by `pic::remap` (Incremento 4).
const VECTOR_KEYBOARD: u8 = 0x21;

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
stub_no_error_code!(timer_stub, VECTOR_TIMER);
stub_no_error_code!(keyboard_stub, VECTOR_KEYBOARD);

stub_with_error_code!(general_protection_stub, VECTOR_GENERAL_PROTECTION);
stub_with_error_code!(page_fault_stub, VECTOR_PAGE_FAULT);

/// Every other exception the CPU can raise. Until ring 3 existed, a
/// vector with no gate meant a bug in the kernel and the `#GP` it turned
/// into was as good an answer as any. A process can raise most of these
/// on purpose —`ud2` is two bytes— and an exception with no gate becomes
/// a `#GP` whose error code names a segment selector that has nothing to
/// do with anything. So they all get a gate, and the dispatcher says
/// which one it was.
///
/// Which ones the CPU pushes an error code for is not a choice: Intel SDM
/// Vol. 3 §6.3.1. Getting it wrong shifts the whole frame.
macro_rules! exception_stubs {
    ($(($name:ident, $vector:expr, $kind:ident, $with_error_code:expr)),* $(,)?) => {
        $( $kind!($name, $vector); )*
        /// The gates `install_gates` adds on top of the named ones, each
        /// with what its stub assumed about the error code. A test
        /// compares that against `pushes_error_code`, which says the same
        /// thing in another shape: the two are written apart on purpose,
        /// because agreeing by accident is not agreement.
        const OTHER_EXCEPTION_GATES: &[(u8, unsafe extern "C" fn(), bool)] =
            &[$(($vector, $name, $with_error_code)),*];
    };
}

exception_stubs![
    (debug_stub, 1, stub_no_error_code, false),
    (overflow_stub, 4, stub_no_error_code, false),
    (bound_range_stub, 5, stub_no_error_code, false),
    (invalid_opcode_stub, 6, stub_no_error_code, false),
    (device_not_available_stub, 7, stub_no_error_code, false),
    (coprocessor_overrun_stub, 9, stub_no_error_code, false),
    (invalid_tss_stub, 10, stub_with_error_code, true),
    (segment_not_present_stub, 11, stub_with_error_code, true),
    (stack_segment_stub, 12, stub_with_error_code, true),
    (reserved_15_stub, 15, stub_no_error_code, false),
    (x87_stub, 16, stub_no_error_code, false),
    (alignment_check_stub, 17, stub_with_error_code, true),
    (machine_check_stub, 18, stub_no_error_code, false),
    (simd_stub, 19, stub_no_error_code, false),
    (virtualisation_stub, 20, stub_no_error_code, false),
    (control_protection_stub, 21, stub_with_error_code, true),
    (reserved_22_stub, 22, stub_no_error_code, false),
    (reserved_23_stub, 23, stub_no_error_code, false),
    (reserved_24_stub, 24, stub_no_error_code, false),
    (reserved_25_stub, 25, stub_no_error_code, false),
    (reserved_26_stub, 26, stub_no_error_code, false),
    (reserved_27_stub, 27, stub_no_error_code, false),
    (hypervisor_stub, 28, stub_no_error_code, false),
    (vmm_communication_stub, 29, stub_with_error_code, true),
    (security_stub, 30, stub_with_error_code, true),
    (reserved_31_stub, 31, stub_no_error_code, false),
];

/// Whether the CPU pushes an error code for this vector, from Intel SDM
/// Vol. 3 §6.3.1 table 6-1. The stubs have to agree: a stub that expects
/// one where there is none shifts every field of the frame by eight
/// bytes, and what it reads as `rip` is whatever came before.
///
/// It exists to be compared against the stubs, which is a thing only a
/// test does; the running kernel gets the answer from which stub the
/// vector is wired to.
#[cfg(test)]
const fn pushes_error_code(vector: u8) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

/// What to call a vector in the log. Short, because the point of the line
/// is the address and the process, not the vocabulary.
fn exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection",
        14 => "#PF page fault",
        16 => "#MF x87 floating-point",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point",
        20 => "#VE virtualisation",
        21 => "#CP control protection",
        28 => "#HV hypervisor injection",
        29 => "#VC VMM communication",
        30 => "#SX security",
        _ => "reserved exception",
    }
}

/// What the kernel is told about a fault a process caused.
#[derive(Debug, Clone, Copy)]
pub struct UserFault {
    pub vector: u8,
    /// What the vector is called, so that the kernel need not keep its
    /// own copy of the table.
    pub name: &'static str,
    /// What the CPU pushed, when the vector pushes one; zero otherwise.
    pub error_code: u64,
    /// The address `CR2` held, for `#PF`; zero otherwise.
    pub address: u64,
    /// Where in the program it happened.
    pub rip: u64,
}

/// What the kernel does with a fault that came from ring 3. Never returns:
/// the process that caused it does not run again
/// (docs/adr/0020-fase3-a-fault-belongs-to-the-process.md).
static USER_FAULT_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Tells this module what to do when a process faults. Until it is set, a
/// fault in ring 3 stops the machine, like one in the kernel.
pub fn set_user_fault_handler(handler: unsafe fn(UserFault) -> !) {
    USER_FAULT_HANDLER.store(handler as usize, Ordering::Release);
}

/// Whether the interrupt that pushed this `CS` happened in ring 3.
///
/// The low two bits of a pushed code selector are the privilege level the
/// CPU was running at — its RPL. Ring 3 is the only level a process runs
/// in, and the kernel's own selector has an RPL of zero. This is the whole
/// of how the kernel tells "the process did something" from "the kernel
/// did something", so it is a function with a name and a test rather than
/// two characters inside a condition.
fn came_from_ring_3(cs: u64) -> bool {
    cs & 3 == 3
}

/// A fault the CPU cannot carry on from.
///
/// If it happened in ring 3 it belongs to the process, and the kernel is
/// handed it so that it can end that process and give the CPU to somebody
/// else. If it happened in the kernel there is nobody else to blame, and
/// the machine stops where it is rather than carrying on over whatever
/// went wrong.
fn fatal(frame: &InterruptStackFrame, address: u64) -> ! {
    if came_from_ring_3(frame.cs) {
        let handler = USER_FAULT_HANDLER.load(Ordering::Acquire);
        if handler != 0 {
            let vector = frame.vector as u8;
            let fault = UserFault {
                vector,
                name: exception_name(vector),
                error_code: frame.error_code,
                address,
                rip: frame.rip,
            };
            // SAFETY: only `set_user_fault_handler` writes there, and what
            // it writes is an `unsafe fn(UserFault) -> !`. Interrupts are
            // off inside this handler, and the handler is written for
            // exactly that.
            let handler: unsafe fn(UserFault) -> ! = unsafe { core::mem::transmute(handler) };
            // SAFETY: as above; it does not come back.
            unsafe { handler(fault) };
        }
        error!("HARLAN: a process faulted before the kernel could take faults");
    }
    halt()
}

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
            error!("HARLAN: #DE divide error at rip={:#x}", frame.rip);
            fatal(frame, 0);
        }
        VECTOR_NMI => {
            // Non-fatal: NMIs are not masked by `cli`, so one could in
            // principle arrive even during Incremento 1's cli-before-lidt
            // window. Nothing in this codebase intentionally raises one;
            // logging and returning is the safe default.
            warn!("HARLAN: NMI received (non-fatal)");
        }
        VECTOR_BREAKPOINT => {
            // The kernel's own self-test breaks on purpose and carries
            // on. A process that does it has no debugger to talk to, so
            // for it a breakpoint is the end.
            if came_from_ring_3(frame.cs) {
                error!("HARLAN: #BP breakpoint from ring 3 at rip={:#x}", frame.rip);
                fatal(frame, 0);
            }
            info!("HARLAN: breakpoint handler OK");
        }
        VECTOR_TIMER => {
            let count = TICK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            // SAFETY: this handler only ever runs as a direct result of a
            // real PIC-routed IRQ0 delivery (that's what vector 0x20 is
            // wired to after `pic::remap`), which is exactly this
            // function's documented precondition.
            unsafe {
                crate::pic::send_eoi();
            }
            if count.is_multiple_of(100) {
                info!("HARLAN: ticks={count}");
            }
            let handler = TICK_HANDLER.load(Ordering::Acquire);
            if handler != 0 {
                // SAFETY: only `set_tick_handler` writes there, and what
                // it writes is an `unsafe fn()`. Interrupts are off
                // inside this handler, which is what the handler is
                // written for.
                let handler: unsafe fn() = unsafe { core::mem::transmute(handler) };
                // SAFETY: as above.
                unsafe { handler() };
            }
        }
        VECTOR_KEYBOARD => {
            // SAFETY: this handler only ever runs as a direct result of a
            // real PIC-routed IRQ1 delivery (vector 0x21 after
            // `pic::remap`), which is exactly what both callees document
            // as their precondition. Not reentrant: the gate cleared IF.
            unsafe {
                crate::keyboard::on_irq();
                crate::pic::send_eoi();
            }
        }
        VECTOR_GENERAL_PROTECTION => {
            error!(
                "HARLAN: #GP error_code={:#x} at rip={:#x}",
                frame.error_code, frame.rip
            );
            fatal(frame, 0);
        }
        VECTOR_PAGE_FAULT => {
            let faulting_address = read_cr2();
            error!(
                "HARLAN: #PF accessing {:#x}, error_code={:#x}, rip={:#x}",
                faulting_address, frame.error_code, frame.rip
            );
            fatal(frame, faulting_address);
        }
        other => {
            error!(
                "HARLAN: {} (vector={other}) error_code={:#x} at rip={:#x}",
                exception_name(other),
                frame.error_code,
                frame.rip
            );
            fatal(frame, 0);
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
    // The address says which stack this ran on: the IST stack the kernel
    // mapped with guard pages, or the static one used before that.
    let here = 0u8;
    error!(
        "HARLAN: #DF DOUBLE FAULT on the stack at {:#x} - halting",
        core::ptr::addr_of!(here) as u64
    );
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
        install_gates();
        load_idt();
    }

    // Self-test: prove the IDT is actually wired up before relying on it
    // for anything else. `int3` is a trap (not a fault): RIP saved on the
    // stack already points past this instruction, so execution resumes
    // normally right after it with no adjustment needed.
    unsafe {
        core::arch::asm!("int3", options(nostack));
    }
}

/// Loads the GDT, TSS and IDT again, reading the addresses of those
/// tables as they are now.
///
/// They hold absolute addresses — the GDTR and IDTR point at the tables,
/// the TSS descriptor carries its base — so a kernel that has moved (see
/// docs/adr/0012-fase3-higher-half-kernel.md) keeps pointing at where it
/// used to be until this runs. Touches no hardware: the PIC, the PIT and
/// the i8042 keep the state they were left in.
///
/// # Safety
///
/// Interrupts must be disabled, the image's relocations must already be
/// applied, and the caller must reinstall the double-fault stack
/// afterwards (`set_double_fault_stack`), because loading the GDT resets
/// IST1 to the one inside the image. Single core.
pub unsafe fn reinstall_descriptors() {
    // SAFETY: same writes as `init`, to the same `'static` tables, with
    // interrupts disabled by this function's own contract.
    unsafe {
        gdt::init();
        install_gates();
        load_idt();
        core::arch::asm!("int3", options(nostack));
    }
}

/// Fills every vector this kernel answers. Separate from `init` so the
/// same set can be installed again after the kernel moves.
///
/// # Safety
///
/// Interrupts must be disabled; single core.
unsafe fn install_gates() {
    // SAFETY: single-threaded, interrupts disabled, writing the `'static`
    // IDT that only this module owns.
    unsafe {
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

        // Everything else the CPU can raise, so that a process cannot
        // reach a vector with no gate.
        let mut index = 0;
        while index < OTHER_EXCEPTION_GATES.len() {
            let (vector, stub, _) = OTHER_EXCEPTION_GATES[index];
            IDT.0[vector as usize] = IdtEntry::new(
                stub as *const () as u64,
                gdt::KERNEL_CODE_SELECTOR,
                0,
                GATE_TYPE_INTERRUPT,
            );
            index += 1;
        }

        IDT.0[VECTOR_TIMER as usize] = IdtEntry::new(
            timer_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
        IDT.0[VECTOR_KEYBOARD as usize] = IdtEntry::new(
            keyboard_stub as *const () as u64,
            gdt::KERNEL_CODE_SELECTOR,
            0,
            GATE_TYPE_INTERRUPT,
        );
    }
}

/// Points the IDTR at the table where it lives now.
///
/// # Safety
///
/// Every gate must be installed first; interrupts disabled.
unsafe fn load_idt() {
    #[repr(C, packed)]
    struct DescriptorTablePointer {
        limit: u16,
        base: u64,
    }
    let idt_ptr = DescriptorTablePointer {
        limit: (size_of::<Idt>() - 1) as u16,
        base: core::ptr::addr_of!(IDT) as u64,
    };
    // SAFETY: `idt_ptr` points at a `'static` IDT fully populated by
    // `install_gates`; `lidt` only loads the IDTR, it cannot itself fault.
    unsafe {
        core::arch::asm!("lidt [{}]", in(reg) &idt_ptr, options(nostack));
    }
}

/// Installs the timer's IDT vector, remaps the PIC (all lines masked),
/// programs the PIT for ~100 Hz and unmasks IRQ0. Does **not** enable
/// interrupts: with the whole set of devices this kernel uses configured
/// first, the caller does that once, explicitly (`InterruptControl::enable`).
///
/// # Safety
///
/// Must run after `init()` (GDT/IDT already installed, including this
/// function's own vector 0x20 write, which happens before the line is
/// unmasked), with interrupts still disabled. Not safe to call more than
/// once or concurrently — single-core kernel in Fase 2.
pub unsafe fn init_timer() {
    // The vector itself was installed by `init()`; what is left is the
    // hardware.
    unsafe {
        // SAFETY: interrupts are still disabled here (nothing between
        // `init()` and this call re-enables them).
        crate::pic::remap();
        // SAFETY: the timer's IDT vector exists and the PIC is
        // remapped to deliver IRQ0 there — the preconditions `pit::init`
        // itself documents.
        crate::pit::init(100);
        // SAFETY: `remap` ran above, the handler for vector 0x20 was
        // installed at the top of this function, and interrupts are
        // still disabled (the read-modify-write precondition).
        crate::pic::unmask(0);
    }
}

/// Installs the keyboard's IDT vector, configures the i8042 and unmasks
/// IRQ1. Like `init_timer`, does not enable interrupts.
///
/// If the controller doesn't respond, returns the error **without**
/// unmasking IRQ1: the gate installed first is then never reachable, and
/// the kernel keeps running (just without input) instead of hanging on it.
///
/// # Safety
///
/// Must run after `init_timer()` (which remaps the PIC) and before
/// interrupts are enabled — the i8042 setup polls the output buffer the
/// IRQ1 handler would otherwise consume. Once only, single-core.
pub unsafe fn init_keyboard() -> Result<(), crate::keyboard::InitError> {
    // The vector was installed by `init()`, before the hardware could
    // possibly raise the line.
    // SAFETY: interrupts are disabled and IRQ1 is still masked (`remap`
    // masked everything and nothing has unmasked line 1 yet), which is
    // exactly `init_controller`'s contract.
    unsafe {
        crate::keyboard::init_controller()?;
    }
    // SAFETY: `remap` ran (in `init_timer`), the vector-0x21 handler is
    // installed, and interrupts are disabled.
    unsafe {
        crate::pic::unmask(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// How the kernel tells a process's fault from its own. Pinned against
    /// the selectors the GDT actually uses, so that moving a descriptor
    /// cannot quietly change the answer.
    #[test]
    fn a_fault_is_the_process_s_only_when_the_cpu_was_in_ring_3() {
        assert!(came_from_ring_3(u64::from(gdt::USER_CODE_SELECTOR)));
        assert!(!came_from_ring_3(u64::from(gdt::KERNEL_CODE_SELECTOR)));
        // Not the descriptor: the privilege bits. The user code descriptor
        // read with an RPL of zero is not ring 3, and never appears.
        assert!(!came_from_ring_3(u64::from(gdt::USER_CODE_SELECTOR & !3)));
        assert!(!came_from_ring_3(0));
        // Ring 1 and 2 exist and nothing in this kernel runs in them.
        assert!(!came_from_ring_3(
            u64::from(gdt::USER_CODE_SELECTOR & !3) | 1
        ));
        assert!(!came_from_ring_3(
            u64::from(gdt::USER_CODE_SELECTOR & !3) | 2
        ));
    }

    /// A vector with no gate is a vector a process can reach, and what it
    /// gets instead is a `#GP` naming a segment selector that has nothing
    /// to do with what happened. Every exception the CPU defines has to
    /// have one.
    #[test]
    fn every_exception_vector_has_a_gate() {
        let named = [
            VECTOR_DIVIDE_ERROR,
            VECTOR_NMI,
            VECTOR_BREAKPOINT,
            VECTOR_DOUBLE_FAULT,
            VECTOR_GENERAL_PROTECTION,
            VECTOR_PAGE_FAULT,
        ];
        let mut covered = [false; 32];
        for vector in named {
            assert!(!covered[vector as usize], "vector {vector} twice");
            covered[vector as usize] = true;
        }
        for (vector, _, _) in OTHER_EXCEPTION_GATES {
            assert!(*vector < 32, "vector {vector} is not an exception");
            assert!(!covered[*vector as usize], "vector {vector} twice");
            covered[*vector as usize] = true;
        }
        for (vector, has_gate) in covered.iter().enumerate() {
            assert!(has_gate, "vector {vector} has no gate");
        }
    }

    /// The other half of that: a stub has to know whether the CPU pushed
    /// an error code, because everything above it in the frame moves by
    /// eight bytes if it is wrong.
    #[test]
    fn the_stubs_agree_with_the_manual_about_error_codes() {
        for (vector, _, with_error_code) in OTHER_EXCEPTION_GATES {
            assert_eq!(
                *with_error_code,
                pushes_error_code(*vector),
                "vector {vector}"
            );
        }
        // And the named ones, which are wired by hand above.
        assert!(!pushes_error_code(VECTOR_DIVIDE_ERROR));
        assert!(!pushes_error_code(VECTOR_NMI));
        assert!(!pushes_error_code(VECTOR_BREAKPOINT));
        assert!(pushes_error_code(VECTOR_DOUBLE_FAULT));
        assert!(pushes_error_code(VECTOR_GENERAL_PROTECTION));
        assert!(pushes_error_code(VECTOR_PAGE_FAULT));
        // Nothing outside the exception range does.
        assert!(!pushes_error_code(VECTOR_TIMER));
        assert!(!pushes_error_code(VECTOR_KEYBOARD));
    }

    /// The log has to name what happened. A vector with no name would
    /// still be handled, but the line it prints would not say what it was.
    #[test]
    fn every_exception_the_cpu_defines_has_a_name() {
        for vector in 0..32u8 {
            let name = exception_name(vector);
            let reserved = matches!(vector, 9 | 15 | 22..=27 | 31);
            assert_eq!(
                name == "reserved exception",
                reserved,
                "vector {vector} is named {name}"
            );
        }
        assert_eq!(exception_name(6), "#UD invalid opcode");
        assert_eq!(exception_name(14), "#PF page fault");
    }
}
