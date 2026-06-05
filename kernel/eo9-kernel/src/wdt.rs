//! Hardware watchdog (board profile only): the autonomous-dev-loop hang backstop.
//!
//! On the Orange Pi 5 Plus the planner drives an unattended UART loop — nobody is at the
//! power button — so *every* kernel outcome must return the board to U-Boot. Clean exits
//! and panics issue PSCI `SYSTEM_RESET` (src/arch/aarch64/power.rs, src/panic.rs); this
//! module covers the third case, a hang: the RK3588's DW-APB watchdog is armed at boot and
//! patted from the drive loop and the idle path, so if the kernel ever stops making
//! scheduling progress for [`TIMEOUT_SECS`] the SoC resets itself back to U-Boot.
//!
//! Doctrine note (SPEC "liveness is event-driven"): this is not a progress backstop — it
//! never *makes* anything advance. It is a dead-man's switch whose firing means the kernel
//! is gone; the only alternative is a bench visit.
//!
//! Hardware: `wdt@feaf0000` (mainline `rk3588s.dtsi`), compatible `rockchip,rk3588-wdt` +
//! `snps,dw-wdt` — the stock Synopsys DW-WDT block, driven exactly like Linux
//! `drivers/watchdog/dw_wdt.c`:
//!   * `WDT_CR` (+0x00): bit 0 = enable (sticky until reset), bit 1 = response mode
//!     (0 = reset the system directly on timeout — what we want; 1 would IRQ first).
//!   * `WDT_TORR` (+0x04): timeout range. The period is `2^(16 + TOP)` tclk cycles; the
//!     same value is mirrored into TOP_INIT (bits 7:4) as Linux does, and a kick loads it.
//!   * `WDT_CCVR` (+0x08): current count (read-only; used to verify the block is alive).
//!   * `WDT_CRR` (+0x0c): writing `0x76` restarts the counter — the "pat".
//!
//! Timeout math: `tclk_wdt0` is gated straight from `xin24m` in the RK3588 CRU (mainline
//! `clk-rk3588.c`), so tclk = 24 MHz and `TOP = 13` gives `2^29 / 24e6 ≈ 22.4 s` — far
//! beyond any legitimate scheduling gap (native on-target compiles measure well under 2 s)
//! while still bounding a wedged board to ~22 s of downtime before U-Boot returns.
//!
//! Arming is **best-effort with loud verification**: if firmware left the WDT clock gated,
//! the enable bit will not read back (or the counter will not move) — we report that
//! hang recovery is unavailable and boot on, rather than blindly poking CRU gate registers
//! (write-mask-high semantics; a wrong blind write risks the boot far more than a missing
//! watchdog does). The CRU ungate, if the live board turns out to need it, is a follow-up
//! taken with the board on the bench.

#[cfg(all(target_arch = "aarch64", feature = "board-opi5plus"))]
mod hw {
    /// DW-APB watchdog 0 on the RK3588 (`wdt@feaf0000`, mainline rk3588s.dtsi).
    const WDT_BASE: usize = 0xfeaf_0000;
    /// Control register: bit 0 enable (sticky), bit 1 response mode (0 = direct reset).
    const WDT_CR: usize = 0x00;
    /// Timeout range register: TOP in bits 3:0, TOP_INIT in bits 7:4.
    const WDT_TORR: usize = 0x04;
    /// Current counter value (read-only).
    const WDT_CCVR: usize = 0x08;
    /// Counter restart register: write [`CRR_KICK`] to reload the counter from TOP.
    const WDT_CRR: usize = 0x0c;
    /// The magic restart value (Synopsys databook / Linux dw_wdt.c).
    const CRR_KICK: u32 = 0x76;
    /// Period exponent: `2^(16+13)` cycles at the 24 MHz tclk ≈ 22.4 s.
    const TOP: u32 = 13;
    /// The human-readable timeout, kept in sync with [`TOP`] for the boot banner.
    pub(super) const TIMEOUT_SECS: u32 = 22;

    fn read(offset: usize) -> u32 {
        // SAFETY: `WDT_BASE + offset` is a DW-WDT register inside the identity-mapped
        // RK3588 device window; `crate::mmio` pins the access to a syndrome-valid GPR form.
        unsafe { crate::mmio::read_u32(WDT_BASE + offset) }
    }

    fn write(offset: usize, value: u32) {
        // SAFETY: as `read`, for writes.
        unsafe { crate::mmio::write_u32(WDT_BASE + offset, value) }
    }

    /// Restart the counter (the pat). One syndrome-valid store; cheap enough for every
    /// drive-loop pass.
    #[inline]
    pub(super) fn pat() {
        write(WDT_CRR, CRR_KICK);
    }

    /// Program the timeout, enable the dog (direct-reset mode), kick it, and verify the
    /// block actually took the configuration. Returns whether hang recovery is armed.
    pub(super) fn arm() -> bool {
        // The Linux dw_wdt_start order: timeout range first (TOP mirrored into TOP_INIT),
        // a kick to load it, then enable with response mode 0 (straight to system reset).
        write(WDT_TORR, TOP | (TOP << 4));
        pat();
        write(WDT_CR, 0x1);
        pat();
        // Verify: the sticky enable bit must read back, and the counter must be live
        // (a clock-gated block reads as dead). CCVR right after a kick sits near
        // 2^(16+TOP), which is non-zero — zero means the block never loaded TOP.
        let enabled = read(WDT_CR) & 0x1 == 0x1;
        let counting = read(WDT_CCVR) != 0;
        enabled && counting
    }
}

#[cfg(all(target_arch = "aarch64", feature = "board-opi5plus"))]
pub fn arm_and_report() {
    if hw::arm() {
        crate::kprintln!(
            "wdt: armed ({}s) - a hang now resets the SoC back to U-Boot",
            hw::TIMEOUT_SECS
        );
    } else {
        crate::kprintln!("wdt: arm FAILED (clock gated?) - hang recovery unavailable");
    }
}

/// Pat the watchdog. Called from every drive-loop pass and every idle wake; a no-op off
/// the board profile (QEMU runs carry no watchdog and are byte-for-behavior unchanged).
#[cfg(all(target_arch = "aarch64", feature = "board-opi5plus"))]
#[allow(dead_code)] // callers live in the wasm drive loops (feature-gated)
#[inline]
pub fn pat() {
    hw::pat();
}

#[cfg(not(all(target_arch = "aarch64", feature = "board-opi5plus")))]
pub fn arm_and_report() {}

#[cfg(not(all(target_arch = "aarch64", feature = "board-opi5plus")))]
#[allow(dead_code)] // callers live in the wasm drive loops (feature-gated)
#[inline]
pub fn pat() {}
