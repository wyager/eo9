//! `time.frozen` — both clocks frozen at a configured instant.
//!
//! Targets the `eo9:time/frozen` stub world: exports `eo9:time/time` where `now()` and
//! `monotonic-now()` always answer the instant bound by `configure`, and `sleep` returns
//! immediately. Part of the deterministic environment of integration milestone I2, and an
//! attenuated clock in the Security sense (see SPEC.md): a program composed with
//! `time.frozen` cannot observe the passage of time at all.
//!
//! `resolution()` reports `u64::MAX`: a clock that never advances has no meaningful
//! granularity, so it reports the coarsest possible one.

#![no_std]

extern crate alloc;

use alloc::string::String;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "frozen",
    path: "../../../wit/time",
});

use exports::eo9::time::frozen_config;
use exports::eo9::time::time::{self, Datetime, Instant};
use exports::eo9::time::types;

/// The frozen instant, bound by `configure`.
struct Frozen {
    now_seconds: i64,
    monotonic_ns: u64,
}

static STATE: ProviderState<Frozen> = ProviderState::new();

/// The `time.frozen` provider.
struct Stub;

/// The root-handle resource: a token referring to the configured frozen instant.
struct FrozenTime;

impl types::Guest for Stub {
    type TimeImpl = FrozenTime;
}

impl types::GuestTimeImpl for FrozenTime {}

impl frozen_config::Guest for Stub {
    async fn configure(now_seconds: i64, monotonic_ns: u64) -> Result<types::TimeImpl, String> {
        STATE.set(Frozen {
            now_seconds,
            monotonic_ns,
        });
        Ok(types::TimeImpl::new(FrozenTime))
    }
}

impl time::Guest for Stub {
    fn default() -> types::TimeImpl {
        types::TimeImpl::new(FrozenTime)
    }

    fn now(_t: time::TimeImplBorrow<'_>) -> Datetime {
        STATE.with(|frozen| Datetime {
            seconds: frozen.now_seconds,
            nanoseconds: 0,
        })
    }

    fn monotonic_now(_t: time::TimeImplBorrow<'_>) -> Instant {
        STATE.with(|frozen| Instant {
            nanoseconds: frozen.monotonic_ns,
        })
    }

    fn resolution(_t: time::TimeImplBorrow<'_>) -> u64 {
        u64::MAX
    }

    /// On a frozen clock no time ever elapses, so the wait is over as soon as it starts.
    async fn sleep(_t: time::TimeImplBorrow<'_>, _duration_ns: u64) {}
}

export!(Stub);
