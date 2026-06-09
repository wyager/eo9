//! aarch64 on QEMU `virt`: EL1, PL011 UART, PL031 RTC, the generic timer, a GICv2 for
//! timer/UART interrupt delivery, and an identity-mapped MMU with W^X for published JIT
//! code pages. This is the reference architecture port (plan/12-kernel.md).

mod boot;
mod exceptions;
mod gic;
pub(crate) mod mmu;
pub(crate) mod power;
pub(crate) mod rtc;
pub(crate) mod timer;
pub(crate) mod uart;

/// Architecture name as spelled in `cargo xtask build-kernel <arch>` / `cargo xtask qemu <arch>`.
pub(crate) const NAME: &str = "aarch64";

/// Where PCI Express lives on this machine (QEMU `virt` with `highmem=off` — xtask passes
/// it so the ECAM stays below 4 GiB, inside the identity-mapped device gigabyte). Consumed
/// by the shared `src/pci.rs`, which is only built with the wasm-store feature.
///
/// The `board-opi5plus` profile has no ECAM at all: the RK3588's DesignWare controllers
/// are reached through the `ConfigAccess` shim's DW implementation, with the board
/// constants and bring-up in [`rk3588_pcie`] (docs/board/rk3588-pcie.md) — this module is
/// QEMU's.
#[cfg(all(feature = "wasm-store", not(feature = "board-opi5plus")))]
pub(crate) mod pci_map {
    /// ECAM (PCIe configuration space) base.
    pub(crate) const ECAM_BASE: usize = 0x3f00_0000;
    /// Buses covered by the 16 MiB low ECAM window (1 MiB per bus).
    pub(crate) const ECAM_BUSES: u8 = 16;
    /// 32-bit PCIe MMIO window: where unassigned memory BARs get placed.
    pub(crate) const MMIO_BASE: usize = 0x1000_0000;
    pub(crate) const MMIO_END: usize = 0x3eff_0000;
}

/// RK3588 DW-PCIe constants and bring-up for the Orange Pi 5 Plus (the controllers
/// serving the two onboard RTL8125 NICs). Board profile only; `wasm-store` because the
/// shared `src/pci.rs` it feeds is wasm-store-gated.
#[cfg(all(feature = "wasm-store", feature = "board-opi5plus"))]
pub(crate) mod rk3588_pcie;

/// PCI INTx delivery on this machine: the gpex host bridge's four legacy interrupt lines
/// land on GIC SPIs 35-38 (`virt`'s irqmap entry 3 + the SPI base 32), level-sensitive.
/// The IRQ handler (`exceptions::kirq`) masks a fired line and records it via
/// `crate::pci::intx_record`; the wasm provider's `wait` consumes the count and unmasks
/// through these functions. Consumed by `src/wasm/pci_provider.rs` (wasm-store builds only).
#[cfg(all(feature = "wasm-store", not(feature = "board-opi5plus")))]
pub(crate) mod pci_intx {
    /// GIC INTID of gpex line 0; lines 1-3 follow consecutively.
    pub(crate) const BASE_INTID: u32 = 35;
    /// Whether this architecture routes PCI interrupts at all (the provider answers
    /// `unsupported` to `enable-interrupts` where it does not).
    pub(crate) const WIRED: bool = true;

    fn intid(line: usize) -> u32 {
        BASE_INTID + (line % crate::pci::INTX_LINES) as u32
    }

    /// Unmask one gpex line at the GIC so a pending or future level-triggered assert is
    /// delivered (and wakes a `wfi`).
    pub(crate) fn unmask(line: usize) {
        super::gic::configure_intid(intid(line));
        super::gic::enable_intid(intid(line));
    }

    /// Mask one gpex line at the GIC.
    pub(crate) fn mask(line: usize) {
        super::gic::disable_intid(intid(line));
    }
}

