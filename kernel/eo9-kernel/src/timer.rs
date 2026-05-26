//! The ARM generic timer (polled).
//!
//! Spike step 2 of the ladder: prove the counter is readable and that the EL1 physical
//! timer's compare condition fires. Interrupt delivery (GIC bring-up) is deliberately left
//! for the scheduler milestone — the spike polls `CNTP_CTL_EL0.ISTATUS` instead, which
//! exercises the same timer hardware without needing the interrupt controller.

use crate::ticks::ticks_to_us;

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
    ticks_to_us(counter(), frequency())
}

/// Print counter readings and run a polled 10 ms timer-condition check.
pub fn self_test() {
    let frequency = frequency();
    let first = counter();
    let second = counter();
    crate::kprintln!(
        "generic timer: counter advancing ({first} -> {second}), uptime {} us",
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
