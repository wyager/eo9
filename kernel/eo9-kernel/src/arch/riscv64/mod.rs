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
