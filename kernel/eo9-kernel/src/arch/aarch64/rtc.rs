//! PL031 real-time clock on QEMU's aarch64 `virt` machine.
//!
//! The RTC's data register holds seconds since the Unix epoch (QEMU initialises it from
//! the host clock), which is exactly what the `eo9:time/time.now` wall clock needs; the
//! sub-second part comes from the generic timer (src/timer.rs).

/// PL031 base address on the QEMU `virt` machine.
#[cfg(not(feature = "board-opi5plus"))]
const RTC_BASE: usize = 0x0901_0000;
/// Data register: current time in seconds since the Unix epoch.
#[cfg(not(feature = "board-opi5plus"))]
const RTCDR: usize = 0x000;

/// Seconds since the Unix epoch.
///
/// The Orange Pi 5 Plus board profile has no PL031 (the RK3588's RTC lives on the PMIC,
/// off the kernel's day-one path — docs/board/orange-pi-5-plus.md), so the wall clock
/// reads 0 there: `time.now` starts at the epoch, monotonic time is unaffected.
pub fn seconds() -> u32 {
    #[cfg(feature = "board-opi5plus")]
    {
        0
    }
    #[cfg(not(feature = "board-opi5plus"))]
    // SAFETY: `RTC_BASE + RTCDR` is the PL031 data register on the `virt` machine;
    // `crate::mmio` pins the access to a syndrome-valid GPR form.
    unsafe {
        crate::mmio::read_u32(RTC_BASE + RTCDR)
    }
}
