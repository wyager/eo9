//! Kill/linearity suite (plan/13-tests.md milestone 1): SPEC "Kill and linearity" — a
//! killed task never observes anything again, anything it transferred away belongs to the
//! transferee, and nothing dangles or leaks: the in-flight provider operation (and the
//! buffer it holds) is dropped with the task, and the provider's backend may still
//! complete the underlying work afterwards, quietly.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use eo9_integration::{fixtures, run};
use eo9_runtime::providers::{BoxOp, Datetime, TimeProvider};
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{Outcome, Providers, ResumeOutcome, SpawnLimits, Task};

/// Shared instrumentation between the test and the tracked provider: how many "buffers"
/// are currently held by in-flight operations, whether the in-flight operation has been
/// dropped, whether the backend completed it, and the waker the runtime handed the
/// operation (the task's doorbell).
#[derive(Default)]
struct Probe {
    buffers_live: AtomicUsize,
    op_dropped: AtomicBool,
    completed: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

/// Stands in for an owned I/O buffer transferred to the provider for the life of the
/// operation: created when the operation starts, released when the operation is dropped
/// or completes — if one is still live after a kill, something leaked.
struct TrackedBuffer {
    probe: Arc<Probe>,
}

impl TrackedBuffer {
    fn new(probe: Arc<Probe>) -> Self {
        probe.buffers_live.fetch_add(1, Ordering::SeqCst);
        Self { probe }
    }
}

impl Drop for TrackedBuffer {
    fn drop(&mut self) {
        self.probe.buffers_live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The pending provider operation: holds the tracked buffer, never resolves on its own,
/// and records the waker it is polled with so the backend can complete it later.
struct TrackedSleep {
    probe: Arc<Probe>,
    _buffer: TrackedBuffer,
}

impl Future for TrackedSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.probe.completed.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        *self.probe.waker.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for TrackedSleep {
    fn drop(&mut self) {
        self.probe.op_dropped.store(true, Ordering::SeqCst);
    }
}

/// A time provider whose `sleep` parks the caller on [`TrackedSleep`].
struct TrackedTime {
    probe: Arc<Probe>,
}

impl TimeProvider for TrackedTime {
    fn now(&mut self) -> Datetime {
        Datetime {
            seconds: 0,
            nanoseconds: 0,
        }
    }

    fn monotonic_now(&mut self) -> u64 {
        0
    }

    fn resolution(&mut self) -> u64 {
        1
    }

    fn sleep(&mut self, _duration_ns: u64) -> BoxOp<()> {
        Box::pin(TrackedSleep {
            probe: self.probe.clone(),
            _buffer: TrackedBuffer::new(self.probe.clone()),
        })
    }
}

#[test]
fn killing_a_task_blocked_on_a_provider_future_leaks_nothing_observable() {
    let image = run::compile_wat(fixtures::sleeper_wat());
    let probe = Arc::new(Probe::default());

    let mut task = Task::spawn(
        &image,
        &[],
        SpawnLimits::default(),
        Providers {
            time: Some(Box::new(TrackedTime {
                probe: probe.clone(),
            })),
            ..Providers::none()
        },
    )
    .expect("sleeper should spawn");

    // The guest runs up to the await on the provider future and parks. The provider now
    // holds the transferred buffer and the task's doorbell waker.
    assert_eq!(task.resume(100 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    assert!(!task.is_runnable());
    assert_eq!(probe.buffers_live.load(Ordering::SeqCst), 1);
    assert!(!probe.op_dropped.load(Ordering::SeqCst));
    assert!(
        probe.waker.lock().unwrap().is_some(),
        "the pending operation must have been polled with the task's doorbell"
    );

    // Kill the task while it is blocked on the provider future.
    assert_eq!(task.kill(), Outcome::Killed);

    // Nothing dangles: the in-flight operation was dropped with the task, and the buffer
    // it held was released — no buffer is still owned by anyone.
    assert!(probe.op_dropped.load(Ordering::SeqCst));
    assert_eq!(
        probe.buffers_live.load(Ordering::SeqCst),
        0,
        "the buffer transferred to the provider must be released when the task is killed"
    );

    // The provider's backend completes the underlying work afterwards, on its own
    // schedule, and the completion goes nowhere — quietly, with no panic: the doorbell
    // waker it still holds outlives the dead task and waking it is a no-op.
    let backend_probe = probe.clone();
    let backend = std::thread::spawn(move || {
        backend_probe.completed.store(true, Ordering::SeqCst);
        if let Some(waker) = backend_probe.waker.lock().unwrap().take() {
            waker.wake();
        }
    });
    backend
        .join()
        .expect("a provider completing work for a dead task must not panic");
    assert!(probe.completed.load(Ordering::SeqCst));
    assert_eq!(probe.buffers_live.load(Ordering::SeqCst), 0);
}

/// The contrast case: the same guest and provider, but the backend completes the sleep
/// while the task is still alive — the task wakes, finishes, and returns its own value.
/// This pins down that the kill test above really did interrupt an operation that would
/// otherwise have completed normally.
#[test]
fn the_same_blocked_task_completes_normally_when_not_killed() {
    let image = run::compile_wat(fixtures::sleeper_wat());
    let probe = Arc::new(Probe::default());

    let mut task = Task::spawn(
        &image,
        &[],
        SpawnLimits::default(),
        Providers {
            time: Some(Box::new(TrackedTime {
                probe: probe.clone(),
            })),
            ..Providers::none()
        },
    )
    .expect("sleeper should spawn");

    assert_eq!(task.resume(100 * FUEL_QUANTUM), ResumeOutcome::Blocked);

    // Complete the operation from the backend while the task is alive.
    probe.completed.store(true, Ordering::SeqCst);
    if let Some(waker) = probe.waker.lock().unwrap().take() {
        waker.wake();
    }

    let outcome = run::drive(&mut task);
    assert_eq!(run::success_value(&outcome), "9");
    assert_eq!(
        probe.buffers_live.load(Ordering::SeqCst),
        0,
        "a completed operation must also return its buffer"
    );
}
