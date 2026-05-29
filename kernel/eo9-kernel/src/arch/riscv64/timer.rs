//! The RISC-V time counter and the SBI supervisor timer.
//!
//! The `time` CSR is the free-running counter (readable from S-mode because OpenSBI sets
//! `mcounteren.TM`); one-shot wake-ups go through the SBI TIME extension, which programs the
//! machine timer underneath and raises the supervisor timer interrupt (`sip.STIP`) when the
//! counter reaches the requested value — the riscv64 equivalent of the aarch64 generic
//! timer's compare condition.

use crate::ticks::{ticks_to_ns, ticks_to_us};

/// Counter frequency in Hz. QEMU's `virt` machine fixes its timebase at 10 MHz
/// (`/cpus/timebase-frequency` in its device tree); like the RAM size, the kernel assumes
/// xtask's QEMU invocation rather than parsing it back out of the FDT.
const TIMEBASE_FREQUENCY_HZ: u64 = 10_000_000;

/// `sip`/`sie` bit for the supervisor timer interrupt.
const STIP: u64 = 1 << 5;

/// Counter frequency in Hz.
pub fn frequency() -> u64 {
    TIMEBASE_FREQUENCY_HZ
}

/// Current value of the free-running `time` CSR.
pub fn counter() -> u64 {
    let count: u64;
    // SAFETY: reading the `time` CSR has no side effects.
    unsafe { core::arch::asm!("rdtime {}", out(reg) count, options(nomem, nostack)) };
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

/// Nominal counter resolution in nanoseconds (at least 1).
pub fn resolution_ns() -> u64 {
    let frequency = frequency();
    if frequency == 0 {
        return 1;
    }
    u64::max(1, 1_000_000_000 / frequency)
}

/// Cancel the supervisor timer (program it effectively-never), clearing its pending
/// interrupt. The trap handler calls this so the timer line drops before `sret`; the
/// executor re-arms it with [`arm_wake`] before the next halt.
pub fn disable() {
    super::sbi::set_timer(u64::MAX);
}

/// Arm the supervisor timer to raise its interrupt `delay_ns` from now (at least one tick),
/// so it can wake the executor's idle halt or be observed in `sip.STIP`.
#[allow(dead_code)] // wasm executor idle path only; not the feature-less CI build
pub fn arm_wake(delay_ns: u64) {
    let frequency = frequency();
    if frequency == 0 {
        return;
    }
    let ticks = (u128::from(delay_ns) * u128::from(frequency) / 1_000_000_000) as u64;
    super::sbi::set_timer(counter().saturating_add(ticks.max(1)));
}

/// Mask interrupt delivery, arm the timer to fire `delay_ns` from now, halt in `wfi` until
/// an interrupt is pending, then unmask (taking the interrupt, which the trap dispatcher
/// services). A pending-and-enabled interrupt wakes `wfi` even while `sstatus.SIE` is clear,
/// so an interrupt that lands between the caller's last poll and the halt is not lost —
/// masking only closes the lost-wakeup window.
///
/// The `wfi`/unmask asm deliberately omits `nomem`: the interrupt taken at the unmask writes
/// memory the caller re-reads right afterwards (the UART input ring), so this sequence must
/// be a compiler-level memory barrier rather than relying on call/inlining boundaries to
/// keep those reads from being cached across the halt.
#[allow(dead_code)] // wasm executor idle path only; not the feature-less CI build
pub fn wait_for_interrupt(delay_ns: u64) {
    // SAFETY: clearing/setting sstatus.SIE and halting do not touch the stack or clobber
    // registers the compiler relies on; the missing `nomem` is the memory clobber discussed
    // above.
    unsafe {
        core::arch::asm!("csrci sstatus, 2", options(nomem, nostack, preserves_flags));
        arm_wake(delay_ns);
        core::arch::asm!("wfi", options(nostack, preserves_flags));
        core::arch::asm!("csrsi sstatus, 2", options(nostack, preserves_flags));
    }
}

/// Print counter readings and run a polled 10 ms timer-condition check (the supervisor-timer
/// pending bit `sip.STIP`, observed with delivery still disabled this early in boot).
pub fn self_test() {
    let frequency = frequency();
    let first = counter();
    let second = counter();
    crate::kprintln!(
        "time counter: advancing ({first} -> {second}), resolution {} ns, uptime {} us",
        resolution_ns(),
        ticks_to_us(second, frequency)
    );

    // Program the SBI timer 10 ms ahead and poll the pending bit; give up loudly after one
    // second rather than hanging boot if the SBI implementation misbehaves.
    let start = counter();
    super::sbi::set_timer(start + frequency / 100);
    let give_up = start + frequency;
    let mut asserted = false;
    while counter() < give_up {
        let sip: u64;
        // SAFETY: reading the `sip` CSR has no side effects.
        unsafe { core::arch::asm!("csrr {}, sip", out(reg) sip, options(nomem, nostack)) };
        if sip & STIP != 0 {
            asserted = true;
            break;
        }
        core::hint::spin_loop();
    }
    let elapsed = counter() - start;
    disable();
    if asserted {
        crate::kprintln!(
            "sbi timer: 10 ms timer condition asserted after {} us (polled sip.STIP)",
            ticks_to_us(elapsed, frequency)
        );
    } else {
        crate::kprintln!("sbi timer: WARNING: 10 ms timer condition not seen within 1 s");
    }
}
