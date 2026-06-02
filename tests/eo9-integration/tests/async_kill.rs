//! Async hardening, section 2 (plan/13): cancellation and kill mid-await.
//!
//! The GAPS caveat under test: "cancellation of an in-flight forwarded call traps". Two
//! distinct mechanisms are pinned, at every depth:
//!
//! * **host kill** — `Task::kill` while the chain is parked: the store (and with it the
//!   parked guest tasks, queued completions, and the in-flight provider operation) is
//!   dropped; nothing leaks and nothing panics;
//! * **guest cancellation** — `subtask.cancel` of an in-flight forwarded call: the
//!   `CANCELLED` event cascades down the chain (each layer cancelling its own
//!   downstream and acknowledging with `task.cancel`), the host operation is released,
//!   and the canceller observes `RETURN_CANCELLED`. The unacknowledged flavor (a layer
//!   that ignores `CANCELLED`) and the cancel-after-terminal flavor are pinned too.

use std::sync::Arc;

use eo9_component::{Component, compose};
use eo9_integration::fixtures::{
    RelayCancel, awaiting_consumer, awaiting_parker, awaiting_relay, cancel_after_done_consumer,
    cancel_after_event_consumer, canceller_consumer,
};
use eo9_integration::park::ParkBed;
use eo9_integration::run;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{Outcome, Providers, ResumeOutcome, SpawnLimits, Task};

fn providers(bed: &Arc<ParkBed>) -> Providers {
    Providers {
        time: Some(bed.clock()),
        ..Providers::none()
    }
}

fn parked_chain(depth: usize, consumer: Component, bed: &Arc<ParkBed>) -> Task {
    let mut program = consumer;
    for _ in 0..depth {
        program = compose(&awaiting_relay(RelayCancel::Cascade), &program)
            .expect("relay $ chain should compose");
    }
    let program = compose(&awaiting_parker(), &program).expect("parker $ chain should compose");
    let image = run::compile_component(&program);
    Task::spawn(&image, &[], SpawnLimits::default(), providers(bed)).expect("chain spawns")
}

/// Host kill while parked, at every depth: `Outcome::Killed`, the in-flight provider
/// operation is released with the store, and nothing panics. (The kill suite covers
/// depth 0 against a raw sleeper; this pins the *forwarded* park.)
#[test]
fn killing_a_chain_parked_through_forwarding_layers_leaks_nothing() {
    for depth in 0..=3 {
        let bed = ParkBed::new();
        let mut task = parked_chain(depth, awaiting_consumer(57), &bed);
        assert_eq!(
            task.resume(1000 * FUEL_QUANTUM),
            ResumeOutcome::Blocked,
            "depth {depth}"
        );
        assert_eq!(bed.started(), 1, "depth {depth}");
        assert!(bed.parked(0), "depth {depth}");

        assert_eq!(task.kill(), Outcome::Killed, "depth {depth}");
        assert_eq!(
            bed.dropped(),
            1,
            "the in-flight host operation must be dropped with the store, depth {depth}"
        );
    }
}

