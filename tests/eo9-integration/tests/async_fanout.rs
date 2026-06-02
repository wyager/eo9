//! Async hardening, section 3 (plan/13): fan-out concurrency.
//!
//! One consumer starts three concurrent calls against the same parking provider — two
//! that park, one that completes eagerly — and awaits them jointly on one waitable set.
//! Pins: no completion is lost, the results stay associated with their calls, the
//! completion order follows the host's completion schedule, repeated runs are
//! byte-identical, and cancelling one of K leaves the others to complete normally.
//!
//! Outcome encoding (see `fixtures::fanout_consumer`): `order * 100000 + sum`, where
//! `order` is the completion-order digits (call index; `9` marks the cancellation) and
//! `sum` adds each completed call's result plus `7 * status` for a cancelled call.

use std::sync::Arc;

use eo9_component::compose;
use eo9_integration::fixtures::{awaiting_parker, fanout_consumer};
use eo9_integration::park::ParkBed;
use eo9_integration::run;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{Providers, ResumeOutcome, SpawnLimits, Task};

fn providers(bed: &Arc<ParkBed>) -> Providers {
    Providers {
        time: Some(bed.clock()),
        ..Providers::none()
    }
}

/// Spawn `parker $ fanout_consumer`, drive to the joint park, then complete the two
/// parked sleeps in the given order and return the outcome value.
fn run_fanout(complete_order: [usize; 2]) -> String {
    let bed = ParkBed::new();
    let program = compose(&awaiting_parker(), &fanout_consumer(false)).expect("parker $ fanout");
    let image = run::compile_component(&program);
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("spawns");

    assert_eq!(task.resume(1000 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    assert_eq!(
        bed.started(),
        2,
        "two of the three calls park (run(60) and run(70)); run(1) is eager"
    );

    // Complete the parked sleeps in the requested order, draining between completions
    // so the completion order is the *observed* order.
    for idx in complete_order {
        bed.complete(idx);
        assert!(task.is_runnable());
        match task.resume(1000 * FUEL_QUANTUM) {
            ResumeOutcome::Done(outcome) => return run::success_value(&outcome).to_string(),
            ResumeOutcome::Blocked => {}
            other => panic!("unexpected resume outcome {other:?}"),
        }
    }
    let outcome = run::drive(&mut task);
    run::success_value(&outcome).to_string()
}

/// Decode `order * 100000 + sum`.
fn decode(value: &str) -> (u32, u32) {
    let n: u32 = value.parse().expect("numeric outcome");
    (n / 100000, n % 100000)
}

/// All three results arrive intact, none lost, and the completion order tracks the
/// host's completion schedule: sleep 0 backs `run(60)` (call 1), sleep 1 backs
/// `run(70)` (call 3).
#[test]
fn three_concurrent_calls_complete_with_no_lost_results() {
    let value = run_fanout([1, 0]);
    let (order, sum) = decode(&value);
    assert_eq!(sum, 160 + 101 + 170, "every result must arrive intact");
    // The eager call (2) completes at issue; then 3 (sleep 1), then 1 (sleep 0).
    assert_eq!(order, 231, "completion order must follow the host schedule");
}

/// The mirror schedule: completing the other sleep first flips exactly the parked
/// calls' order digits.
#[test]
fn the_completion_order_tracks_the_host_schedule() {
    let value = run_fanout([0, 1]);
    let (order, sum) = decode(&value);
    assert_eq!(sum, 160 + 101 + 170);
    assert_eq!(order, 213);
}

/// Same schedule, byte-identical outcome: fan-out completion delivery is
/// deterministic.
#[test]
fn fanout_is_deterministic_across_runs() {
    assert_eq!(run_fanout([1, 0]), run_fanout([1, 0]));
    assert_eq!(run_fanout([0, 1]), run_fanout([0, 1]));
}

/// Cancel one of K: the third call is cancelled after issue (resolving to
/// RETURN_CANCELLED = 4 and releasing its host operation); the eager call and the
/// remaining parked call complete normally.
#[test]
fn cancelling_one_of_three_leaves_the_others_intact() {
    let bed = ParkBed::new();
    let program = compose(&awaiting_parker(), &fanout_consumer(true)).expect("parker $ fanout");
    let image = run::compile_component(&program);
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("spawns");

    assert_eq!(task.resume(1000 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    assert_eq!(bed.started(), 2);
    assert!(
        bed.dropped() >= 1,
        "the cancelled call's host operation must be released by the cascade"
    );
    assert!(bed.parked(0), "call 1's sleep must still be in flight");

    bed.complete(0);
    let outcome = run::drive(&mut task);
    let (order, sum) = decode(run::success_value(&outcome));
    // Eager call 2 at issue, the cancellation step (9), then call 1's completion.
    assert_eq!(order, 291);
    assert_eq!(sum, 101 + 4 * 7 + 160);
    assert_eq!(bed.dropped(), 2, "both started operations released");
}
