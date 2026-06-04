//! Exhaustive loom models of the [`Doorbell`](crate::task::Doorbell) edge protocol — the
//! narrow-adopt item from docs/spikes/timing-strategies.md ("the Doorbell primitive is
//! exactly loom-shaped — would have found the lost wakeup exhaustively").
//!
//! Run with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p eo9-runtime --lib loom_ -- --nocapture
//! ```
//!
//! Under `--cfg loom` the Doorbell's atomics and mutex are loom's model-checked versions
//! (see `bell_sync` in task.rs), so these tests run the **real** `ring`/`register`/
//! `poll_edge` implementation under every interleaving of the modeled threads — not a
//! transcript of it. What stays a model: the *callers* of the protocol. `Task::runnable`
//! and the `task.wait` host function cannot be constructed without a wasmtime store, so
//! the tests re-state their few-line poll shapes (with line-by-line comments mapping back
//! to task.rs / link.rs). Fidelity gaps, honestly: (1) waker identity flows through
//! `std::sync::Arc` (loom does not model the refcount, only the tracked atomics inside);
//! (2) the parent's *park* is modeled as "returned `Pending` with no further polls", which
//! is exactly the embedder contract (a future that returns `Pending` is re-polled only
//! when its waker fires).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};
use std::task::{Context, Poll, Waker};

use loom::sync::atomic::{AtomicBool, Ordering};

use crate::task::Doorbell;

// -----------------------------------------------------------------------------------
// Harness
// -----------------------------------------------------------------------------------

/// A waker whose wake sets a loom-tracked flag, standing in for "the parked thread was
/// actually woken" in assertions.
struct WakeFlag(AtomicBool);

impl std::task::Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn flag_waker() -> (Waker, Arc<WakeFlag>) {
    let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
    (Waker::from(flag.clone()), flag)
}

/// Run a loom model and report how many schedules were explored.
fn model_counted(name: &'static str, f: impl Fn() + Send + Sync + 'static) {
    let schedules = Arc::new(AtomicUsize::new(0));
    let counter = schedules.clone();
    loom::model(move || {
        counter.fetch_add(1, StdOrdering::Relaxed);
        f();
    });
    println!(
        "loom[{name}]: {} schedules explored",
        schedules.load(StdOrdering::Relaxed)
    );
}

// -----------------------------------------------------------------------------------
// (a) The fundamental property: a ring concurrent with the edge-wait is never lost.
// -----------------------------------------------------------------------------------

#[test]
fn loom_ring_concurrent_with_edge_wait_is_never_lost() {
    model_counted("ring-vs-wait", || {
        let bell = Arc::new(Doorbell::default());

        let ringer = {
            let bell = bell.clone();
            loom::thread::spawn(move || bell.ring())
        };

        // One waiter polling the real protocol once, then (notionally) parking.
        let (waker, flag) = flag_waker();
        let mut cx = Context::from_waker(&waker);
        let polled = bell.poll_edge(|| bell.is_rung().then_some(()), &mut cx);

        ringer.join().unwrap();

        // Every schedule must end with the edge observable: either the poll saw it, or
        // the ring drained the registered waker and woke the (parked) waiter.
        assert!(
            polled.is_ready() || flag.0.load(Ordering::SeqCst),
            "the doorbell edge was lost: poll returned Pending and the waiter was never woken"
        );
    });
}

// -----------------------------------------------------------------------------------
// (b) The regression: the pre-fix task.wait shape (the discarded Ready, bd67f89).
// -----------------------------------------------------------------------------------

