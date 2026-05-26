//! `eo9 run`: resolve a program, compile it through the compile cache, spawn it with the
//! unix root providers, drive it to completion, and print its outcome as WAVE.
//!
//! The drive loop is the simple built-in one from plan/11-usermode.md milestone 1: it
//! donates fuel in fixed slices, blocks the calling thread when the task is waiting on
//! host I/O, and stops when `main` finishes or the task traps. Adopting `eo9-sched`'s
//! run-queue machinery (so many tasks can share the CPU under a real policy) is later
//! work — with a single foreground task there is nothing for a scheduler to decide yet.

use eo9_component::{ArgSpec, Component, ComponentKind};
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{NamedArg, Outcome, ResumeOutcome, SpawnLimits, Task, WaveValue};

use crate::cli::{Config, EXIT_ABNORMAL, EXIT_FAILURE, EXIT_SUCCESS, vlog};
use crate::compile;
use crate::providers;
use crate::source;

/// Fuel donated per resume by the built-in drive loop. The loop keeps donating until the
/// program finishes, so this is a scheduling granule, not a budget.
const RESUME_DONATION: u64 = 100 * FUEL_QUANTUM;

pub fn cmd_run(cfg: &Config, reference: &str, flags: &[(String, String)]) -> Result<u8, String> {
    let source = source::resolve_program(cfg, reference)?;

    // The argument signature drives flag handling (SPEC.md "Type-directed arguments"):
    // a flag filling a string-typed parameter is taken literally, everything else is
    // WAVE text checked against the signature at spawn.
    let info = Component::load(source.bytes.clone())
        .map_err(|err| format!("{}: not a loadable component: {err}", source.origin))?
        .describe();
    if info.kind == ComponentKind::Provider {
        return Err(format!(
            "{} is a provider, not a binary: providers are composed (`$`), never run",
            source.origin
        ));
    }
    let args = bind_args(&info.args, flags);

    // Obtain the image through the compile cache: a hit is deserialized without codegen,
    // a miss compiles once and caches the very image that runs below (plan 06).
    let store = cfg.open_store()?;
    let loaded = compile::load_image(cfg, &store, &source)?;

    // Spawn against the unix root providers. Only interfaces the component imports are
    // linked; an import beyond what the runtime can provide is rejected here.
    let limits = SpawnLimits {
        max_memory: cfg.max_memory,
        max_table_elements: None,
    };
    let mut task = Task::spawn(&loaded.image, &args, limits, providers::root_providers())
        .map_err(|err| format!("cannot spawn {}: {err}", source.origin))?;

    // The built-in drive loop: donate, run, park on I/O, repeat.
    let mut resumes: u64 = 0;
    let outcome = loop {
        resumes += 1;
        match task.resume(RESUME_DONATION) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => {}
            ResumeOutcome::Blocked => providers::wait_until_runnable(&task),
        }
    };
    vlog!(
        cfg,
        "task finished after {resumes} resume donation(s) of {RESUME_DONATION} fuel"
    );
    if let Outcome::Success(value) | Outcome::Failure(value) = &outcome
        && !value.ty.is_empty()
    {
        vlog!(cfg, "outcome payload type: {}", value.ty);
    }

    let (rendered, code) = render_outcome(&outcome);
    println!("{rendered}");
    Ok(code)
}

/// Bind `--flag value` pairs to `main`'s parameters. A flag filling a `string`-typed
/// parameter is taken literally and WAVE-quoted here; every other value is passed through
/// as WAVE text. Unknown, duplicate, or missing arguments are reported by the runtime's
/// spawn-time type check against the signature.
fn bind_args(params: &[ArgSpec], flags: &[(String, String)]) -> Vec<NamedArg> {
    flags
        .iter()
        .map(|(name, raw)| {
            let is_string = params
                .iter()
                .any(|param| param.name == *name && param.ty == "string");
            let value = if is_string {
                wave_string(raw)
            } else {
                raw.clone()
            };
            NamedArg::new(name.clone(), value)
        })
        .collect()
}

/// Render the executor's view of how the task ended as the spec's three-way
/// `program-outcome` (success / failure / abnormal) in WAVE, plus the process exit code.
fn render_outcome(outcome: &Outcome) -> (String, u8) {
    match outcome {
        Outcome::Success(value) => (render_arm("success", value), EXIT_SUCCESS),
        Outcome::Failure(value) => (render_arm("failure", value), EXIT_FAILURE),
        Outcome::Trapped(reason) => (
            format!("abnormal(trapped({}))", wave_string(reason)),
            EXIT_ABNORMAL,
        ),
        Outcome::Killed => ("abnormal(killed)".to_string(), EXIT_ABNORMAL),
    }
}

fn render_arm(arm: &str, value: &WaveValue) -> String {
    if value.value.is_empty() {
        arm.to_string()
    } else {
        format!("{arm}({})", value.value)
    }
}

/// Encode text as a WAVE string literal.
fn wave_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{{{:x}}}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, ty: &str) -> ArgSpec {
        ArgSpec {
            name: name.to_string(),
            ty: ty.to_string(),
        }
    }

    #[test]
    fn wave_strings_are_quoted_and_escaped() {
        assert_eq!(wave_string("eo9"), "\"eo9\"");
        assert_eq!(wave_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(wave_string("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(wave_string("line\nbreak"), "\"line\\u{a}break\"");
    }

    #[test]
    fn string_parameters_take_their_flag_text_literally() {
        let params = [spec("name", "string"), spec("excited", "bool")];
        let flags = [
            ("name".to_string(), "world".to_string()),
            ("excited".to_string(), "true".to_string()),
        ];
        let args = bind_args(&params, &flags);
        assert_eq!(args[0], NamedArg::new("name", "\"world\""));
        assert_eq!(args[1], NamedArg::new("excited", "true"));
    }

    #[test]
    fn unknown_flags_pass_through_for_the_runtime_to_reject() {
        let params = [spec("seed", "u64")];
        let flags = [("nonsense".to_string(), "1".to_string())];
        let args = bind_args(&params, &flags);
        assert_eq!(args[0], NamedArg::new("nonsense", "1"));
    }

    #[test]
    fn outcomes_map_to_the_documented_exit_codes() {
        let success = Outcome::Success(WaveValue {
            ty: "variant { greeted }".to_string(),
            value: "greeted".to_string(),
        });
        assert_eq!(
            render_outcome(&success),
            ("success(greeted)".to_string(), 0)
        );

        let empty_success = Outcome::Success(WaveValue {
            ty: String::new(),
            value: String::new(),
        });
        assert_eq!(render_outcome(&empty_success), ("success".to_string(), 0));

        let failure = Outcome::Failure(WaveValue {
            ty: "string".to_string(),
            value: "\"boom\"".to_string(),
        });
        assert_eq!(
            render_outcome(&failure),
            ("failure(\"boom\")".to_string(), 1)
        );

        let trapped = Outcome::Trapped("wasm trap: unreachable".to_string());
        assert_eq!(
            render_outcome(&trapped),
            (
                "abnormal(trapped(\"wasm trap: unreachable\"))".to_string(),
                2
            )
        );

        assert_eq!(
            render_outcome(&Outcome::Killed),
            ("abnormal(killed)".to_string(), 2)
        );
    }
}
