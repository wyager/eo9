//! Power the machine off: ACPI S5 through the q35 PM registers.
//!
//! QEMU's q35 machine exposes the ICH9 ACPI PM1a control register at I/O port 0x604;
//! writing the S5 sleep type with the sleep-enable bit powers the machine off and makes
//! QEMU exit with status 0 — keeping `cargo xtask qemu x86_64` scriptable, exactly like
//! PSCI SYSTEM_OFF on aarch64 and SBI SRST on riscv64. The i440fx register (0xB004) is
//! poked as a fallback before parking, in case the machine type ever changes.

use super::io::outw;

/// q35 (ICH9) PM1a control register.
const Q35_PM1A_CNT: u16 = 0x604;
/// i440fx (PIIX4) PM1a control register, the fallback.
const I440FX_PM1A_CNT: u16 = 0xB004;
/// SLP_TYP = S5 | SLP_EN, the value QEMU's ACPI tables advertise for soft-off.
const SLP_TYP5_SLP_EN: u16 = 0x2000;

/// The power-off mechanism named in the shared end-of-run banner (src/main.rs).
pub const OFF_REQUEST: &str = "ACPI S5 power-off";

/// Ask the platform to power off; falls back to the i440fx register, then parks.
pub fn system_off() -> ! {
    outw(Q35_PM1A_CNT, SLP_TYP5_SLP_EN);
    outw(I440FX_PM1A_CNT, SLP_TYP5_SLP_EN);
    park()
}

/// Low-power spin, for when there is nothing left to do (or power-off failed).
pub fn park() -> ! {
    loop {
        // SAFETY: `hlt` only pauses the CPU until the next interrupt event (or forever,
        // with delivery masked) — exactly what parking wants.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}