/// One poll of the `task.wait` host function's blocked-child branch (link.rs), against a
/// real child doorbell. `act_on_ready` selects the fixed shape (act on the Ready edge by
/// self-waking) or the pre-fix shape (discard it).
///
/// Returns whether the parent observed the child runnable (and so would resume it instead
/// of parking). The child here is parked-on-I/O, so "runnable" is exactly "doorbell rung"
/// (`Task::is_runnable` with `parked == true`).
fn wait_site_poll(child_bell: &Doorbell, parent: &Waker, act_on_ready: bool) -> bool {
    // link.rs: `if child.is_runnable() { cx.waker().wake_by_ref(); }` — the parent keeps
    // itself awake and resumes the child on its next iteration; no park happens.
    if child_bell.is_rung() {
        return true;
    }

    // link.rs: `let runnable = child.runnable(); ... runnable.as_mut().poll(cx)` — the
    // real protocol, polled with the parent's waker.
    let mut cx = Context::from_waker(parent);
    let ready = child_bell
        .poll_edge(|| child_bell.is_rung().then_some(()), &mut cx)
        .is_ready();

    if ready && act_on_ready {
        // The fix (bd67f89): the Ready edge is the only wake left — act on it.
        parent.wake_by_ref();
    }
    // Pre-fix: `let _ = runnable.as_mut().poll(cx);` — the Ready is discarded.

    // Either way the host fn returns Pending here and the parent parks on its own
    // doorbell (the embedder re-polls it only when `parent` is woken).
    false
}

/// The completion model: the child's I/O finishes on another thread exactly once — a
/// parked child only runs on parent-donated fuel, so no second ring is ever coming.
fn wait_site_model(act_on_ready: bool) {
    let child_bell = Arc::new(Doorbell::default());

    let completion = {
        let bell = child_bell.clone();
        loom::thread::spawn(move || bell.ring())
    };

    let (parent_waker, parent_flag) = flag_waker();
    let observed = wait_site_poll(&child_bell, &parent_waker, act_on_ready);

    completion.join().unwrap();

    // The completion has happened, and the child will never ring again. If the parent
    // neither observed runnability nor holds a delivered/incoming wake, it is parked
    // forever over a runnable child — the plan/11 hang.
    assert!(
        observed || parent_flag.0.load(Ordering::SeqCst),
        "lost wakeup: the parent parked forever while the child sits runnable"
    );
}

/// Loom must FIND the pre-fix bug: some schedule (ring's drain between the parent's
/// first check and the registration; the sticky flag making the re-observation Ready;
/// the Ready discarded) violates the assertion.
#[test]
fn loom_finds_the_prefix_discarded_ready_hang() {
    let result = catch_unwind(AssertUnwindSafe(|| {
        loom::model(|| wait_site_model(false));
    }));
    assert!(
        result.is_err(),
        "loom failed to find the pre-fix lost-wakeup counterexample — the model no \
         longer matches the protocol"
    );
}

/// The fixed shape passes every schedule.
#[test]
fn loom_fixed_wait_site_never_loses_the_wakeup() {
    model_counted("wait-site-fixed", || wait_site_model(true));
}

// -----------------------------------------------------------------------------------
// (c) Drain-all: one ring wakes every registered waiter (the 5fa53e8 shape — a
//     last-write-wins waiter slot would lose one of them).
// -----------------------------------------------------------------------------------

#[test]
fn loom_ring_wakes_every_registered_waiter() {
    model_counted("drain-all", || {
        let bell = Arc::new(Doorbell::default());

        // Waiter A on its own thread.
        let waiter_a = {
            let bell = bell.clone();
            loom::thread::spawn(move || {
                let (waker, flag) = flag_waker();
                let mut cx = Context::from_waker(&waker);
                let ready = bell
                    .poll_edge(|| bell.is_rung().then_some(()), &mut cx)
                    .is_ready();
                (ready, flag)
            })
        };

        // The ring, concurrent with both waiters.
        let ringer = {
            let bell = bell.clone();
            loom::thread::spawn(move || bell.ring())
        };

        // Waiter B on the model's main thread.
        let (waker_b, flag_b) = flag_waker();
        let mut cx = Context::from_waker(&waker_b);
        let ready_b = bell
            .poll_edge(|| bell.is_rung().then_some(()), &mut cx)
            .is_ready();

        let (ready_a, flag_a) = waiter_a.join().unwrap();
        ringer.join().unwrap();

        assert!(
            ready_a || flag_a.0.load(Ordering::SeqCst),
            "waiter A lost: the ring must wake every registered waiter"
        );
        assert!(
            ready_b || flag_b.0.load(Ordering::SeqCst),
            "waiter B lost: the ring must wake every registered waiter"
        );
    });
}
