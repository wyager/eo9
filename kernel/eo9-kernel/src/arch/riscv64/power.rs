//! Power the machine off: SBI system reset, with the `virt` test device as a fallback.
//!
//! OpenSBI implements the SRST extension on QEMU's `virt` machine, so a shutdown request
//! makes QEMU exit — keeping `cargo xtask qemu riscv64` scriptable, exactly like PSCI
//! SYSTEM_OFF does for the aarch64 port. If SRST is somehow unavailable the SiFive test
//! device (the `virt` machine's syscon power-off register) is poked directly before parking.

/// SiFive test device on the `virt` machine; writing FINISHER_PASS powers the machine off.
const TEST_DEVICE: usize = 0x0010_0000;
/// "Finisher" value for a clean power-off.
const FINISHER_PASS: u32 = 0x5555;

/// The power-off mechanism named in the shared end-of-run banner (src/main.rs).
pub const OFF_REQUEST: &str = "SBI system shutdown";

/// Ask the platform to power off; falls back to the test device, then parks.
pub fn system_off() -> ! {
    super::sbi::system_reset_shutdown();
    // SAFETY: the test device's finisher register is a valid MMIO word on the `virt`
    // machine; writing it powers the machine off (or does nothing on other platforms).
    unsafe { core::ptr::write_volatile(TEST_DEVICE as *mut u32, FINISHER_PASS) };
    park()
}

/// Low-power spin, for when there is nothing left to do (or power-off failed).
pub fn park() -> ! {
    loop {
        // SAFETY: `wfi` only pauses the hart until the next interrupt event.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
