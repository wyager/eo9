//! The ARM generic timer (polled).
//!
//! Spike step 2 of the ladder: prove the counter is readable and that the EL1 physical
//! timer's compare condition fires. Interrupt delivery (GIC bring-up) is deliberately left
//! for the scheduler milestone — the spike polls `CNTP_CTL_EL0.ISTATUS` instead, which
//! exercises the same timer hardware without needing the interrupt controller.

use crate::ticks::{ticks_to_ns, ticks_to_us};

/// Counter-timer frequency in Hz (CNTFRQ_EL0, set by QEMU).
pub fn frequency() -> u64 {
    let frequency: u64;
    // SAFETY: reading CNTFRQ_EL0 has no side effects.
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack)) };
    frequency
}

/// Current physical counter value (CNTPCT_EL0), with an ISB so the read is not hoisted.
pub fn counter() -> u64 {
    let count: u64;
    // SAFETY: an ISB plus a read of CNTPCT_EL0 has no side effects.
    unsafe {
        core::arch::asm!("isb", "mrs {}, cntpct_el0", out(reg) count, options(nomem, nostack));
    }
    count
}

/// Microseconds since the counter started (effectively: since the machine powered on).
pub fn uptime_us() -> u64 {
    uptime_ns() / 1_000
}

/// Nanoseconds since the counter started; the kernel's monotonic clock.
pub fn uptime_ns() -> u64 {
    ticks_to_ns(counter(), frequency())
}

/// Nanoseconds into the current second, for the sub-second part of the wall clock.
pub fn subsecond_ns() -> u32 {
    let frequency = frequency();
    if frequency == 0 {
        return 0;
    }
    ticks_to_ns(counter() % frequency, frequency) as u32
}

/// Disable the EL1 physical timer (clear ENABLE), deasserting its interrupt. The IRQ handler
/// calls this so the level-sensitive timer line drops before the EOI; the executor re-arms it
/// with [`arm_wake`] before the next `wfi`.
#[cfg(target_os = "none")]
pub fn disable() {
    // SAFETY: writing the EL1 physical timer control register at EL1 only affects that timer.
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) 0_u64, options(nomem, nostack));
    }
}

/// Nominal counter resolution in nanoseconds (at least 1).
pub fn resolution_ns() -> u64 {
    let frequency = frequency();
    if frequency == 0 {
        return 1;
    }
    u64::max(1, 1_000_000_000 / frequency)
}

/// Arm the EL1 physical timer to assert its interrupt `delay_ns` from now, *unmasked* so it
/// reaches the GIC and can wake a `wfi`. The kernel executor's idle path arms a short wake,
/// executes `wfi`, and re-arms on the next loop — which, the generic-timer PPI being
/// level-sensitive, deasserts the previous signal and clears the GIC pending state with no
/// EOI needed (the interrupt is never taken as an exception; PSTATE.I stays masked).
// Used only by the wasm executor's idle path (src/wasm/mod.rs), which the feature-less CI
// kernel build does not compile; keep it unconditional like the rest of the timer MMIO.
#[cfg(target_os = "none")]
#[allow(dead_code)]
pub fn arm_wake(delay_ns: u64) {
    let frequency = frequency();
    if frequency == 0 {
        return;
    }
    // ticks = delay_ns * frequency / 1e9, at least one, clamped to the 32-bit TVAL counter.
    let ticks = (u128::from(delay_ns) * u128::from(frequency) / 1_000_000_000) as u64;
    let tval = u64::from(u32::try_from(ticks.max(1)).unwrap_or(u32::MAX));
    // ENABLE = 1, IMASK = 0 (assert to the GIC).
    // SAFETY: programming the EL1 physical timer at EL1 only affects that timer.
    unsafe {
        core::arch::asm!(
            "msr cntp_tval_el0, {tval}",
            "msr cntp_ctl_el0, {ctl}",
            tval = in(reg) tval,
            ctl = in(reg) 0b01_u64,
            options(nomem, nostack),
        );
    }
}

/// Print counter readings and run a polled 10 ms timer-condition check.
pub fn self_test() {
    let frequency = frequency();
    let first = counter();
    let second = counter();
    crate::kprintln!(
        "generic timer: counter advancing ({first} -> {second}), resolution {} ns, uptime {} us",
        resolution_ns(),
        ticks_to_us(second, frequency)
    );

    // Program the EL1 physical timer 10 ms ahead with its interrupt masked, then poll the
    // ISTATUS bit. ENABLE = 1, IMASK = 1.
    let programmed_ticks = frequency / 100;
    let start = counter();
    // SAFETY: writing the EL1 physical timer's compare/control registers at EL1 is
    // architecturally permitted and only affects the (masked) timer.
    unsafe {
        core::arch::asm!(
            "msr cntp_tval_el0, {ticks}",
            "msr cntp_ctl_el0, {ctl}",
            ticks = in(reg) programmed_ticks,
            ctl = in(reg) 0b11_u64,
            options(nomem, nostack),
        );
    }
    loop {
        let control: u64;
        // SAFETY: reading the timer control register has no side effects.
        unsafe {
            core::arch::asm!("mrs {}, cntp_ctl_el0", out(reg) control, options(nomem, nostack));
        }
        if control & (1 << 2) != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    let elapsed = counter() - start;
    // SAFETY: disabling the timer again.
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) 0_u64, options(nomem, nostack));
    }
    crate::kprintln!(
        "generic timer: 10 ms timer condition asserted after {} us (polled ISTATUS)",
        ticks_to_us(elapsed, frequency)
    );
}
