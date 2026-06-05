//! time — run a program and report how long it took.
//!
//! Targets the `eo9-examples:time/time` world (see `wit/world.wit`): a standard
//! component, not a shell builtin — the smallest executor. It imports the exec
//! capability (compile + task), takes the program to run as a component-typed `main`
//! argument (`time hello`, `time (gpu.virtio $ draw)`), measures the compile and run
//! phases with its own granted clock, prints the report, and forwards the child's
//! outcome as its own.
//!
//! Because the measuring instrument is a capability like everything else, composing a
//! clock onto `time` attenuates the measurement itself: `time.frozen … $ time hello`
//! honestly reports zero elapsed time.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

mod bindings {
    // The text and time imports map onto the shared SDK modules; the eo9:exec
    // interfaces are not part of the SDK world yet, so they are generated here (the
    // same arrangement as eosh).
    wit_bindgen::generate!({
        world: "time",
        generate_all,
        with: {
            "eo9:text/types@0.1.0": eo9_guest::api::text::types,
            "eo9:text/text@0.1.0": eo9_guest::api::text::text,
            "eo9:time/time@0.1.0": eo9_guest::api::time::time,
        },
    });
}

use bindings::eo9::exec::component_algebra::Component;
use bindings::eo9::exec::{compile, task};
use bindings::{Guest, ProgramFailure, ProgramSuccess};
use eo9_guest::api::time::time;

/// Monotonic nanoseconds from the granted clock.
fn now_ns(clock: &time::TimeImpl) -> u64 {
    time::monotonic_now(clock).nanoseconds
}

/// Render a nanosecond duration as seconds with millisecond precision (`1.234s`).
fn render(ns: u64) -> String {
    let ms = ns / 1_000_000;
    format!("{}.{:03}s", ms / 1000, ms % 1000)
}

/// Render the child's outcome the way the shell does: the WAVE value text of whichever
/// arm it took, or the abnormal exit.
fn render_outcome(outcome: &task::ProgramOutcome) -> String {
    match outcome {
        task::ProgramOutcome::Success(value) => value.value.clone(),
        task::ProgramOutcome::Failure(value) => value.value.clone(),
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Trapped(reason)) => {
            format!("trapped: {reason}")
        }
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Killed) => String::from("killed"),
    }
}

struct Eo9MainExport;

impl Guest for Eo9MainExport {
    async fn main(prog: Component) -> Result<ProgramSuccess, ProgramFailure> {
        let clock = time::default();

        // Compile, timed separately: on-target codegen is the dominant cold cost and
        // deserves its own line (the executors announce it too).
        let t0 = now_ns(&clock);
        let opts = compile::CompileOpts {
            debug_info: false,
            safepoint_maps: false,
        };
        let image = compile::compile(prog, opts)
            .map_err(|err| ProgramFailure::Compile(format!("{err:?}")))?;
        let t1 = now_ns(&clock);

        // Spawn and wait. The child runs with the executor's standard child
        // environment — `time` passes the program exactly as it received it (the
        // detach rule: as composed, never more).
        let limits = task::SpawnLimits { max_memory: None };
        let child = task::spawn(&image, &[], alloc::vec::Vec::new(), limits)
            .map_err(|err| ProgramFailure::Spawn(format!("{err:?}")))?;
        let outcome = task::wait(&child).await;
        let t2 = now_ns(&clock);

        let report = format!(
            "compile {}  real {}",
            render(t1.saturating_sub(t0)),
            render(t2.saturating_sub(t1))
        );
        eo9_guest::text::write_out_line(&report)
            .map_err(|err| ProgramFailure::Io(format!("{err:?}")))?;

        let rendered = render_outcome(&outcome);
        match outcome {
            task::ProgramOutcome::Success(_) => Ok(ProgramSuccess::Timed(rendered)),
            _ => Err(ProgramFailure::ChildFailed(rendered)),
        }
    }
}

bindings::export!(Eo9MainExport with_types_in bindings);
