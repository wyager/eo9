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
#[cfg(feature = "wasm-store")]
pub(crate) mod pci_map {
    /// ECAM (PCIe configuration space) base.
    pub(crate) const ECAM_BASE: usize = 0x3f00_0000;
    /// Buses covered by the 16 MiB low ECAM window (1 MiB per bus).
    pub(crate) const ECAM_BUSES: u8 = 16;
    /// 32-bit PCIe MMIO window: where unassigned memory BARs get placed.
    pub(crate) const MMIO_BASE: usize = 0x1000_0000;
    pub(crate) const MMIO_END: usize = 0x3eff_0000;
}

/// Boot banner: machine identification, exception level, timer frequency, wall clock.
pub(crate) fn banner() {
    crate::kprintln!();
    crate::kprintln!("Eo9 kernel — aarch64 (QEMU virt)");
    crate::kprintln!("  exception level: EL{}", current_el());
    crate::kprintln!("  counter-timer frequency: {} Hz", timer::frequency());
    crate::kprintln!(
        "  wall clock: {}.{:09} s since the Unix epoch (PL031 + generic timer)",
        rtc::seconds(),
        timer::subsecond_ns()
    );
}

/// Interrupt delivery: bring up the GICv2 and forward the EL1 physical timer PPI (INTID 30)
/// plus the PL011 UART (SPI 33 on `virt`) so the executor can `wfi`-idle and be woken either
/// by the timer (a sleep deadline) or by a keystroke arriving on the console — instead of
/// busy-polling. The IRQ vector (boot.rs `__irq_entry` → `kirq`) acknowledges and EOIs them
/// (draining UART input into the ring); every other exception stays fatal.
pub(crate) fn interrupts_init() {
    gic::init();
    for intid in [26u32, 27, 29, 30, 33] {
        gic::configure_intid(intid);
        gic::enable_intid(intid);
    }
    // Unmask the UART receive interrupt so an arriving byte asserts SPI 33.
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
