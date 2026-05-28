//! IRQ dispatch and fatal exception reporting.
//!
//! Two interrupt sources are taken as IRQs (forwarded by the GIC, src/gic.rs): the EL1
//! generic timer (the executor's `wfi` wake) and the PL011 UART receive line (a keystroke
//! wakes the core and is captured into the input ring). Every *other* exception is a kernel
//! bug (bad pointer, unaligned Device-memory access, missing FP enable, wasm traps are
//! explicit checks in generated code, not CPU exceptions): the handler dumps the syndrome
//! registers over serial and parks so the output can be read.

/// Names for the 16 vector-table entries, indexed by the value the stub passes in.
const VECTOR_NAMES: [&str; 16] = [
    "current EL, SP_EL0: synchronous",
    "current EL, SP_EL0: IRQ",
    "current EL, SP_EL0: FIQ",
    "current EL, SP_EL0: SError",
    "current EL, SP_ELx: synchronous",
    "current EL, SP_ELx: IRQ",
    "current EL, SP_ELx: FIQ",
    "current EL, SP_ELx: SError",
    "lower EL, aarch64: synchronous",
    "lower EL, aarch64: IRQ",
    "lower EL, aarch64: FIQ",
    "lower EL, aarch64: SError",
    "lower EL, aarch32: synchronous",
    "lower EL, aarch32: IRQ",
    "lower EL, aarch32: FIQ",
    "lower EL, aarch32: SError",
];

/// IRQ handler, called from the IRQ vector stub (`__irq_entry` in src/boot.rs) with the
/// caller-saved registers already preserved. Acknowledges the pending interrupt at the GIC,
/// services it (the generic timer is disabled so its level-sensitive line drops — the
/// executor re-arms it before the next `wfi`; the UART's RX bytes are drained into the input
/// ring and its interrupt cleared), and signals end-of-interrupt. Both sources exist to wake
/// the executor's `wfi` idle path — the timer for sleep deadlines, the UART for input.
#[unsafe(no_mangle)]
extern "C" fn kirq() {
    let iar = crate::gic::acknowledge();
    let intid = iar & 0x3ff;
    // 1020-1023 are spurious / special and must not be EOI'd.
    if intid >= 1020 {
        return;
    }
    // Generic-timer PPIs (26/27/29/30): drop the level-sensitive line before the EOI.
    if matches!(intid, 26 | 27 | 29 | 30) {
        crate::timer::disable();
    }
    // PL011 UART (SPI 33 on `virt`): drain received bytes into the input ring and clear the
    // UART's interrupt sources, so the keystroke that woke the core is captured and the
    // level-sensitive line deasserts before the EOI.
    if intid == 33 {
        crate::uart::drain_rx();
    }
    crate::gic::end_of_interrupt(iar);
}

/// Called from every exception vector (src/boot.rs) with the vector index and the
/// syndrome/return/fault-address registers already read.
#[unsafe(no_mangle)]
extern "C" fn kexception(vector: u64, esr: u64, elr: u64, far: u64) -> ! {
    let name = VECTOR_NAMES
        .get(vector as usize)
        .copied()
        .unwrap_or("unknown vector");
    crate::kprintln!();
    crate::kprintln!("FATAL EXCEPTION: {name} (vector {vector})");
    crate::kprintln!("  esr_el1 = {esr:#018x} (EC = {:#04x})", (esr >> 26) & 0x3f);
    crate::kprintln!("  elr_el1 = {elr:#018x}");
    crate::kprintln!("  far_el1 = {far:#018x}");
    crate::kprintln!("parked; exit QEMU with Ctrl-A then X");
    crate::psci::park()
}
