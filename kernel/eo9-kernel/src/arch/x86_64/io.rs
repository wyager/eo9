//! x86 port I/O helpers. Every legacy device this port drives — the COM1 UART, the 8259
//! PICs, the PIT, the CMOS RTC, and the ACPI power-off register — is reached through
//! `in`/`out` instructions rather than MMIO.

/// Read one byte from an I/O port.
pub(super) fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: port reads have no memory side effects; every caller names a fixed legacy
    // port that exists on the q35 machine.
    unsafe {
        core::arch::asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write one byte to an I/O port.
pub(super) fn outb(port: u16, value: u8) {
    // SAFETY: as above, for writes.
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Write one 16-bit word to an I/O port (the ACPI PM1a control register).
pub(super) fn outw(port: u16, value: u16) {
    // SAFETY: as above, for writes.
    unsafe {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags));
    }
}

/// A short delay between programming steps of the legacy PIC: a write to the conventional
/// "POST diagnostic" port 0x80, which is unused and takes roughly a microsecond.
pub(super) fn io_wait() {
    outb(0x80, 0);
}
