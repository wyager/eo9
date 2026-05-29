//! PL031 real-time clock on QEMU's aarch64 `virt` machine.
//!
//! The RTC's data register holds seconds since the Unix epoch (QEMU initialises it from
//! the host clock), which is exactly what the `eo9:time/time.now` wall clock needs; the
//! sub-second part comes from the generic timer (src/timer.rs).

/// PL031 base address on the QEMU `virt` machine.
const RTC_BASE: usize = 0x0901_0000;
/// Data register: current time in seconds since the Unix epoch.
const RTCDR: usize = 0x000;

/// Seconds since the Unix epoch.
pub fn seconds() -> u32 {
    // SAFETY: `RTC_BASE + RTCDR` is the PL031 data register on the `virt` machine; a
    // volatile MMIO read has no other side conditions.
    unsafe { core::ptr::read_volatile((RTC_BASE + RTCDR) as *const u32) }
}
