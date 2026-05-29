//! The TSC and the legacy PIT: the x86_64 port's monotonic counter and wake timer.
//!
//! The free-running counter is the TSC (`rdtsc`), whose frequency is measured once at boot
//! against a 20 ms PIT one-shot — the classic calibration, good to a fraction of a percent,
//! which is plenty for `eo9:time`'s monotonic clock and the executor's deadlines. One-shot
//! wake-ups go through PIT channel 0 in mode 0 (interrupt on terminal count, IRQ 0): the
//! x86 equivalent of the aarch64 generic-timer compare and the riscv64 SBI timer. The PIT's
//! 16-bit counter caps a single programming at ~54.9 ms; longer waits simply wake early and
//! the caller re-arms — a harmless spurious wake for the executor, which always re-checks
//! its deadlines. (The LAPIC one-shot timer removes that cap and is the recorded upgrade
//! path once the port grows past the PIC.)

use core::sync::atomic::{AtomicU64, Ordering};

use super::io::{inb, outb};
use super::pic;
use crate::ticks::{ticks_to_ns, ticks_to_us};

/// PIT input clock in Hz.
const PIT_HZ: u64 = 1_193_182;
/// PIT channel 0 data port.
const PIT_CH0: u16 = 0x40;
/// PIT mode/command port.
const PIT_CMD: u16 = 0x43;
/// Command: channel 0, lobyte/hibyte access, mode 0 (interrupt on terminal count), binary.
const PIT_CH0_ONESHOT: u8 = 0x30;
/// Read-back command: latch the status of channel 0.
const PIT_READBACK_STATUS_CH0: u8 = 0xE2;
/// Status bit 7: the state of the OUT pin (high once the one-shot reaches terminal count).
const PIT_STATUS_OUT: u8 = 1 << 7;
/// Longest single PIT one-shot (a full 16-bit count).
const PIT_MAX_TICKS: u64 = 0xFFFF;

/// Calibrated TSC frequency in Hz (0 = not measured yet).
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

/// Read the time-stamp counter.
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `rdtsc` reads the time-stamp counter into edx:eax with no other effects.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Program a PIT channel-0 one-shot of `ticks` PIT cycles (mode 0: OUT drops low, then goes
/// high — raising IRQ 0 — when the count expires).
fn pit_oneshot(ticks: u16) {
    outb(PIT_CMD, PIT_CH0_ONESHOT);
    outb(PIT_CH0, (ticks & 0xFF) as u8);
    outb(PIT_CH0, (ticks >> 8) as u8);
}

/// Whether the current PIT one-shot has expired (OUT pin high), via the read-back command.
fn pit_expired() -> bool {
    outb(PIT_CMD, PIT_READBACK_STATUS_CH0);
    inb(PIT_CH0) & PIT_STATUS_OUT != 0
}

/// Counter frequency in Hz (the calibrated TSC rate). The first call runs the 20 ms PIT
/// calibration; later calls return the cached value.
pub fn frequency() -> u64 {
    let cached = TSC_HZ.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    // 20 ms calibration window: PIT_HZ / 50 = 23_863 ticks, comfortably inside 16 bits.
    let window_ticks = (PIT_HZ / 50) as u16;
    let start = rdtsc();
    pit_oneshot(window_ticks);
    while !pit_expired() {
        core::hint::spin_loop();
    }
    let elapsed = rdtsc().wrapping_sub(start);
    let measured = (elapsed * 50).max(1);
    TSC_HZ.store(measured, Ordering::Relaxed);
    measured
}

/// Current value of the free-running counter (the TSC).
pub fn counter() -> u64 {
    rdtsc()
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

/// Quiet the wake timer: mask IRQ 0 until the next [`arm_wake`]. The trap handler calls
/// this when the timer fires so a stale one-shot cannot interrupt again; the executor
/// re-arms before its next halt.
pub fn disable() {
    pic::set_masked(pic::IRQ_TIMER, true);
}

/// Arm the wake timer to raise IRQ 0 `delay_ns` from now (clamped to the PIT's ~54.9 ms
/// maximum single shot — a longer wait wakes early and the caller re-arms), so it can wake
/// the executor's idle halt.
#[allow(dead_code)] // wasm executor idle path only; not the feature-less CI build
pub fn arm_wake(delay_ns: u64) {
    let ticks = (u128::from(delay_ns) * u128::from(PIT_HZ) / 1_000_000_000) as u64;
    let ticks = ticks.clamp(1, PIT_MAX_TICKS) as u16;
    pit_oneshot(ticks);
    pic::set_masked(pic::IRQ_TIMER, false);
}

/// Mask interrupt delivery, arm the timer to fire `delay_ns` from now, halt until an
/// interrupt arrives, then continue with delivery enabled (taking the interrupt, which the
/// trap dispatcher services). The `sti; hlt` pair is x86's lost-wakeup-free idle: `sti`
/// only takes effect after the following instruction, so an interrupt that became pending
/// while delivery was masked wakes the `hlt` rather than slipping in front of it.
///
/// The `sti; hlt` asm deliberately omits `nomem`: the interrupt taken at the halt writes
/// memory the caller re-reads right afterwards (the UART input ring), so this sequence must
/// be a compiler-level memory barrier rather than relying on call/inlining boundaries to
/// keep those reads from being cached across the halt.
#[allow(dead_code)] // wasm executor idle path only; not the feature-less CI build
pub fn wait_for_interrupt(delay_ns: u64) {
    // SAFETY: masking delivery, programming the wake timer, and halting do not touch the
    // stack or clobber registers the compiler relies on; the missing `nomem` on the
    // halt/unmask pair is the memory clobber discussed above.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        arm_wake(delay_ns);
        core::arch::asm!("sti", "hlt", options(nostack, preserves_flags));
    }
}

/// Print counter readings and run a polled 10 ms timer-condition check (the PIT one-shot's
/// OUT pin, observed with interrupt delivery still disabled this early in boot).
pub fn self_test() {
    let frequency = frequency();
    let first = counter();
    let second = counter();
    crate::kprintln!(
        "time counter: advancing ({first} -> {second}), resolution {} ns, uptime {} us",
        resolution_ns(),
        ticks_to_us(second, frequency)
    );

    // Program the PIT 10 ms ahead and poll its OUT pin; give up loudly after one second
    // rather than hanging boot if the device misbehaves.
    let start = counter();
    pit_oneshot((PIT_HZ / 100) as u16);
    let give_up = start + frequency;
    let mut asserted = false;
    while counter() < give_up {
        if pit_expired() {
            asserted = true;
            break;
        }
        core::hint::spin_loop();
    }
    let elapsed = counter() - start;
    if asserted {
        crate::kprintln!(
            "pit timer: 10 ms timer condition asserted after {} us (polled OUT)",
            ticks_to_us(elapsed, frequency)
        );
    } else {
        crate::kprintln!("pit timer: WARNING: 10 ms timer condition not seen within 1 s");
    }
}
