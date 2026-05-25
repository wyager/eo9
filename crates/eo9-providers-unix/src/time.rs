//! Root provider for `eo9:time` — wall-clock and monotonic time from the host OS.
//!
//! `now`, `monotonic-now`, and `resolution` are synchronous (matching the WIT
//! signatures); `sleep` completes asynchronously through a [`Completer`].
//!
//! Sleeps are serviced by one dedicated timer thread per provider (a deadline min-heap
//! plus a condvar), not by the shared blocking pool, so thousands of concurrent sleeps
//! cost one thread rather than one pool slot each.
//!
//! Kill behavior: a pending `sleep` always fires at (or after) its deadline — including
//! after the provider itself has been dropped — and the completion is simply dropped if
//! nobody is listening. Nothing is ever aborted early, so the "at least `duration-ns`
//! elapsed" contract holds for every completion that is observed.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

use crate::completion::Completer;

/// A point on the monotonic clock: nanoseconds since an arbitrary per-provider epoch
/// (WIT `instant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    /// Nanoseconds since the provider's epoch.
    pub nanoseconds: u64,
}

/// Wall-clock time: seconds and nanoseconds since the Unix epoch, UTC (WIT `datetime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datetime {
    /// Whole seconds since the Unix epoch (negative for instants before it).
    pub seconds: i64,
    /// Nanoseconds within the second, always in `0..1_000_000_000`.
    pub nanoseconds: u32,
}

/// The host trait mirroring the WIT `eo9:time/time` interface (minus `default`).
pub trait TimeHost: Send + Sync {
    /// Current wall-clock time.
    fn now(&self) -> Datetime;
    /// Current monotonic time.
    fn monotonic_now(&self) -> Instant;
    /// Granularity of this clock in nanoseconds.
    fn resolution(&self) -> u64;
    /// Completes once at least `duration_ns` nanoseconds of monotonic time have elapsed.
    fn sleep(&self, duration_ns: u64, complete: Completer<()>);
}

/// The unix time provider. Corresponds to the WIT `time-impl` root handle.
pub struct TimeProvider {
    /// Epoch of the monotonic clock: provider construction time.
    epoch: StdInstant,
    timer: Arc<TimerShared>,
}

impl TimeProvider {
    /// A provider reading the host's real clocks.
    pub fn new() -> Self {
        let timer = Arc::new(TimerShared {
            state: Mutex::new(TimerState {
                queue: BinaryHeap::new(),
                next_seq: 0,
                shutdown: false,
            }),
            wake: Condvar::new(),
        });
        let thread_timer = Arc::clone(&timer);
        // Detached on purpose: pending sleeps keep firing at their deadlines even after
        // the provider is dropped; the thread exits once shutdown is requested and the
        // queue has drained.
        thread::Builder::new()
            .name("eo9-timer".to_owned())
            .spawn(move || timer_loop(&thread_timer))
            .expect("failed to spawn timer thread");
        Self {
            epoch: StdInstant::now(),
            timer,
        }
    }
}

impl Default for TimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TimeProvider {
    fn drop(&mut self) {
        let mut state = self.timer.lock_state();
        state.shutdown = true;
        drop(state);
        self.timer.wake.notify_one();
    }
}

impl TimeHost for TimeProvider {
    fn now(&self) -> Datetime {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => Datetime {
                seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                nanoseconds: since.subsec_nanos(),
            },
            // Host clock set before 1970: count backwards, keeping nanoseconds positive
            // within the (earlier) second.
            Err(err) => {
                let before = err.duration();
                let mut seconds = -i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
                let mut nanoseconds = 0;
                if before.subsec_nanos() > 0 {
                    seconds -= 1;
                    nanoseconds = 1_000_000_000 - before.subsec_nanos();
                }
                Datetime {
                    seconds,
                    nanoseconds,
                }
            }
        }
    }

    fn monotonic_now(&self) -> Instant {
        Instant {
            nanoseconds: u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX),
        }
    }

    fn resolution(&self) -> u64 {
        // The API reports time in nanoseconds; the host's std clocks are nanosecond
        // denominated. The true hardware granularity may be coarser, but this provider
        // does not degrade it further (time.fuzzy is the place for that).
        1
    }

    fn sleep(&self, duration_ns: u64, complete: Completer<()>) {
        let deadline = StdInstant::now() + Duration::from_nanos(duration_ns);
        let mut state = self.timer.lock_state();
        let seq = state.next_seq;
        state.next_seq += 1;
        state.queue.push(TimerEntry {
            deadline,
            seq,
            complete,
        });
        drop(state);
        self.timer.wake.notify_one();
    }
}

struct TimerShared {
    state: Mutex<TimerState>,
    wake: Condvar,
}

impl TimerShared {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, TimerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct TimerState {
    queue: BinaryHeap<TimerEntry>,
    next_seq: u64,
    shutdown: bool,
}

struct TimerEntry {
    deadline: StdInstant,
    seq: u64,
    complete: Completer<()>,
}

// Ordering ignores the completer: earliest deadline first (BinaryHeap is a max-heap, so
// the comparison is reversed), ties broken by submission order.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

fn timer_loop(shared: &TimerShared) {
    let mut state = shared.lock_state();
    loop {
        let now = StdInstant::now();
        if let Some(next) = state.queue.peek() {
            if next.deadline <= now {
                let entry = state.queue.pop().expect("peeked entry exists");
                drop(state);
                (entry.complete)(());
                state = shared.lock_state();
            } else {
                let wait = next.deadline - now;
                state = shared
                    .wake
                    .wait_timeout(state, wait)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
        } else if state.shutdown {
            return;
        } else {
            state = shared
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::completer;
    use std::sync::mpsc;

    #[test]
    fn wall_clock_tracks_system_time() {
        let provider = TimeProvider::new();
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let now = provider.now();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(now.seconds >= before - 1 && now.seconds <= after + 1);
        assert!(now.nanoseconds < 1_000_000_000);
    }

    #[test]
    fn monotonic_clock_never_goes_backwards() {
        let provider = TimeProvider::new();
        let mut previous = provider.monotonic_now();
        for _ in 0..1000 {
            let next = provider.monotonic_now();
            assert!(next >= previous);
            previous = next;
        }
    }

    #[test]
    fn resolution_is_reported_in_nanoseconds() {
        assert_eq!(TimeProvider::new().resolution(), 1);
    }

    #[test]
    fn sleep_completes_after_the_requested_duration() {
        let provider = TimeProvider::new();
        let start = provider.monotonic_now();
        let (tx, rx) = mpsc::channel();
        provider.sleep(5_000_000, completer(move |()| tx.send(()).unwrap()));
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let elapsed = provider.monotonic_now().nanoseconds - start.nanoseconds;
        assert!(elapsed >= 5_000_000, "slept only {elapsed}ns");
    }

    #[test]
    fn sleeps_complete_in_deadline_order_even_when_submitted_out_of_order() {
        let provider = TimeProvider::new();
        let (tx, rx) = mpsc::channel();
        for (label, duration_ns) in [("long", 40_000_000u64), ("short", 5_000_000)] {
            let tx = tx.clone();
            provider.sleep(duration_ns, completer(move |()| tx.send(label).unwrap()));
        }
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "short");
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "long");
    }

    #[test]
    fn pending_sleeps_fire_even_after_the_provider_is_dropped() {
        let provider = TimeProvider::new();
        let (tx, rx) = mpsc::channel();
        provider.sleep(5_000_000, completer(move |()| tx.send(()).unwrap()));
        drop(provider);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}
