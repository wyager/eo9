//! Minimal PSCI client: end-of-run power transition.
//!
//! QEMU's `virt` machine provides PSCI through the HVC conduit when the guest is started
//! without EL2/EL3 (which is how xtask launches it), so a single `SYSTEM_OFF` call makes
//! QEMU exit. That gives `cargo xtask qemu aarch64` a clean, scriptable end-of-run instead
//! of requiring the user to kill the emulator by hand.
//!
//! On the Orange Pi 5 Plus (`board-opi5plus`) the conduit is **SMC** to TF-A BL31, and the
//! transition is **SYSTEM_RESET, not SYSTEM_OFF**: the board runs an unattended UART dev
//! loop where nobody can touch the power button, so every kernel exit — clean run, panic,
//! `poweroff` builtin — must land back at the U-Boot prompt for the next iteration. An
//! actually-off board is a bench visit; a reset board is the loop continuing.

/// PSCI 0.2 `SYSTEM_OFF` function id (SMC64 calling convention).
#[cfg(not(feature = "board-opi5plus"))]
const PSCI_END_OF_RUN: u64 = 0x8400_0008;
/// PSCI 0.2 `SYSTEM_RESET` function id (SMC64 calling convention) — the board dev loop.
#[cfg(feature = "board-opi5plus")]
const PSCI_END_OF_RUN: u64 = 0x8400_0009;

/// Ask the platform for the end-of-run transition (off on QEMU, reset on the board);
/// parks the core if the call somehow returns.
///
/// Conduit: QEMU `virt` (no EL3) serves PSCI over **HVC**; on the Orange Pi 5 Plus the
/// call goes to TF-A BL31 at EL3 over **SMC** (the board DTB's `psci.method`), selected by
/// the `board-opi5plus` profile.
pub fn system_off() -> ! {
    // On the board, make sure everything printed (the outcome line, a panic report) is on
    // the wire before the SoC resets out from under the UART FIFO.
    #[cfg(feature = "board-opi5plus")]
    super::uart::tx_drain();
    // SAFETY: a PSCI call via the platform's conduit with a valid function id either does
    // not return (SYSTEM_OFF/SYSTEM_RESET) or returns an error in x0; it clobbers only
    // x0-x3 per SMCCC.
    unsafe {
        #[cfg(not(feature = "board-opi5plus"))]
        core::arch::asm!(
            "hvc #0",
            inout("x0") PSCI_END_OF_RUN => _,
            lateout("x1") _,
            lateout("x2") _,
            lateout("x3") _,
            options(nomem, nostack),
        );
        #[cfg(feature = "board-opi5plus")]
        core::arch::asm!(
            "smc #0",
            inout("x0") PSCI_END_OF_RUN => _,
            lateout("x1") _,
            lateout("x2") _,
            lateout("x3") _,
            options(nomem, nostack),
        );
    }
    park()
}

/// The end-of-run mechanism named in the shared banner (src/main.rs).
#[cfg(not(feature = "board-opi5plus"))]
pub const OFF_REQUEST: &str = "PSCI SYSTEM_OFF";
/// Board profile: every exit is a reset back to U-Boot (the autonomous dev loop).
#[cfg(feature = "board-opi5plus")]
pub const OFF_REQUEST: &str = "PSCI SYSTEM_RESET (back to U-Boot)";

/// Low-power spin, for when there is nothing left to do (or power-off failed).
pub fn park() -> ! {
    loop {
        // SAFETY: `wfe` only pauses the core until the next event.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