/// PCI INTx delivery, board profile: not wired yet. The RK3588's DW controllers deliver
/// all four INTx pins on ONE GIC SPI per controller (rk3588-base.dtsi: SPI 245 for
/// pcie2x1l1, SPI 250 for pcie2x1l2, edge-rising), demuxed by reading the controller's
/// `PCIE_CLIENT_INTR_STATUS_LEGACY` APB register — per-controller demux state the shared
/// swizzle model does not carry yet. The provider answers `unsupported` to
/// `enable-interrupts`, so drivers fall back to their polled paths (`rk3588_pcie` module
/// docs sketch the wiring; the RTL8125 driver lane picks it up).
#[cfg(all(feature = "wasm-store", feature = "board-opi5plus"))]
pub(crate) mod pci_intx {
    /// Whether this architecture routes PCI interrupts at all.
    pub(crate) const WIRED: bool = false;

    pub(crate) fn unmask(_line: usize) {}

    pub(crate) fn mask(_line: usize) {}
}

/// The machine's platform (memory-mapped, non-PCI) device region table, consumed by
/// `src/platform.rs` for the `eo9:platform` root provider (wasm-store builds only).
///
/// QEMU `virt` exposes two benign test regions so the provider's full semantics —
/// exclusive claim/busy, in-bounds access, the per-name boot grant — are exercisable
/// under the scripted battery before any board hardware exists (the M0 lane of
/// docs/board/usb-ohci-plan.md; `check-usb`'s platcheck step drives them):
///
/// * `pl031-rtc` — the PL031 real-time clock (the same device `rtc.rs` reads at boot;
///   reads are side-effect-free, RTCDR at offset 0 ticks once a second).
/// * `pl061-gpio` — the PL061 GPIO block (QEMU `virt` memmap: 0x0903_0000). Listed so a
///   table with MORE regions than a restricted grant (`platform=pl031-rtc`) exists —
///   the cross-region claim-denied case needs a present-but-ungranted name.
///
/// Both sit in the identity-mapped device gigabyte. The board profile's table (the
/// RK3588 EHCI/OHCI register blocks) lands with the M1 board lane — empty until then,
/// so a board boot with the `platform` token grants nothing yet.
#[cfg(all(feature = "wasm-store", not(feature = "board-opi5plus")))]
pub(crate) mod platform_regions {
    use crate::platform::RegionDef;

    pub(crate) const REGIONS: &[RegionDef] = &[
        RegionDef {
            name: "pl031-rtc",
            base: 0x0901_0000,
            size: 0x1000,
            has_irq: false,
        },
        RegionDef {
            name: "pl061-gpio",
            base: 0x0903_0000,
            size: 0x1000,
            has_irq: false,
        },
    ];
}

/// Platform region table, board profile: empty until the M1 USB lane lands the four
/// RK3588 EHCI/OHCI register blocks (docs/board/usb-ohci-plan.md §0).
#[cfg(all(feature = "wasm-store", feature = "board-opi5plus"))]
pub(crate) mod platform_regions {
    use crate::platform::RegionDef;

    pub(crate) const REGIONS: &[RegionDef] = &[];
}

/// DMA-coherence maintenance for buffers a PCI device masters (descriptor rings, frame
/// buffers). On the board the RK3588's PCIe controllers are NOT cache-coherent — mainline
/// `rk3588-base.dtsi` carries no `dma-coherent` on any pcie node, so Linux uses
/// non-cacheable coherent allocations + streaming cache maintenance there — while this
/// kernel's DMA buffers live in the ordinary (cacheable) heap. Every CPU access to such a
/// buffer therefore brackets itself with a clean+invalidate-to-PoC sweep (the
/// bringup-playbook §3 rule: every handoff between cache regimes sweeps the shared bytes,
/// and a bus-mastering device is exactly an agent reading/writing at the PoC):
///
/// * after the CPU writes (descriptor publish, transmit payload): the sweep's `dsb sy`
///   also IS the reference drivers' `dma_wmb()`-before-doorbell — the descriptor reaches
///   DRAM before any subsequent Device-memory doorbell store can be issued;
/// * before the CPU reads (completion poll, received frame): the invalidate discards
///   stale lines so the load observes what the device wrote.
///
/// QEMU `virt` emulates coherent DMA (device writes land in the same host memory the
/// guest's cached accesses use), so the non-board build is a no-op.
#[cfg(feature = "wasm-store")]
pub(crate) mod dma_coherence {
    /// Clean+invalidate `[start, start+len)` to the PoC and barrier (board), or nothing
    /// (QEMU virt — coherent).
    #[cfg(feature = "board-opi5plus")]
    pub(crate) fn sync(start: usize, len: usize) {
        super::mmu::clean_invalidate_to_poc(start, len);
    }

