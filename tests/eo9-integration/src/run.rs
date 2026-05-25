//! Running a component to its outcome under a given set of root providers.
//!
//! These helpers are the execution half of the harness: compile with the pinned Eo9
//! engine, spawn against the supplied [`Providers`], and drive the task with repeated
//! fuel donations until it finishes. Tests that need finer control (spawn errors, kill,
//! fuel accounting) use `eo9_runtime::Task` directly and can still reuse [`drive`].

use std::time::{Duration, Instant};

use eo9_component::Component;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    EngineOptions, Image, NamedArg, Outcome, Providers, ResumeOutcome, SpawnLimits, Task,
    new_engine,
};

/// How long [`drive`] waits for a blocked task or an unfinished run before failing the
/// test. Generous on purpose: the suites use in-memory providers, so anything close to
/// this is a hang, not load.
const DRIVE_DEADLINE: Duration = Duration::from_secs(60);

/// Fuel donated per [`drive`] iteration.
const DONATION: u64 = 100 * FUEL_QUANTUM;

/// Compile a component value into an executable image with the pinned engine.
pub fn compile_component(component: &Component) -> Image {
    let engine = new_engine(&EngineOptions::default()).expect("pinned engine config is valid");
    Image::compile(&engine, component.bytes()).expect("fixture component should compile")
}

/// Compile raw component bytes or WAT text into an executable image.
pub fn compile_wat(wat: &str) -> Image {
    let engine = new_engine(&EngineOptions::default()).expect("pinned engine config is valid");
    Image::compile(&engine, wat).expect("fixture WAT should compile")
}

/// Spawn `image` with the given arguments and providers and drive it to its outcome.
pub fn run_image(image: &Image, args: &[NamedArg], providers: Providers) -> Outcome {
    let mut task = Task::spawn(image, args, SpawnLimits::default(), providers)
        .expect("fixture task should spawn");
    drive(&mut task)
}

/// Compile `component` and run it to its outcome (see [`run_image`]).
pub fn run_component(component: &Component, args: &[NamedArg], providers: Providers) -> Outcome {
    run_image(&compile_component(component), args, providers)
}

/// Drive a spawned task to completion: donate fuel repeatedly, waiting for the doorbell
/// whenever the task blocks on a provider completion. Panics if the task neither finishes
/// nor becomes runnable within a generous deadline.
pub fn drive(task: &mut Task) -> Outcome {
    let deadline = Instant::now() + DRIVE_DEADLINE;
    loop {
        match task.resume(DONATION) {
            ResumeOutcome::Done(outcome) => return outcome,
            ResumeOutcome::OutOfFuel => {}
            ResumeOutcome::Blocked => {
                while !task.is_runnable() {
                    assert!(
                        Instant::now() < deadline,
                        "task blocked and no provider completion arrived"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "task did not finish within the harness deadline"
        );
    }
}

/// The WAVE text of a successful outcome's value; panics (with the outcome) otherwise.
pub fn success_value(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Success(value) => &value.value,
        other => panic!("expected the program's own success, got {other:?}"),
    }
}

/// The WAVE text of a failed outcome's value; panics (with the outcome) otherwise.
pub fn failure_value(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Failure(value) => &value.value,
        other => panic!("expected the program's own failure, got {other:?}"),
    }
}
