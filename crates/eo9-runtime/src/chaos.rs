//! Seeded chaos injection for the synchronization primitives (test-only).
//!
//! Behind the non-default `chaos` cargo feature, [`point`] inserts seeded, reproducible
//! scheduling perturbations (yields and short sleeps) at the runtime's doorbell and park
//! boundaries — the principled replacement for "run CPU hogs and hope" when hunting
//! host-layer timing bugs (see `docs/spikes/timing-strategies.md`). With the feature off
//! (the default, including every release build) the function is an empty `#[inline]` stub
//! and every call site compiles to nothing; this honors plan/11's ruling against
//! production code carrying interleaving hooks.
//!
//! Reproducibility contract: one global seed (`EO9_CHAOS_SEED`, else generated and
//! printed) derives a per-thread SplitMix64 stream keyed by thread creation order. Same
//! seed ⇒ same per-thread decision streams. With real OS threads this is *statistical*
//! replay, not exact replay: the injected delays repeat, the OS may still vary around
//! them. In practice the injected delays dominate natural jitter by orders of magnitude.

/// A chaos point. `_site` names the boundary for logging/tuning; the site string is a
/// `&'static str` so the disabled stub costs nothing to call.
#[cfg(not(feature = "chaos"))]
#[inline(always)]
pub fn point(_site: &'static str) {}

#[cfg(feature = "chaos")]
pub use enabled::point;

#[cfg(feature = "chaos")]
mod enabled {
    use std::cell::Cell;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// The global seed: `EO9_CHAOS_SEED` if set, else generated from `RandomState` and
    /// printed so a failing run can be replayed.
    fn global_seed() -> u64 {
        static SEED: OnceLock<u64> = OnceLock::new();
        *SEED.get_or_init(|| {
            let seed = std::env::var("EO9_CHAOS_SEED")
                .ok()
                .and_then(|raw| raw.parse::<u64>().ok())
                .unwrap_or_else(|| {
                    use std::hash::{BuildHasher, Hasher};
                    std::collections::hash_map::RandomState::new()
                        .build_hasher()
                        .finish()
                });
            eprintln!("eo9 chaos: seed={seed}");
            seed
        })
    }

    /// Percent of points that sleep (vs. yield or do nothing). Default 10.
    fn sleep_pct() -> u64 {
        static PCT: OnceLock<u64> = OnceLock::new();
        *PCT.get_or_init(|| {
            std::env::var("EO9_CHAOS_SLEEP_PCT")
                .ok()
                .and_then(|raw| raw.parse::<u64>().ok())
                .map_or(10, |pct| pct.min(100))
        })
    }

    fn splitmix(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    thread_local! {
        static STREAM: Cell<u64> = {
            static THREAD_INDEX: AtomicU64 = AtomicU64::new(0);
            let index = THREAD_INDEX.fetch_add(1, Ordering::Relaxed);
            // Distinct, deterministic stream per thread creation order.
            Cell::new(global_seed() ^ index.wrapping_mul(0xA076_1D64_78BD_642F))
        };
    }

    /// Perturb the schedule at `_site`: mostly nothing, sometimes a yield, sometimes a
    /// 10 µs – 3 ms sleep (the range that turns nanosecond windows into hittable ones).
    pub fn point(_site: &'static str) {
        STREAM.with(|cell| {
            let mut state = cell.get();
            let draw = splitmix(&mut state);
            cell.set(state);
            let pct = draw % 100;
            let sleep = sleep_pct();
            if pct < sleep {
                // Log-ish spread: 10µs..~3ms, biased small.
                let micros = 10 + (draw >> 8) % 2990;
                let micros = if draw & 1 == 0 {
                    micros / 8 + 10
                } else {
                    micros
                };
                std::thread::sleep(Duration::from_micros(micros));
            } else if pct < sleep + 20 {
                std::thread::yield_now();
            }
        });
    }
}
