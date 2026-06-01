//! `restart.backoff` — the exponential-backoff restart policy.
//!
//! Targets the `eo9:svc/restart-backoff` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:svc/restart-policy` plus its
//! compose-time configuration. Restart number `n` (1-based) is delayed by
//! `base-delay-ms * 2^(n-1)`, capped at one hour; once `max-restarts` restarts have been
//! performed the policy gives up.
//!
//!   detach worker = cruncher --rounds 50 restart (restart.backoff --max-restarts 5 --base-delay-ms 200)
//!
//! * Unconfigured, the policy **gives up immediately** (the deny-all posture: plain
//!   composition never silently retries forever).
//! * The component imports nothing — `describe` shows an empty capability surface — so
//!   it provably cannot do anything but compute its answer.

#![no_std]

extern crate alloc;

use alloc::string::String;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "restart-backoff",
    path: "../../../wit/svc",
    generate_all,
});

use exports::eo9::svc::backoff_config;
use exports::eo9::svc::restart_policy::{self, FailureHistory, RestartAction};

/// One hour, the ceiling on any single delay.
const MAX_DELAY_MS: u64 = 60 * 60 * 1000;

/// The configured budget: (max restarts, base delay in milliseconds).
static CONFIG: ProviderState<(u32, u64)> = ProviderState::new();

/// The `restart.backoff` policy.
struct Stub;

impl backoff_config::Guest for Stub {
    fn configure(max_restarts: u32, base_delay_ms: u64) -> Result<(), String> {
        CONFIG.set((max_restarts, base_delay_ms));
        Ok(())
    }
}

impl restart_policy::Guest for Stub {
    fn decide(history: FailureHistory) -> RestartAction {
        if !CONFIG.is_set() {
            // Unconfigured: never retry (deny-all posture).
            return RestartAction::GiveUp;
        }
        CONFIG.with(|(max_restarts, base_delay_ms)| {
            if history.total_restarts >= *max_restarts {
                return RestartAction::GiveUp;
            }
            // This will be restart number `total_restarts + 1`; its delay doubles each
            // time: base * 2^total_restarts, capped. The shift is clamped so huge
            // budgets cannot overflow.
            let exponent = history.total_restarts.min(22);
            let delay = base_delay_ms
                .saturating_mul(1u64 << exponent)
                .min(MAX_DELAY_MS);
            RestartAction::RestartAfterMs(delay)
        })
    }
}

export!(Stub);
