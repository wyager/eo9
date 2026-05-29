//! Minimal GICv2 bring-up — enough to take timer and UART interrupts and let the core sleep.
//!
//! The kernel's executor used to busy-poll on `Poll::Pending` (a guest awaiting
//! `time.sleep`, or eosh awaiting `read-line` at the prompt), pinning a host CPU at 100%.
//! The fix is to `wfi` instead, which only wakes on an interrupt that reaches the PE — so we
//! bring up the GIC distributor + CPU interface and *forward* the EL1 physical timer PPI
//! (INTID 30) and the PL011 UART SPI (INTID 33) to this core.
//!
//! Interrupts are taken as exceptions: IRQs are unmasked (PSTATE.I = 0) once the GIC is up,
//! the EL1 IRQ vector dispatches to `exceptions::kirq`, which reads the IAR, services the
//! source (re-arms/quiets the timer, drains the UART RX FIFO into a ring), and writes EOI.
//! Synchronous exceptions stay fatal as before. The core halts in `wfi` at an idle prompt
//! and wakes promptly on a keystroke or the armed timer deadline (src/timer.rs, src/uart.rs).
//!
//! This needs the QEMU `virt` machine to expose a GICv2 (`-M virt,gic-version=2` in xtask);
//! GICv3 would use a system-register CPU interface and per-PE redistributors instead.

/// GIC distributor base on the QEMU `virt` machine (GICv2).
const GICD_BASE: usize = 0x0800_0000;
/// GIC CPU interface base on the QEMU `virt` machine (GICv2).
const GICC_BASE: usize = 0x0801_0000;

/// Distributor control register.
const GICD_CTLR: usize = 0x000;
/// Set-enable registers (one bit per INTID; write-1-to-set).
const GICD_ISENABLER: usize = 0x100;

/// CPU interface control register.
const GICC_CTLR: usize = 0x000;
/// Interrupt priority mask register (only higher-priority — numerically lower — interrupts
/// are forwarded; 0xff lets everything through).
const GICC_PMR: usize = 0x004;
/// Interrupt acknowledge register (read to take the pending interrupt; returns its INTID).
const GICC_IAR: usize = 0x00c;
/// End-of-interrupt register (write the value read from IAR to complete the interrupt).
const GICC_EOIR: usize = 0x010;

fn gicc_read(offset: usize) -> u32 {
    // SAFETY: `GICC_BASE + offset` is a valid GICv2 CPU-interface register on `virt`.
    unsafe { core::ptr::read_volatile((GICC_BASE + offset) as *const u32) }
}

/// Acknowledge the highest-priority pending interrupt, returning the raw IAR value (its low
/// 10 bits are the INTID; 1020-1023 are spurious). Pass the same value back to [`end_of_interrupt`].
pub fn acknowledge() -> u32 {
    gicc_read(GICC_IAR)
}

/// Complete an interrupt previously taken with [`acknowledge`].
pub fn end_of_interrupt(iar: u32) {
    gicc_write(GICC_EOIR, iar);
}

fn gicd_write(offset: usize, value: u32) {
    // SAFETY: `GICD_BASE + offset` is a valid GICv2 distributor register on `virt`.
    unsafe { core::ptr::write_volatile((GICD_BASE + offset) as *mut u32, value) }
}

fn gicc_write(offset: usize, value: u32) {
    // SAFETY: `GICC_BASE + offset` is a valid GICv2 CPU-interface register on `virt`.
    unsafe { core::ptr::write_volatile((GICC_BASE + offset) as *mut u32, value) }
}

/// Enable the distributor and this core's CPU interface so forwarded interrupts can reach
/// the PE (and thus serve as `wfi` wake-ups). Call once during boot, after the MMU is on
/// (the GIC sits in the device-mapped low gigabyte).
pub fn init() {
    // Enable the distributor's interrupt forwarding.
    gicd_write(GICD_CTLR, 1);
    // Let interrupts of any priority through the CPU interface (lower value = higher prio).
    gicc_write(GICC_PMR, 0xff);
    // No sub-priority preemption grouping needed.
    gicc_write(0x008 /* GICC_BPR */, 0);
    // Enable the CPU interface so enabled, pending interrupts assert this PE's IRQ line.
    gicc_write(GICC_CTLR, 1);
}

/// Diagnostic: give an INTID a usable (non-zero, mid) priority and put it in group 0.
pub fn configure_intid(intid: u32) {
    // Priority register: one byte per INTID.
    let prio_reg = 0x400 + (intid as usize);
    // SAFETY: GICD_IPRIORITYR byte accessible.
    unsafe { core::ptr::write_volatile((GICD_BASE + prio_reg) as *mut u8, 0x80) };
}

/// Enable forwarding of a single interrupt ID (e.g. INTID 30, the EL1 physical timer PPI).
pub fn enable_intid(intid: u32) {
    let register = GICD_ISENABLER + (intid as usize / 32) * 4;
    // ISENABLER is write-1-to-set: writing the single bit enables just that INTID.
    gicd_write(register, 1u32 << (intid % 32));
}