    #[cfg(not(feature = "board-opi5plus"))]
    pub(crate) fn sync(_start: usize, _len: usize) {}
}

/// Boot banner: machine identification, exception level, timer frequency, wall clock.
pub(crate) fn banner() {
    crate::kprintln!();
    #[cfg(feature = "board-opi5plus")]
    crate::kprintln!("Eo9 kernel — aarch64 (Orange Pi 5 Plus, RK3588)");
    #[cfg(not(feature = "board-opi5plus"))]
    crate::kprintln!("Eo9 kernel — aarch64 (QEMU virt)");
    // An optional build stamp baked in at compile time (`EO9_BUILD_STAMP=… cargo build`).
    // Plain builds set nothing and print nothing; `cargo xtask check-kexec` stamps its
    // second kernel so the gate can tell the kexec'd image apart from the booted one on
    // the same serial stream.
    if let Some(stamp) = option_env!("EO9_BUILD_STAMP") {
        crate::kprintln!("  build stamp: {stamp}");
    }
    crate::kprintln!("  exception level: EL{}", current_el());
    crate::kprintln!("  counter-timer frequency: {} Hz", timer::frequency());
    // The board profile has no PL031 (rtc.rs: the RK3588's RTC lives on the PMIC, unread
    // on day one) — say so instead of claiming a device that is not there.
    #[cfg(feature = "board-opi5plus")]
    crate::kprintln!(
        "  wall clock: {}.{:09} s since the Unix epoch (no RTC read on this board yet - generic timer only)",
        rtc::seconds(),
        timer::subsecond_ns()
    );
    #[cfg(not(feature = "board-opi5plus"))]
    crate::kprintln!(
        "  wall clock: {}.{:09} s since the Unix epoch (PL031 + generic timer)",
        rtc::seconds(),
        timer::subsecond_ns()
    );
}

/// Interrupt delivery: bring up the GIC and forward the EL1 virtual timer PPI (INTID 27 —
/// the timer the kernel drives; the whole generic-timer PPI family 26/27/29/30 is enabled)
/// plus the PL011 UART (SPI 33 on `virt`) so the executor can `wfi`-idle and be woken either
/// by the timer (a sleep deadline) or by a keystroke arriving on the console — instead of
/// busy-polling. The IRQ vector (boot.rs `__irq_entry` → `kirq`) acknowledges and EOIs them
/// (draining UART input into the ring); every other exception stays fatal.
pub(crate) fn interrupts_init() {
    gic::init();
    // The generic-timer PPI family everywhere; the console UART's SPI only where it is
    // known (QEMU `virt`: PL011 on SPI 33). The board profile leaves the UART SPI unwired
    // for day one — input arrives through the executor's idle-path scavenger poll instead
    // (src/arch/aarch64/uart.rs module docs; wiring UART2's SPI is a recorded follow-up).
    #[cfg(not(feature = "board-opi5plus"))]
    const INTIDS: [u32; 5] = [26, 27, 29, 30, 33];
    #[cfg(feature = "board-opi5plus")]
    const INTIDS: [u32; 4] = [26, 27, 29, 30];
    for intid in INTIDS {
        gic::configure_intid(intid);
        gic::enable_intid(intid);
    }
    // Unmask the UART receive interrupt so an arriving byte asserts its line (a no-op on
    // the board profile, where the line is left exactly as U-Boot programmed it).
    uart::enable_rx_interrupt();
    // SAFETY: clearing PSTATE.I (DAIF.I) enables IRQ delivery; the IRQ vector is installed.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack)) };
}

/// The current exception level (expected: 1 on QEMU `virt` without EL2/EL3 enabled).
fn current_el() -> u64 {
    let current_el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) current_el, options(nomem, nostack)) };
    (current_el >> 2) & 0b11
}
