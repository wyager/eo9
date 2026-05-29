//! The IDT, the interrupt/exception entry stubs, and the trap dispatcher.
//!
//! Interrupts (the PIT wake timer on IRQ 0 and the COM1 UART on IRQ 4, both routed through
//! the remapped 8259 PIC as vectors 0x20 and 0x24) are serviced; every CPU exception is a
//! kernel bug (wasm traps are explicit checks in generated code, not CPU exceptions), so the
//! handler dumps the vector, error code and instruction pointer over serial and parks so the
//! output can be read.
//!
//! Entry stubs are generated 16 bytes apart from a single base label; each pushes a dummy
//! error code when the CPU did not push one, then the vector number, and falls into a common
//! path that saves the caller-saved registers and calls [`ktrap`]. The kernel is built
//! soft-float without SSE (the `x86_64-unknown-none` target), so only the integer
//! caller-saved registers need saving here; when Cranelift-generated wasm code (which does
//! use SSE) arrives with the codegen milestone, the stubs grow the XMM save area too.

use core::sync::atomic::{AtomicU64, Ordering};

use super::pic;

core::arch::global_asm!(
    r#"
// One stub per vector, 16 bytes apart so the IDT builder can compute their addresses from
// the single `vector_stubs` base. Vectors 8, 10-14, 17, 21, 29 and 30 get a CPU-pushed
// error code; every other stub pushes a dummy 0 so the frame layout is uniform.
.macro vecstub vecnum, haserr
.balign 16
    .if \haserr == 0
    push 0
    .endif
    push \vecnum
    jmp trap_common
.endm

.section .text.vectors, "ax"
.balign 16
.globl vector_stubs
vector_stubs:
    vecstub 0, 0
    vecstub 1, 0
    vecstub 2, 0
    vecstub 3, 0
    vecstub 4, 0
    vecstub 5, 0
    vecstub 6, 0
    vecstub 7, 0
    vecstub 8, 1
    vecstub 9, 0
    vecstub 10, 1
    vecstub 11, 1
    vecstub 12, 1
    vecstub 13, 1
    vecstub 14, 1
    vecstub 15, 0
    vecstub 16, 0
    vecstub 17, 1
    vecstub 18, 0
    vecstub 19, 0
    vecstub 20, 0
    vecstub 21, 1
    vecstub 22, 0
    vecstub 23, 0
    vecstub 24, 0
    vecstub 25, 0
    vecstub 26, 0
    vecstub 27, 0
    vecstub 28, 0
    vecstub 29, 1
    vecstub 30, 1
    vecstub 31, 0
    vecstub 32, 0
    vecstub 33, 0
    vecstub 34, 0
    vecstub 35, 0
    vecstub 36, 0
    vecstub 37, 0
    vecstub 38, 0
    vecstub 39, 0
    vecstub 40, 0
    vecstub 41, 0
    vecstub 42, 0
    vecstub 43, 0
    vecstub 44, 0
    vecstub 45, 0
    vecstub 46, 0
    vecstub 47, 0

// Common trap path: save the integer caller-saved registers, hand (vector, error code,
// interrupted RIP) to the Rust dispatcher, restore, drop the vector/error slots, return.
// Stack alignment: the CPU frame plus the two pushed slots plus these nine registers leave
// RSP 16-byte aligned at the call, as the SysV ABI requires.
trap_common:
    push rax
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    mov rdi, [rsp + 72]
    mov rsi, [rsp + 80]
    mov rdx, [rsp + 88]
    call ktrap
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    add rsp, 16
    iretq
"#
);

unsafe extern "C" {
    /// Base of the 16-byte-strided entry stubs above.
    static vector_stubs: u8;
}

/// Number of vectors the kernel installs handlers for: the 32 CPU exceptions plus the 16
/// remapped PIC lines.
const VECTOR_COUNT: usize = 48;

