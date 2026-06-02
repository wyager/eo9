//! Async hardening, section 1 (plan/13): deep suspension chains.
//!
//! An awaiting consumer over N forwarding guests (N = 0..3) over a genuinely-parking
//! leaf — every boundary honestly async (async-lowered calls awaited through the
//! callback ABI), the park driven by a controllable host clock. Pins, at every depth:
//! the chain parks (not spins), exactly one host operation backs the whole chain, the
//! result survives every forwarding layer intact, completion is prompt once the host
//! completes, and repeated runs are byte-identical.

use std::sync::Arc;

use eo9_component::{Component, compose};
use eo9_integration::fixtures::{RelayCancel, awaiting_consumer, awaiting_parker, awaiting_relay};
use eo9_integration::park::ParkBed;
use eo9_integration::run;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{Providers, ResumeOutcome, SpawnLimits, Task};

/// `parker $ relay^depth $ consumer(x)`.
fn chain(depth: usize, x: u32) -> Component {
    let mut program = awaiting_consumer(x);
    for _ in 0..depth {
        program = compose(&awaiting_relay(RelayCancel::Cascade), &program)
            .expect("relay $ chain should compose");
    }
    compose(&awaiting_parker(), &program).expect("parker $ chain should compose")
}

fn providers(bed: &Arc<ParkBed>) -> Providers {
    Providers {
        time: Some(bed.clock()),
        ..Providers::none()
    }
}

/// Run a parking chain at `depth`: park, verify the park, complete, verify the result.
fn park_and_complete(depth: usize) -> String {
    let image = run::compile_component(&chain(depth, 57));
    let bed = ParkBed::new();
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("chain spawns");

    // The whole chain runs up to the host await and parks.
    assert_eq!(
        task.resume(1000 * FUEL_QUANTUM),
        ResumeOutcome::Blocked,
        "a chain over a parking leaf must park, depth {depth}"
    );
    assert!(!task.is_runnable());
    assert_eq!(
        bed.started(),
        1,
        "exactly one host operation backs the chain, depth {depth}"
    );
    assert!(bed.parked(0));

    // Complete the host operation; the completion propagates through every layer.
    bed.complete(0);
    assert!(task.is_runnable(), "completion must ring the doorbell");
    let outcome = run::drive(&mut task);
    let value = run::success_value(&outcome).to_string();
    assert_eq!(
        bed.dropped(),
        1,
        "the completed operation must be released, depth {depth}"
    );
    value
}

#[test]
fn a_parked_chain_completes_with_the_result_intact_at_depth_0() {
    assert_eq!(park_and_complete(0), "157");
}

#[test]
fn a_parked_chain_completes_with_the_result_intact_at_depth_1() {
    assert_eq!(park_and_complete(1), "167");
}

#[test]
fn a_parked_chain_completes_with_the_result_intact_at_depth_2() {
    assert_eq!(park_and_complete(2), "177");
}

#[test]
fn a_parked_chain_completes_with_the_result_intact_at_depth_3() {
    assert_eq!(park_and_complete(3), "187");
}

/// The same chain twice, same completion schedule: byte-identical outcomes.
#[test]
fn parked_chains_are_deterministic_across_runs() {
    for depth in 0..=3 {
        assert_eq!(
            park_and_complete(depth),
            park_and_complete(depth),
            "depth {depth} must be deterministic"
        );
    }
}

/// An eager leaf under the same awaiting machinery: the chain completes without ever
/// parking (the RETURNED arms of every layer), and no host operation is started.
#[test]
fn an_eager_leaf_completes_through_the_awaiting_machinery_without_parking() {
    for depth in 0..=3 {
        let bed = ParkBed::new();
        let outcome = run::run_component(&chain(depth, 7), &[], providers(&bed));
        assert_eq!(
            run::success_value(&outcome),
            (107 + 10 * depth as u32).to_string(),
            "depth {depth}"
        );
        assert_eq!(bed.started(), 0, "no host operation for an eager leaf");
    }
}
