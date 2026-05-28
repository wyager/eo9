//! `time.monotonic-stub` — a deterministic stand-in clock.
//!
//! Targets the `eo9:time/monotonic-stub` stub world: exports `eo9:time/time` as a
//! counter-driven clock. `configure` binds the start value and the step; every
//! observation of the clock (`now` or `monotonic-now`) answers the current counter and
//! then advances it by the step, and `sleep` advances the counter by the requested
//! duration and returns. Runs against this clock are therefore a pure function of the
//! program's own behaviour — the deterministic stand-in the spec calls for
//! (see SPEC.md, "Time API" and Security).
//!
//! Concretely, the counter is a number of nanoseconds: `monotonic-now` reports it as an
//! instant, and `now` reports it as a wall-clock datetime measured from the Unix epoch
//! (so wall time advances in lockstep from the configured start). `resolution()` reports
//! the configured step and does not advance the counter. The counter saturates instead
//! of wrapping, keeping the clock monotonic even at the extreme.

#![no_std]

extern crate alloc;

use alloc::string::String;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "monotonic-stub",
    path: "../../../wit/time",
});

use exports::eo9::time::monotonic_stub_config;
use exports::eo9::time::time::{self, Datetime, Instant};
use exports::eo9::time::types;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The clock state: the current counter (nanoseconds) and the per-observation step.
struct Clock {
    counter_ns: u64,
    step_ns: u64,
}

impl Clock {
    /// Answer the current counter value, then advance by the configured step.
    fn observe(&mut self) -> u64 {
        let observed = self.counter_ns;
        self.counter_ns = self.counter_ns.saturating_add(self.step_ns);
        observed
    }
}

static STATE: ProviderState<Clock> = ProviderState::new();

/// The `time.monotonic-stub` provider.
struct Stub;

/// The root-handle resource: a token referring to the configured clock state.
struct StubClock;

impl types::Guest for Stub {
    type TimeImpl = StubClock;
}

impl types::GuestTimeImpl for StubClock {}

impl monotonic_stub_config::Guest for Stub {
    fn configure(start_ns: u64, step_ns: u64) -> Result<types::TimeImpl, String> {
        STATE.set(Clock {
            counter_ns: start_ns,
            step_ns,
        });
        Ok(types::TimeImpl::new(StubClock))
    }
}

impl time::Guest for Stub {
    fn default() -> types::TimeImpl {
        types::TimeImpl::new(StubClock)
    }

    fn now(_t: time::TimeImplBorrow<'_>) -> Datetime {
        let observed = STATE.with(Clock::observe);
        Datetime {
            seconds: (observed / NANOS_PER_SECOND) as i64,
            nanoseconds: (observed % NANOS_PER_SECOND) as u32,
        }
    }

    fn monotonic_now(_t: time::TimeImplBorrow<'_>) -> Instant {
        Instant {
            nanoseconds: STATE.with(Clock::observe),
        }
    }

    fn resolution(_t: time::TimeImplBorrow<'_>) -> u64 {
        STATE.with(|clock| clock.step_ns)
    }

    /// Advance the counter by the requested duration and return: on the stand-in clock,
    /// sleeping *is* what makes time pass.
    async fn sleep(_t: time::TimeImplBorrow<'_>, duration_ns: u64) {
        STATE.with(|clock| {
            clock.counter_ns = clock.counter_ns.saturating_add(duration_ns);
        });
    }
}

export!(Stub);
