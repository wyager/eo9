//! Goldfish RTC on QEMU's riscv64 `virt` machine.
//!
//! The device reports nanoseconds since the Unix epoch (QEMU initialises it from the host
//! clock) through a latched 64-bit register pair: reading `TIME_LOW` returns the low half
//! and latches the matching high half into `TIME_HIGH`. The whole-second part is what the
//! `eo9:time/time.now` wall clock needs; the sub-second part comes from the time counter
//! (src/arch/riscv64/timer.rs), mirroring the aarch64 PL031 + generic-timer split.

/// Goldfish RTC base address on the QEMU riscv64 `virt` machine.
const RTC_BASE: usize = 0x0010_1000;
/// Low 32 bits of the time in nanoseconds (reading latches TIME_HIGH).
const TIME_LOW: usize = 0x00;
/// High 32 bits of the latched time.
const TIME_HIGH: usize = 0x04;

fn mmio_read(offset: usize) -> u32 {
    // SAFETY: `RTC_BASE + offset` is a valid Goldfish RTC register on the `virt` machine; a
    // volatile MMIO read has no other side conditions.
    unsafe { core::ptr::read_volatile((RTC_BASE + offset) as *const u32) }
}

/// Seconds since the Unix epoch.
pub fn seconds() -> u32 {
    let low = u64::from(mmio_read(TIME_LOW));
    let high = u64::from(mmio_read(TIME_HIGH));
    (((high << 32) | low) / 1_000_000_000) as u32
}
