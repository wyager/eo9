//! x86_64 on QEMU `q35`: PVH direct boot into long mode, COM1 16550 UART, CMOS RTC, the
//! TSC + PIT for the monotonic counter and wake timer, and the legacy 8259 PIC for
//! timer/UART interrupt delivery. The port follows the aarch64 reference layer
//! (src/arch/aarch64/) module for module; `mmu` replaces the boot stub's 2 MiB RWX identity
//! map with 4 KiB-granular tables (NXE + WP) so published JIT code gets the same W^X
//! treatment as on the other ports.

mod boot;
mod io;
mod pic;
mod traps;

pub(crate) mod mmu;
pub(crate) mod power;
pub(crate) mod rtc;
pub(crate) mod timer;
pub(crate) mod uart;

/// Architecture name as spelled in `cargo xtask build-kernel <arch>` / `cargo xtask qemu <arch>`.
pub(crate) const NAME: &str = "x86_64";

/// Where PCI Express lives on this machine (QEMU `q35`): the MMCONFIG/ECAM window at its
/// default base and the 32-bit PCI hole below it for BAR placement. The shared `src/pci.rs`
/// compiles against these so wasm-store builds link, but the x86_64 QEMU invocation does
/// not wire the `pci` boot grant yet — these values are documented, not verified
/// (plan/12-kernel.md Decisions).
#[cfg(feature = "wasm-store")]
pub(crate) mod pci_map {
    /// ECAM (PCIe configuration space) base (q35 MMCONFIG default).
    pub(crate) const ECAM_BASE: usize = 0xb000_0000;
    /// Buses walked (the window covers 256; bus 0 is all QEMU populates by default).
    pub(crate) const ECAM_BUSES: u8 = 16;
    /// 32-bit PCI hole: where unassigned memory BARs would be placed.
    pub(crate) const MMIO_BASE: usize = 0x8000_0000;
    pub(crate) const MMIO_END: usize = 0xb000_0000;
}

/// Boot banner: machine identification, privilege level, timer frequency, wall clock.
pub(crate) fn banner() {
    crate::kprintln!();
    crate::kprintln!("Eo9 kernel — x86_64 (QEMU q35)");
    crate::kprintln!("  privilege: long mode, ring 0 (PVH direct boot)");
    crate::kprintln!(
        "  time counter frequency: {} Hz (TSC, PIT-calibrated)",
        timer::frequency()
    );
    crate::kprintln!(
        "  wall clock: {}.{:09} s since the Unix epoch (CMOS RTC + TSC)",
        rtc::seconds(),
        timer::subsecond_ns()
    );
}

/// Interrupt delivery: remap the 8259 PICs away from the exception vectors (the IDT itself
/// was installed by the boot path), route the COM1 receive line (IRQ 4), and enable
/// delivery (`sti`) — so the executor can halt in `hlt` and be woken either by a timer
/// deadline or by a keystroke. The wake-timer line (IRQ 0) stays masked until
/// `timer::arm_wake` programs a one-shot. Ends with a one-shot end-to-end check that a
/// timer interrupt actually arrives through the trap path, since the feature-less image has
/// no executor to exercise it.
pub(crate) fn interrupts_init() {
    pic::init();
    pic::set_masked(pic::IRQ_COM1, false);
    uart::enable_rx_interrupt();
    // SAFETY: enabling delivery only lets the lines unmasked above through; the IDT and the
    // PIC remap are in place.
    unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };

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