/// The backend completing a sleep for a killed chain is a quiet no-op (the doorbell
/// waker outlives the dead task), exactly as for an unforwarded park.
#[test]
fn a_completion_arriving_after_the_kill_goes_nowhere_quietly() {
    let bed = ParkBed::new();
    let mut task = parked_chain(2, awaiting_consumer(57), &bed);
    assert_eq!(task.resume(1000 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    task.kill_in_place();
    bed.complete(0); // must not panic; the wake lands on a dead task's doorbell
    assert_eq!(task.outcome(), Some(&Outcome::Killed));
}

/// Guest cancellation of an in-flight call directly against the parking provider: the
/// provider's `CANCELLED` arm cancels its host sleep, acknowledges with `task.cancel`,
/// and the canceller observes `RETURN_CANCELLED` (4). The host operation is released.
#[test]
fn cancelling_an_in_flight_call_resolves_to_return_cancelled() {
    let bed = ParkBed::new();
    let mut task = parked_chain(0, canceller_consumer(57), &bed);
    let outcome = run::drive(&mut task);
    assert_eq!(
        run::success_value(&outcome),
        "4",
        "the canceller must observe RETURN_CANCELLED"
    );
    assert_eq!(
        bed.dropped(),
        1,
        "the cancelled host operation must be released"
    );
}

/// The same cancellation through forwarding layers that cascade: each relay cancels its
/// own downstream and acknowledges; the canceller still observes `RETURN_CANCELLED` and
/// the host operation is still released. This is the literal GAPS caveat, exercised.
#[test]
fn cancelling_a_forwarded_call_cascades_through_the_chain() {
    for depth in 1..=2 {
        let bed = ParkBed::new();
        let mut task = parked_chain(depth, canceller_consumer(57), &bed);
        let outcome = run::drive(&mut task);
        assert_eq!(
            run::success_value(&outcome),
            "4",
            "depth {depth}: the cancellation must cascade to RETURN_CANCELLED"
        );
        assert_eq!(bed.dropped(), 1, "depth {depth}");
    }
}

/// A layer that ignores its `CANCELLED` event: the canceller's sync `subtask.cancel`
/// has nothing to resolve it, so the whole task parks forever — the liveness failure
/// the SPEC's bounded-await rule exists for. Pinned: a quiet park (not a trap), and a
/// host kill still cleans up.
#[test]
fn an_unacknowledged_cancellation_parks_the_canceller_forever() {
    let bed = ParkBed::new();
    let mut program = canceller_consumer(57);
    program = compose(&awaiting_relay(RelayCancel::Ignore), &program).expect("relay $ canceller");
    program = compose(&awaiting_parker(), &program).expect("parker $ chain");
    let image = run::compile_component(&program);
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("chain spawns");

    // The cancel is delivered, the relay ignores it, and the canceller stays blocked —
    // donating more fuel never helps.
    for round in 0..5 {
        assert_eq!(
            task.resume(1000 * FUEL_QUANTUM),
            ResumeOutcome::Blocked,
            "round {round}: an unacknowledged cancel must park, not trap"
        );
        assert!(!task.is_runnable(), "round {round}");
    }
    // The host operation is still held by the (cancel-ignoring) chain.
    assert_eq!(bed.started(), 1);

    // A host kill remains the backstop and releases everything.
    assert_eq!(task.kill(), Outcome::Killed);
    assert_eq!(bed.dropped(), 1);
}

/// Cancelling after an eager completion: a call that returns at issue never mints a
/// subtask handle (the ABI packs no waitable with `RETURNED`), so the cancel attempt
/// names a nonexistent handle and traps on it. Pinned: the failure is a trap at the
/// guest's own error, not a hang or a corrupted result.
#[test]
fn cancelling_an_eagerly_completed_call_traps_on_the_missing_handle() {
    let bed = ParkBed::new();
    let mut task = parked_chain(0, cancel_after_done_consumer(), &bed);
    let outcome = run::drive(&mut task);
    match &outcome {
        Outcome::Trapped(reason) => {
            assert!(
                reason.contains("unknown handle"),
                "the trap should name the missing handle, got: {reason}"
            );
        }
        other => panic!("cancel of a handle-less eager call must trap, got {other:?}"),
    }
}

/// Cancelling a subtask whose completion event was already consumed: the canonical ABI
/// calls this a guest error, and the runtime traps. Pinned so the conversion work knows
/// the exact failure mode.
#[test]
fn cancelling_after_the_completion_event_traps_by_contract() {
    let bed = ParkBed::new();
    let mut task = parked_chain(0, cancel_after_event_consumer(), &bed);
    assert_eq!(task.resume(1000 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    bed.complete(0);
    let outcome = run::drive(&mut task);
    match &outcome {
        Outcome::Trapped(reason) => {
            assert!(
                reason.to_lowercase().contains("cancel"),
                "the trap should name the cancel, got: {reason}"
            );
        }
        other => panic!("cancel-after-terminal must trap, got {other:?}"),
    }
}
