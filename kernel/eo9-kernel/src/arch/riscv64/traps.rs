//! Trap dispatch: interrupts (the supervisor timer and the PLIC's external line) are
//! serviced; every synchronous exception is a kernel bug (wasm traps are explicit checks in
//! generated code, not CPU exceptions), so the handler dumps `scause`/`sepc`/`stval` over
//! serial and parks so the output can be read.

use core::sync::atomic::{AtomicU64, Ordering};

/// Interrupt bit of `scause` (set for interrupts, clear for synchronous exceptions).
const INTERRUPT: u64 = 1 << 63;
/// Supervisor timer interrupt cause.
const IRQ_S_TIMER: u64 = 5;
/// Supervisor external (PLIC) interrupt cause.
const IRQ_S_EXTERNAL: u64 = 9;

/// Names for the standard synchronous exception causes, indexed by `scause`.
const EXCEPTION_NAMES: [&str; 16] = [
    "instruction address misaligned",
    "instruction access fault",
    "illegal instruction",
    "breakpoint",
    "load address misaligned",
    "load access fault",
    "store/AMO address misaligned",
    "store/AMO access fault",
    "environment call from U-mode",
    "environment call from S-mode",
    "reserved (10)",
    "reserved (11)",
    "instruction page fault",
    "load page fault",
    "reserved (14)",
    "store/AMO page fault",
];

/// Count of supervisor-timer interrupts taken; lets boot verify end-to-end delivery
/// (`super::interrupts_init`) without an executor running.
static TIMER_IRQS: AtomicU64 = AtomicU64::new(0);

/// How many supervisor-timer interrupts have been taken so far.
pub(super) fn timer_irq_count() -> u64 {
    TIMER_IRQS.load(Ordering::Acquire)
}

/// Trap dispatcher, called from `__trap_entry` (src/arch/riscv64/boot.rs) with the
/// caller-saved registers already preserved. Interrupts are serviced and return (the timer
/// is quieted via the SBI so its pending bit drops — the executor re-arms it before the next
/// halt; the UART's RX bytes are drained into the input ring through the PLIC claim loop);
/// synchronous exceptions are fatal.
#[unsafe(no_mangle)]
extern "C" fn ktrap(scause: u64, sepc: u64, stval: u64) {
    if scause & INTERRUPT != 0 {
        match scause & !INTERRUPT {
            // Supervisor timer: cancel it so the pending bit drops before `sret`; the
            // executor re-arms it before the next halt (mirrors the aarch64 handler).
            IRQ_S_TIMER => {
                super::timer::disable();
                TIMER_IRQS.fetch_add(1, Ordering::Release);
            }
            // Supervisor external: claim every pending PLIC source; UART0 receive bytes go
            // into the input ring, which both deasserts the UART's line and captures the
            // keystroke that woke the hart.
            IRQ_S_EXTERNAL => loop {
                let source = super::plic::claim();
                if source == 0 {
                    break;
                }
                if source == super::plic::UART0_SOURCE {
                    super::uart::drain_rx();
                }
                super::plic::complete(source);
            },
            // Anything else (e.g. a software interrupt) is unexpected but harmless: it is
            // not enabled in `sie`, so simply ignore it — matching the aarch64 handler's
            // treatment of unexpected INTIDs.
            _ => {}
        }
        return;
    }

    let name = EXCEPTION_NAMES
        .get(scause as usize)
        .copied()
        .unwrap_or("unknown exception");
    crate::kprintln!();
    crate::kprintln!("FATAL EXCEPTION: {name} (scause {scause:#x})");
    crate::kprintln!("  sepc  = {sepc:#018x}");
    crate::kprintln!("  stval = {stval:#018x}");
    crate::kprintln!("parked; exit QEMU with Ctrl-A then X");
    super::power::park()
}
