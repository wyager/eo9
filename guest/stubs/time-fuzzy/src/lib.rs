//! `time.fuzzy` — an underlying clock with degraded resolution.
//!
//! Targets the `eo9:time/fuzzy` stub world: imports `eo9:time/time` and re-exports it
//! with every reading quantized to the granularity bound by `configure` — the
//! Spectre-class side-channel mitigation of SPEC.md's Security section (attenuating the
//! attacker's clock is just provider substitution). The root handle is shared with the
//! underlying provider: `configure` and `default()` hand out the underlying clock's own
//! handle, and every operation forwards the borrow it was given.
//!
//! Quantization is field-wise and floors toward minus infinity: a granularity below one
//! second truncates the nanosecond field to a multiple of the granularity; a granularity
//! of one second or more zeroes the nanoseconds and truncates the seconds to a multiple
//! of the whole-second part of the granularity. `resolution()` reports the coarser of
//! the underlying resolution and the configured granularity, and `sleep` rounds the
//! requested duration *up* to the next multiple of the granularity before forwarding, so
//! sleep completions cannot be used as a finer timer than the clock itself.

#![no_std]

extern crate alloc;

use alloc::string::String;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "fuzzy",
    path: "../../../wit/time",
});

use eo9::time::time as underlying;
use eo9::time::types::TimeImpl;
use exports::eo9::time::fuzzy_config;
use exports::eo9::time::time::{self, Datetime, Instant};

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The configured granularity in nanoseconds (always at least 1).
static GRANULARITY_NS: ProviderState<u64> = ProviderState::new();

/// Floor `datetime` to the configured granularity (see the crate docs for the
/// field-wise rule).
fn quantize_datetime(datetime: Datetime, granularity_ns: u64) -> Datetime {
    if granularity_ns >= NANOS_PER_SECOND {
        let granularity_s = (granularity_ns / NANOS_PER_SECOND) as i64;
        Datetime {
            seconds: datetime.seconds - datetime.seconds.rem_euclid(granularity_s),
            nanoseconds: 0,
        }
    } else {
        Datetime {
            seconds: datetime.seconds,
            nanoseconds: datetime.nanoseconds - datetime.nanoseconds % (granularity_ns as u32),
        }
    }
}

/// The `time.fuzzy` provider.
struct Stub;

impl fuzzy_config::Guest for Stub {
    async fn configure(granularity_ns: u64) -> Result<TimeImpl, String> {
        if granularity_ns == 0 {
            return Err(String::from("granularity-ns must be at least 1"));
        }
        GRANULARITY_NS.set(granularity_ns);
        Ok(underlying::default())
    }
}

impl time::Guest for Stub {
    fn default() -> TimeImpl {
        underlying::default()
    }

    fn now(t: &TimeImpl) -> Datetime {
        let granularity = GRANULARITY_NS.with(|g| *g);
        // The imported and exported interfaces have structurally identical but distinct
        // generated record types, hence the field-wise conversion.
        let underlying = underlying::now(t);
        quantize_datetime(
            Datetime {
                seconds: underlying.seconds,
                nanoseconds: underlying.nanoseconds,
            },
            granularity,
        )
    }

    fn monotonic_now(t: &TimeImpl) -> Instant {
        let granularity = GRANULARITY_NS.with(|g| *g);
        let instant = underlying::monotonic_now(t);
        Instant {
            nanoseconds: instant.nanoseconds - instant.nanoseconds % granularity,
        }
    }

    fn resolution(t: &TimeImpl) -> u64 {
        let granularity = GRANULARITY_NS.with(|g| *g);
        u64::max(underlying::resolution(t), granularity)
    }

    async fn sleep(t: &TimeImpl, duration_ns: u64) {
        let granularity = GRANULARITY_NS.with(|g| *g);
        let rounded_up = duration_ns
            .div_ceil(granularity)
            .saturating_mul(granularity);
        underlying::sleep(t, rounded_up).await;
    }
}

export!(Stub);
