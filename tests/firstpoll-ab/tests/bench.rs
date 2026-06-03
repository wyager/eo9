//! Rung-4 measurement for the first-poll A/B (docs/spikes/first-poll-inline.md,
//! "Measure"): per-call overhead of guest->guest boundaries when the callee completes
//! eagerly, and a regression check that genuinely-parked chains are unaffected.
//!
//! Run explicitly, in both arms, with timings printed:
//!
//!   cargo test --release --test bench -- --ignored --nocapture
//!   cargo test --release --test bench --features first-poll-inline -- --ignored --nocapture
//!
//! `eager_chain_depth_N`: `time_leaf_async $ time_relay(EagerPoll, AsyncLift)^N $
//! eager_consumer(SyncLower)` — every boundary is an async-lowered call to a
//! callback-ABI callee that completes without waiting. Arm A pays a queue round trip
//! (and fiber suspension) per boundary per call; arm B runs every activation inline.
//! The same image compiles once; each iteration spawns and drives to completion, so the
//! delta divided by iterations bounds the per-spawn savings (instantiation cost is
//! identical in both arms and dominates the absolute numbers).
//!
//! `parked_chain_depth_3`: the async_chains park-and-complete cycle — the leaf genuinely
//! parks on the host clock, so inlining only moves the initial activations earlier; the
//! completion cascade is event-loop-driven in both arms and the totals should match.

use std::time::Instant;

use eo9_component::{Component, compose};
use eo9_integration::fixtures::{
    ConsumerCall, RelayCancel, RelayExport, RelayImport, awaiting_consumer, awaiting_parker,
    awaiting_relay, eager_consumer, time_leaf_async, time_relay,
};
use eo9_integration::park::ParkBed;
use eo9_integration::run;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{Providers, ResumeOutcome, SpawnLimits, Task};

fn arm() -> &'static str {
    if cfg!(feature = "first-poll-inline") {
        "first-poll-inline ON "
    } else {
        "first-poll-inline OFF"
    }
}

fn eager_chain(depth: usize) -> Component {
    let mut program: Component = eager_consumer(ConsumerCall::SyncLower);
    for _ in 0..depth {
        program = compose(
            &time_relay(RelayImport::EagerPoll, RelayExport::AsyncLift),
            &program,
        )
        .expect("relay $ chain should compose");
    }
    compose(&time_leaf_async(), &program).expect("leaf $ chain should compose")
}

fn bench_eager(depth: usize, iterations: u32) {
    let image = run::compile_component(&eager_chain(depth));
    // Warm-up + correctness: the chain completes in both arms with the same value.
    let outcome = run::run_image(&image, &[], Providers::none());
    assert_eq!(run::success_value(&outcome), "7002");

    let start = Instant::now();
    for _ in 0..iterations {
        let outcome = run::run_image(&image, &[], Providers::none());
        assert_eq!(run::success_value(&outcome), "7002");
    }
    let total = start.elapsed();
    println!(
        "[{}] eager chain depth {depth}: {iterations} runs in {total:?} ({:?}/run)",
        arm(),
        total / iterations,
    );
}

#[test]
#[ignore = "measurement, run explicitly with --ignored --nocapture in both arms"]
fn eager_chain_depth_1() {
    bench_eager(1, 200);
}

#[test]
#[ignore = "measurement, run explicitly with --ignored --nocapture in both arms"]
fn eager_chain_depth_2() {
    bench_eager(2, 200);
}

#[test]
#[ignore = "measurement, run explicitly with --ignored --nocapture in both arms"]
fn eager_chain_depth_4() {
    bench_eager(4, 200);
}

#[test]
#[ignore = "measurement, run explicitly with --ignored --nocapture in both arms"]
fn parked_chain_depth_3() {
    let mut program = awaiting_consumer(57);
    for _ in 0..3 {
        program = compose(&awaiting_relay(RelayCancel::Cascade), &program)
            .expect("relay $ chain should compose");
    }
    let chain = compose(&awaiting_parker(), &program).expect("parker $ chain should compose");
    let image = run::compile_component(&chain);

    let iterations: u32 = 100;
    let start = Instant::now();
    for _ in 0..iterations {
        let bed = ParkBed::new();
        let providers = Providers {
            time: Some(bed.clock()),
            ..Providers::none()
        };
        let mut task =
            Task::spawn(&image, &[], SpawnLimits::default(), providers).expect("chain spawns");
        assert_eq!(task.resume(1000 * FUEL_QUANTUM), ResumeOutcome::Blocked);
        bed.complete(0);
        let outcome = run::drive(&mut task);
        assert_eq!(run::success_value(&outcome), "187");
    }
    let total = start.elapsed();
    println!(
        "[{}] parked chain depth 3: {iterations} park+complete cycles in {total:?} ({:?}/cycle)",
        arm(),
        total / iterations,
    );
}
