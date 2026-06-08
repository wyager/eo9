//! riscv64 on QEMU `virt`: S-mode under OpenSBI, NS16550A UART, Goldfish RTC, the `time`
//! CSR + SBI timer, and a SiFive-style PLIC for UART interrupt delivery. The port follows
//! the aarch64 reference layer (src/arch/aarch64/) module for module; `mmu` runs the hart
//! under an Sv39 identity map with W^X for published JIT code pages.

mod boot;
mod plic;
mod sbi;
mod traps;

pub(crate) mod mmu;
pub(crate) mod power;
pub(crate) mod rtc;
pub(crate) mod timer;
pub(crate) mod uart;

/// Architecture name as spelled in `cargo xtask build-kernel <arch>` / `cargo xtask qemu <arch>`.
pub(crate) const NAME: &str = "riscv64";

/// Where PCI Express lives on this machine (QEMU riscv64 `virt`; fixed addresses, no
/// `highmem` dependence). The ECAM sits inside the MMIO gigapage the Sv39 map already
/// covers; the 32-bit BAR window `0x4000_0000..0x8000_0000` gets its own RW-NX gigapage in
/// `mmu::init`. Consumed by the shared `src/pci.rs` (wasm-store builds only).
#[cfg(feature = "wasm-store")]
pub(crate) mod pci_map {
    /// ECAM (PCIe configuration space) base.
    pub(crate) const ECAM_BASE: usize = 0x3000_0000;
    /// Buses walked: the window is 256 MiB (256 buses), but everything QEMU `virt` plugs in
    /// with a plain `-device …-pci` lands on bus 0; 16 keeps the walk identical to aarch64.
    pub(crate) const ECAM_BUSES: u8 = 16;
    /// 32-bit PCIe MMIO window: where unassigned memory BARs get placed.
    pub(crate) const MMIO_BASE: usize = 0x4000_0000;
    pub(crate) const MMIO_END: usize = 0x8000_0000;
}

/// PCI INTx delivery on this machine: the gpex host bridge's four legacy interrupt lines
/// land on PLIC sources 0x20-0x23, level-sensitive. The trap handler (`traps::ktrap`) masks
/// a fired source and records it via `crate::pci::intx_record`; the wasm provider's `wait`
/// consumes the count and unmasks through these functions. Consumed by
/// `src/wasm/pci_provider.rs` (wasm-store builds only).
#[cfg(feature = "wasm-store")]
pub(crate) mod pci_intx {
    /// PLIC source number of gpex line 0; lines 1-3 follow consecutively.
    pub(crate) const BASE_SOURCE: u32 = 0x20;
    /// Whether this architecture routes PCI interrupts at all.
    pub(crate) const WIRED: bool = true;

    fn source(line: usize) -> u32 {
        BASE_SOURCE + (line % crate::pci::INTX_LINES) as u32
    }

    /// Unmask one gpex line at the PLIC so a pending or future level-triggered assert is
    /// delivered (and wakes a `wfi`).
    pub(crate) fn unmask(line: usize) {
        super::plic::enable_source(source(line));
    }

    /// Mask one gpex line at the PLIC.
    pub(crate) fn mask(line: usize) {
        super::plic::disable_source(source(line));
    }
}

/// DMA-coherence maintenance for PCI-mastered buffers: a no-op on this port — its only
/// machine is QEMU, whose emulated DMA is coherent (device writes land in the same host
/// memory the guest's cached accesses use). The aarch64 board profile carries the real
/// clean+invalidate sweeps; see its `dma_coherence` docs.
#[cfg(feature = "wasm-store")]
pub(crate) mod dma_coherence {
    pub(crate) fn sync(_start: usize, _len: usize) {}
}

/// Boot banner: machine identification, privilege mode, timer frequency, wall clock.
pub(crate) fn banner() {
    crate::kprintln!();
    crate::kprintln!("Eo9 kernel — riscv64 (QEMU virt)");
    crate::kprintln!("  privilege: S-mode (entered from OpenSBI)");
    crate::kprintln!("  time counter frequency: {} Hz", timer::frequency());
    crate::kprintln!(
        "  wall clock: {}.{:09} s since the Unix epoch (Goldfish RTC + time CSR)",
        rtc::seconds(),
        timer::subsecond_ns()
    );
}

/// Interrupt delivery: forward the UART receive line (PLIC source 10) to this hart's S-mode
/// context, enable the supervisor timer and external interrupts in `sie`, and unmask
/// delivery (`sstatus.SIE`) — so the executor can halt in `wfi` and be woken either by a
/// timer deadline or by a keystroke. Ends with a one-shot end-to-end check that a timer
/// interrupt actually arrives through the trap path, since the feature-less image has no
/// executor to exercise it.
pub(crate) fn interrupts_init() {
    plic::init();
    plic::enable_source(plic::UART0_SOURCE);
    uart::enable_rx_interrupt();

    // `sie` bits: supervisor timer (5) and supervisor external (9) interrupt enables.
    const SIE_STIE_SEIE: u64 = (1 << 5) | (1 << 9);
    // SAFETY: setting interrupt-enable bits and the global SIE flag only enables delivery;
    // the trap vector was installed by the boot stub.
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) SIE_STIE_SEIE, options(nomem, nostack));
        core::arch::asm!("csrsi sstatus, 2", options(nomem, nostack));
    }

    // Prove delivery end to end: arm a 10 ms wake and wait (bounded) for the trap dispatcher
    // to have counted it. A failure here is loud but non-fatal — the rest of boot is still
    // useful for debugging.
    let before = traps::timer_irq_count();
    let start = timer::counter();
    timer::arm_wake(10_000_000);
    let give_up = start + timer::frequency();
    while traps::timer_irq_count() == before && timer::counter() < give_up {
        core::hint::spin_loop();
    }
    if traps::timer_irq_count() > before {
        crate::kprintln!(
            "interrupts: timer interrupt delivered through the trap path after {} us",
            crate::ticks::ticks_to_us(timer::counter() - start, timer::frequency())
        );
    } else {
        crate::kprintln!("interrupts: WARNING: no timer interrupt within 1 s of arming");
    }
}