/// One 16-byte long-mode IDT gate descriptor.
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        reserved: 0,
    };

    /// A present interrupt gate in the boot code segment (selector 0x08) for `handler`.
    /// Interrupt gates clear IF on entry, so the dispatcher never nests.
    fn interrupt_gate(handler: usize) -> Self {
        Self {
            offset_low: handler as u16,
            selector: 0x08,
            ist: 0,
            type_attr: 0x8E,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

/// The IDT itself; built once by [`init`] and never touched again.
static mut IDT: [IdtEntry; VECTOR_COUNT] = [IdtEntry::EMPTY; VECTOR_COUNT];

/// The 10-byte operand of `lidt`.
#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u64,
}

/// Names for the architectural exception vectors, indexed by vector number.
const EXCEPTION_NAMES: [&str; 32] = [
    "divide error",
    "debug",
    "non-maskable interrupt",
    "breakpoint",
    "overflow",
    "bound range exceeded",
    "invalid opcode",
    "device not available",
    "double fault",
    "coprocessor segment overrun",
    "invalid TSS",
    "segment not present",
    "stack-segment fault",
    "general protection fault",
    "page fault",
    "reserved (15)",
    "x87 floating-point error",
    "alignment check",
    "machine check",
    "SIMD floating-point error",
    "virtualization error",
    "control protection error",
    "reserved (22)",
    "reserved (23)",
    "reserved (24)",
    "reserved (25)",
    "reserved (26)",
    "reserved (27)",
    "hypervisor injection error",
    "VMM communication error",
    "security error",
    "reserved (31)",
];

/// Count of wake-timer interrupts taken; lets boot verify end-to-end delivery
/// (`super::interrupts_init`) without an executor running.
static TIMER_IRQS: AtomicU64 = AtomicU64::new(0);

/// How many wake-timer interrupts have been taken so far.
pub(super) fn timer_irq_count() -> u64 {
    TIMER_IRQS.load(Ordering::Acquire)
}

/// Build the IDT (one interrupt gate per stub) and load it. Call once during boot, before
/// any line is unmasked.
pub(super) fn init() {
    let base = (&raw const vector_stubs).addr();
    let idt = &raw mut IDT;
    // SAFETY: built once, on the single boot CPU, before interrupts are enabled; each gate
    // points at the matching 16-byte-strided stub.
    unsafe {
        let mut vector = 0;
        while vector < VECTOR_COUNT {
            (*idt)[vector] = IdtEntry::interrupt_gate(base + vector * 16);
            vector += 1;
        }
        let descriptor = IdtDescriptor {
            limit: (core::mem::size_of::<[IdtEntry; VECTOR_COUNT]>() - 1) as u16,
            base: idt as u64,
        };
        core::arch::asm!("lidt [{}]", in(reg) &descriptor, options(nostack, preserves_flags));
    }
}

/// The faulting linear address of the most recent page fault.
fn cr2() -> u64 {
    let value: u64;
    // SAFETY: reading CR2 has no side effects.
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Trap dispatcher, called from `trap_common` with the caller-saved registers preserved.
/// Hardware interrupts are serviced and acknowledged; CPU exceptions are fatal.
#[unsafe(no_mangle)]
extern "C" fn ktrap(vector: u64, error: u64, rip: u64) {
    if vector >= u64::from(pic::VECTOR_BASE) && vector < u64::from(pic::VECTOR_BASE) + 16 {
        let irq = (vector - u64::from(pic::VECTOR_BASE)) as u8;
        // The 8259 raises IRQ 7 / IRQ 15 as its "spurious interrupt" artifact; one that is
        // not actually in service must not be acknowledged (except the cascade line for a
        // spurious slave interrupt).
        if (irq == 7 || irq == 15) && !pic::in_service(irq) {
            if irq == 15 {
                pic::end_of_interrupt(2);
            }
            return;
        }
        match irq {
            // Wake timer: quiet it (mask until re-armed) so a stale one-shot cannot fire
            // again; the executor re-arms before its next halt (mirrors the other ports).
            pic::IRQ_TIMER => {
                super::timer::disable();
                TIMER_IRQS.fetch_add(1, Ordering::Release);
            }
            // COM1 receive: drain every waiting byte into the input ring, which both
            // deasserts the UART's line and captures the keystroke that woke the CPU.
            pic::IRQ_COM1 => super::uart::drain_rx(),
            // Anything else is unexpected but harmless: it is masked in the PIC, so simply
            // acknowledge and ignore it — matching the other ports' treatment of unexpected
            // interrupt IDs.
            _ => {}
        }
        pic::end_of_interrupt(irq);
        return;
    }

    let name = EXCEPTION_NAMES
        .get(vector as usize)
        .copied()
        .unwrap_or("unknown vector");
    crate::kprintln!();
    crate::kprintln!("FATAL EXCEPTION: {name} (vector {vector})");
    crate::kprintln!("  error = {error:#x}");
    crate::kprintln!("  rip   = {rip:#018x}");
    if vector == 14 {
        crate::kprintln!("  cr2   = {:#018x}", cr2());
    }
    crate::kprintln!("parked; exit QEMU with Ctrl-A then X");
    super::power::park()
}
