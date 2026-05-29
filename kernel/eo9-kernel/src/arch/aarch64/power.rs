//! Minimal PSCI client: power the machine off.
//!
//! QEMU's `virt` machine provides PSCI through the HVC conduit when the guest is started
//! without EL2/EL3 (which is how xtask launches it), so a single `SYSTEM_OFF` call makes
//! QEMU exit. That gives `cargo xtask qemu aarch64` a clean, scriptable end-of-run instead
//! of requiring the user to kill the emulator by hand.

/// PSCI 0.2 `SYSTEM_OFF` function id (SMC64 calling convention).
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// Ask the platform to power off; parks the core if the call somehow returns.
pub fn system_off() -> ! {
    // SAFETY: a PSCI call via HVC with a valid function id either does not return
    // (SYSTEM_OFF) or returns an error in x0; it clobbers only x0-x3 per SMCCC.
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inout("x0") PSCI_SYSTEM_OFF => _,
            lateout("x1") _,
            lateout("x2") _,
            lateout("x3") _,
            options(nomem, nostack),
        );
    }
    park()
}

/// The power-off mechanism named in the shared end-of-run banner (src/main.rs).
pub const OFF_REQUEST: &str = "PSCI SYSTEM_OFF";

/// Low-power spin, for when there is nothing left to do (or power-off failed).
pub fn park() -> ! {
    loop {
        // SAFETY: `wfe` only pauses the core until the next event.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
