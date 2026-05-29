//! Minimal 8259A PIC driver — enough to route the PIT timer (IRQ 0) and the COM1 UART
//! (IRQ 4) to the CPU as vectors 0x20 and 0x24.
//!
//! q35 still wires the legacy interrupt sources through the dual 8259s, and for a
//! single-CPU kernel that needs exactly two lines the PIC is materially simpler than
//! bringing up the LAPIC + IOAPIC (which would need ACPI table parsing and LAPIC timer
//! calibration). The upgrade path — LAPIC/IOAPIC, MSI for PCI devices, and the LAPIC
//! one-shot timer — is recorded in plan/12 and becomes worthwhile once the x86_64 port
//! reaches the codegen/driver milestones. The PICs are remapped away from the CPU exception
//! range (master → 0x20, slave → 0x28) and every line except the ones explicitly unmasked
//! stays masked.

use super::io::{inb, io_wait, outb};

/// Master PIC command/data ports.
const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
/// Slave PIC command/data ports.
const SLAVE_CMD: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

/// End-of-interrupt command.
const EOI: u8 = 0x20;
/// Read the in-service register on the next data-port read.
const READ_ISR: u8 = 0x0B;

/// Vector base the master PIC is remapped to (IRQ 0 → vector 0x20).
pub(super) const VECTOR_BASE: u8 = 0x20;

/// PIT timer line.
pub(super) const IRQ_TIMER: u8 = 0;
/// COM1 UART line.
pub(super) const IRQ_COM1: u8 = 4;

/// Remap both PICs to vectors 0x20..0x2F and mask every line. Call once during boot before
/// unmasking the lines the kernel owns.
pub(super) fn init() {
    // ICW1: start initialization, expect ICW4.
    outb(MASTER_CMD, 0x11);
    io_wait();
    outb(SLAVE_CMD, 0x11);
    io_wait();
    // ICW2: vector offsets.
    outb(MASTER_DATA, VECTOR_BASE);
    io_wait();
    outb(SLAVE_DATA, VECTOR_BASE + 8);
    io_wait();
    // ICW3: master has the slave on IRQ 2; the slave's cascade identity is 2.
    outb(MASTER_DATA, 1 << 2);
    io_wait();
    outb(SLAVE_DATA, 2);
    io_wait();
    // ICW4: 8086 mode.
    outb(MASTER_DATA, 0x01);
    io_wait();
    outb(SLAVE_DATA, 0x01);
    io_wait();
    // Mask everything; lines are unmasked individually as the kernel takes ownership.
    outb(MASTER_DATA, 0xFF);
    outb(SLAVE_DATA, 0xFF);
}

/// Mask or unmask one IRQ line (0..=15).
pub(super) fn set_masked(irq: u8, masked: bool) {
    let (port, bit) = if irq < 8 {
        (MASTER_DATA, irq)
    } else {
        (SLAVE_DATA, irq - 8)
    };
    let mask = inb(port);
    let mask = if masked {
        mask | (1 << bit)
    } else {
        mask & !(1 << bit)
    };
    outb(port, mask);
}

/// Acknowledge an interrupt on `irq` (0..=15) so the PIC can deliver the next one.
pub(super) fn end_of_interrupt(irq: u8) {
    if irq >= 8 {
        outb(SLAVE_CMD, EOI);
    }
    outb(MASTER_CMD, EOI);
}

/// Whether `irq` (0..=15) is actually in service — used to tell a real IRQ 7/15 from the
/// 8259's spurious-interrupt artifact, which must not be acknowledged.
pub(super) fn in_service(irq: u8) -> bool {
    let (cmd, bit) = if irq < 8 {
        (MASTER_CMD, irq)
    } else {
        (SLAVE_CMD, irq - 8)
    };
    outb(cmd, READ_ISR);
    inb(cmd) & (1 << bit) != 0
}
