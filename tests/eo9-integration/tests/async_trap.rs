//! Async hardening, section 4 (plan/13): a sibling subtask traps while others are parked.
//!
//! Two named `step` slots — `park` (the awaiting parker, backed by the controllable
//! host clock) and `trap` (a provider whose body is `unreachable`) — under one
//! consumer that issues a call to each. The parker's call genuinely parks on the host
//! bed; the trapper's queued activation then traps. Pins: the trap surfaces promptly
//! as the whole program's outcome (a trap poisons the composed program — SPEC kill
//! semantics), and the parked sibling's in-flight host operation is released with the
//! store rather than leaked or left hanging.

use std::sync::Arc;

use eo9_component::{compose, rename};
use eo9_integration::fixtures::{awaiting_consumer, awaiting_parker, mixed_consumer, step_trapper};
use eo9_integration::park::ParkBed;
use eo9_integration::run;
use eo9_runtime::{Outcome, Providers, SpawnLimits, Task};

fn providers(bed: &Arc<ParkBed>) -> Providers {
    Providers {
        time: Some(bed.clock()),
        ..Providers::none()
    }
}

/// The direct shape first: an awaiting consumer over the trapper alone. The queued
/// callee traps in its activation; the consumer's await surfaces it.
#[test]
fn a_trapping_callee_surfaces_as_the_programs_trap() {
    let bed = ParkBed::new();
    let program = compose(&step_trapper(), &awaiting_consumer(57)).expect("trapper $ consumer");
    let image = run::compile_component(&program);
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("spawns");
    match run::drive(&mut task) {
        Outcome::Trapped(reason) => assert!(
            reason.contains("unreachable"),
            "the trap must carry the callee's own reason, got: {reason}"
        ),
        other => panic!("a trapping callee must trap the program, got {other:?}"),
    }
}

/// The matrix shape: one sibling parked on the host clock, the other trapping. The
/// trap must surface (no hang), and dropping the finished task must release the parked
/// sibling's in-flight host operation.
#[test]
fn a_trap_while_a_sibling_is_parked_surfaces_and_releases_the_park() {
    let bed = ParkBed::new();
    let parker = rename(&awaiting_parker(), "eo9-tests:hard/step", "park").expect("rename park");
    let trapper = rename(&step_trapper(), "eo9-tests:hard/step", "trap").expect("rename trap");
    let program = compose(
        &parker,
        &compose(&trapper, &mixed_consumer()).expect("trap $ mixed"),
    )
    .expect("park $ (trap $ mixed)");

    let image = run::compile_component(&program);
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("spawns");

    // Drive to the outcome: the trapper poisons the program while the parker's host
    // sleep is in flight. `drive` enforces the no-hang half (its deadline panics).
    let outcome = run::drive(&mut task);
    match &outcome {
        Outcome::Trapped(reason) => assert!(
            reason.contains("unreachable"),
            "the sibling's trap must surface, got: {reason}"
        ),
        other => panic!("the program must trap, got {other:?}"),
    }

    // The parked sibling's operation is in flight at trap time…
    assert_eq!(
        bed.started(),
        1,
        "the parker must have parked before the trap"
    );
    // …and dropping the task (the store) releases it: nothing leaks.
    drop(task);
    assert_eq!(
        bed.dropped(),
        1,
        "the parked sibling's host operation must be released with the store"
    );
}
