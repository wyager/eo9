//! Minimal SBI client (OpenSBI runs in M-mode underneath this kernel).
//!
//! Only two extensions are used: TIME (`set_timer`, the supervisor timer behind
//! [`super::timer`]) and SRST (`system_reset`, behind [`super::power`]). Calls follow the
//! SBI v0.2+ calling convention: extension id in `a7`, function id in `a6`, arguments in
//! `a0`/`a1`, error/value back in `a0`/`a1`.

/// SBI "TIME" extension id.
const EID_TIME: usize = 0x54494D45;
/// SBI "SRST" (system reset) extension id.
const EID_SRST: usize = 0x53525354;

/// One SBI call with up to two arguments; returns the error code from `a0` (0 = success).
fn call(eid: usize, fid: usize, arg0: usize, arg1: usize) -> isize {
    let error: usize;
    // SAFETY: an `ecall` from S-mode traps into the SBI firmware, which follows the SBI
    // calling convention: it clobbers only a0/a1 (declared as outputs) and returns here.
    unsafe {
        core::arch::asm!(
            "ecall",
            inout("a0") arg0 => error,
            inout("a1") arg1 => _,
            in("a6") fid,
            in("a7") eid,
            options(nostack),
        );
    }
    error as isize
}

/// Program the supervisor timer: the SBI clears the pending supervisor-timer interrupt now
/// and raises it again once the `time` CSR reaches `time_value`. Passing `u64::MAX`
/// effectively cancels the timer (and clears a pending interrupt), which is how
/// [`super::timer::disable`] quiets the line.
pub(super) fn set_timer(time_value: u64) {
    let _ = call(EID_TIME, 0, time_value as usize, 0);
}

/// Ask the SBI to shut the machine down (reset type 0 = shutdown, reason 0 = none). Returns
/// only if the SRST extension is unavailable or refused the request.
pub(super) fn system_reset_shutdown() {
    let _ = call(EID_SRST, 0, 0, 0);
}
