//! Fatal exception reporting.
//!
//! The spike kernel has no legitimate exception traffic: interrupts stay masked (the timer
//! is polled) and wasm traps are explicit checks in the generated code, not CPU
//! exceptions. Any vector firing therefore indicates a kernel bug (bad pointer, unaligned
//! Device-memory access, missing FP enable, …), and the most useful thing to do is dump
//! the syndrome registers over serial and park so the output can be read.

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
