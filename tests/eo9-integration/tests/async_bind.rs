//! Async hardening, section 5 (plan/13): compose-time configuration in an async chain.
//!
//! Two pins on the `bind` entrypoint (`eo9:rt/configured.bind`, SPEC "configure never
//! traps") interacting with honestly-async programs:
//!
//! * a baked configuration is bound at **spawn**, before `main` runs — so a program
//!   that parks immediately still observes the configured value after the park;
//! * a configuration the provider refuses surfaces as the typed pre-run refusal
//!   (`SpawnError::ConfigurationRefused` carrying the provider's own text), even
//!   though the program it would have fed parks on its first instruction — the
//!   program is never entered.

use std::sync::Arc;

use eo9_component::{compose, configure};
use eo9_integration::fixtures::{GATE_REFUSAL, gate_parker, gate_provider};
use eo9_integration::park::ParkBed;
use eo9_integration::run;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{Providers, ResumeOutcome, SpawnError, SpawnLimits, Task};

fn providers(bed: &Arc<ParkBed>) -> Providers {
    Providers {
        time: Some(bed.clock()),
        ..Providers::none()
    }
}

/// `configure(gate, level: 6) $ gate-parker`: the consumer parks on the host clock
/// before reading the gate; the value it then reads (42) proves `bind` ran at spawn
/// and the baked configuration survived the park.
#[test]
fn a_baked_configuration_is_bound_before_the_chain_parks() {
    let configured = configure(&gate_provider(), &[("level", "6")]).expect("configure should bake");
    let program = compose(&configured, &gate_parker()).expect("configured $ parker");

    let bed = ParkBed::new();
    let image = run::compile_component(&program);
    let mut task =
        Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)).expect("spawns");

    // The program parks before touching the gate.
    assert_eq!(task.resume(1000 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    assert_eq!(bed.started(), 1);

    bed.complete(0);
    let outcome = run::drive(&mut task);
    assert_eq!(
        run::success_value(&outcome),
        "42",
        "the configured level (6 * 7) must be observable after the park"
    );
}

/// `configure(gate, level: 13) $ gate-parker`: the provider refuses at bind time; the
/// refusal is the typed pre-run error and the (would-park) program is never entered —
/// no host operation is ever started.
#[test]
fn a_refused_configuration_is_a_typed_pre_run_error_even_for_a_parking_chain() {
    let configured =
        configure(&gate_provider(), &[("level", "13")]).expect("configure should bake");
    let program = compose(&configured, &gate_parker()).expect("configured $ parker");

    let bed = ParkBed::new();
    let image = run::compile_component(&program);
    match Task::spawn(&image, &[], SpawnLimits::default(), providers(&bed)) {
        Err(SpawnError::ConfigurationRefused(msg)) => {
            assert_eq!(msg, GATE_REFUSAL, "the provider's own text must surface");
        }
        Ok(_) => panic!("a refused configuration must fail the spawn"),
        Err(other) => panic!("expected ConfigurationRefused, got {other}"),
    }
    assert_eq!(
        bed.started(),
        0,
        "the program must never be entered: no host operation"
    );
}
