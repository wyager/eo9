//! `eo9 run`: resolve a program, compile it through the compile cache, spawn it with the
//! unix root providers, drive it to completion, and print its outcome as WAVE.
//!
//! The drive loop is the simple built-in one from plan/11-usermode.md milestone 1: it
//! donates fuel in fixed slices, blocks the calling thread when the task is waiting on
//! host I/O, and stops when `main` finishes or the task traps. Adopting `eo9-sched`'s
//! run-queue machinery (so many tasks can share the CPU under a real policy) is later
//! work — with a single foreground task there is nothing for a scheduler to decide yet.

use eo9_component::{ArgSpec, Component, ComponentKind, ImportNeed};
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{NamedArg, Outcome, ResumeOutcome, SpawnLimits, Task, WaveValue};

use crate::cli::{
    Config, EXIT_ABNORMAL, EXIT_FAILURE, EXIT_SUCCESS, OutcomeChannel, ProgramArg, vlog,
};
use crate::compile;
use crate::providers;
use crate::source;

/// Fuel donated per resume by the built-in drive loop. The loop keeps donating until the
/// program finishes, so this is a scheduling granule, not a budget.
const RESUME_DONATION: u64 = 100 * FUEL_QUANTUM;

pub fn cmd_run(cfg: &Config, reference: &str, flags: &[ProgramArg]) -> Result<u8, String> {
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
    // The filesystem is never granted implicitly: a program that *requires* eo9:fs needs
    // an explicit `--fs-root` grant, and saying so here beats the raw linker error.
    // Optional fs imports simply observe absence (the runtime auto-seals them).
    if cfg.fs_root.is_none() && requires_fs(&info.imports) {
        return Err(format!(
            "{} requires the eo9:fs filesystem capability, which eo9 does not grant by \
             default: pass `--fs-root <dir>` to give the program access to a host \
             directory (guest paths cannot escape that root)",
            source.origin
        ));
    }
    let args = bind_args(&info.args, flags)?;

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
    let mut task = Task::spawn(
        &loaded.image,
        &args,
        limits,
        providers::root_providers(cfg)?,
    )
    .map_err(|err| {
        stale_store_hint(
            &source.origin,
            format!("cannot spawn {}: {err}", source.origin),
        )
    })?;

    let outcome = drive_to_completion(cfg, &mut task);
    if let Outcome::Success(value) | Outcome::Failure(value) = &outcome
        && !value.ty.is_empty()
    {
        vlog!(cfg, "outcome payload type: {}", value.ty);
    }

    let (rendered, code) = render_outcome(&outcome);
    print_outcome(cfg, &rendered);
    Ok(code)
}

/// When spawning a **store-resolved** component fails because its shape does not match
/// what this runtime links — a missing interface instance or resource implementation —
/// the most common cause is a binding made by an older eo9 (the bundled programs are
/// auto-refreshed, but a name the user once re-bound, or a store the refresh chose not
/// to touch, can still hold old bytes). Point at the recovery command instead of leaving
/// only the raw linker text.
pub(crate) fn stale_store_hint(origin: &str, message: String) -> String {
    let looks_like_a_shape_mismatch = [
        "resource implementation is missing",
        "implementation is missing",
        "matching implementation",
        "imports instance",
        "unknown import",
    ]
    .iter()
    .any(|needle| message.contains(needle));
    if origin.contains("(store object") && looks_like_a_shape_mismatch {
        format!(
            "{message} (this component may have been built for an older eo9 — try \
             `eo9 store reseed`)"
        )
    } else {
        message
    }
}

/// Write the rendered outcome line to the channel selected by `--outcome` (stderr by
/// default: program output owns stdout, the exit code already encodes the outcome).
pub(crate) fn print_outcome(cfg: &Config, rendered: &str) {
    match cfg.outcome {
        OutcomeChannel::Stderr => eprintln!("{rendered}"),
        OutcomeChannel::Stdout => println!("{rendered}"),
        OutcomeChannel::Quiet => {}
    }
}

/// The built-in drive loop: donate fuel, run, park the thread on I/O, repeat until the
/// task finishes. Shared by `eo9 run` and `eo9 shell`.
pub(crate) fn drive_to_completion(cfg: &Config, task: &mut Task) -> Outcome {
    let mut resumes: u64 = 0;
    let mut donated: u64 = 0;
    let outcome = loop {
        // `--max-fuel`: a hard budget on donated fuel. When the budget is exhausted the
        // task is killed (the run ends as `abnormal(killed)`) instead of spinning forever
        // on a busy loop (user-study finding: CPU was the weakest limit).
        if let Some(max_fuel) = cfg.max_fuel
            && donated.saturating_sub(task.unspent_fuel()) >= max_fuel
        {
            vlog!(cfg, "fuel budget of {max_fuel} exhausted; killing the task");
            break task.kill_in_place();
        }
        resumes += 1;
        donated += RESUME_DONATION;
        match task.resume(RESUME_DONATION) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => {}
            ResumeOutcome::Blocked => providers::wait_until_runnable(task),
        }
    };
    vlog!(
        cfg,
        "task finished after {resumes} resume donation(s) of {RESUME_DONATION} fuel"
    );
    outcome
}

/// Bind the program's command-line arguments to `main`'s parameters.
///
/// * `--flag value` pairs bind by name: a flag filling a `string`-typed parameter is taken
///   literally and WAVE-quoted here; a flag filling a `list<string>` parameter with a
///   value that isn't already WAVE list syntax is wrapped as a one-element list; every
///   other value is passed through as WAVE text.
/// * Bare positional values fill the still-unfilled parameters in declaration order
///   (string parameters quoted, everything else passed through).
/// * When `main`'s **final** parameter is `list<string>` it is variadic: positional values
///   left over once the other parameters are filled are collected into it (so
///   `cat a.txt b.txt` works), and it defaults to the empty list when nothing fills it.
///
/// Unknown, duplicate, or type-mismatched arguments are reported by the runtime's
/// spawn-time check against the signature; "more positionals than parameters" is the one
/// error that has to be reported here, because such a value has no name to carry.
fn bind_args(params: &[ArgSpec], flags: &[ProgramArg]) -> Result<Vec<NamedArg>, String> {
    let variadic = params
        .last()
        .filter(|param| param.ty.trim() == "list<string>");
    let mut named: Vec<NamedArg> = Vec::new();
    let mut variadic_values: Vec<String> = Vec::new();

    for arg in flags {
        match arg {
            ProgramArg::Flag { name, value } => {
                let ty = params
                    .iter()
                    .find(|param| param.name == *name)
                    .map(|param| param.ty.trim());
                let encoded = match ty {
                    Some("string") => wave_string(value),
                    Some("list<string>") if !value.trim_start().starts_with('[') => {
                        format!("[{}]", wave_string(value))
                    }
                    _ => value.clone(),
                };
                named.push(NamedArg::new(name.clone(), encoded));
            }
            ProgramArg::Positional(value) => {
                let next_unfilled = params
                    .iter()
                    .filter(|param| variadic.map(|v| v.name != param.name).unwrap_or(true))
                    .find(|param| !named.iter().any(|arg| arg.name == param.name));
                match (next_unfilled, variadic) {
                    (Some(param), _) => {
                        let encoded = if param.ty.trim() == "string" {
                            wave_string(value)
                        } else {
                            value.clone()
                        };
                        named.push(NamedArg::new(param.name.clone(), encoded));
                    }
                    (None, Some(_)) => variadic_values.push(wave_string(value)),
                    (None, None) => {
                        return Err(format!(
                            "unexpected positional argument {value:?}: this program's \
                             parameters are already filled (pass values as `--<flag> <value>`)"
                        ));
                    }
                }
            }
        }
    }

    if let Some(param) = variadic
        && !named.iter().any(|arg| arg.name == param.name)
    {
        named.push(NamedArg::new(
            param.name.clone(),
            format!("[{}]", variadic_values.join(", ")),
        ));
    } else if !variadic_values.is_empty() {
        return Err(format!(
            "positional arguments and `--{}` were both given; use one or the other",
            variadic.map(|param| param.name.as_str()).unwrap_or("flag")
        ));
    }
    Ok(named)
}

/// Whether the component has a *required* import of an `eo9:fs` interface — i.e. it
/// cannot run without an explicit `--fs-root` grant. Optional fs imports do not count:
/// the runtime seals those with absence.
fn requires_fs(imports: &[ImportNeed]) -> bool {
    imports
        .iter()
        .any(|need| need.required && !need.authority_free && need.interface.starts_with("eo9:fs/"))
}

/// Render the executor's view of how the task ended as the spec's three-way
/// `program-outcome` (success / failure / abnormal) in WAVE, plus the process exit code.
pub(crate) fn render_outcome(outcome: &Outcome) -> (String, u8) {
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
pub(crate) fn wave_string(text: &str) -> String {
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

    fn flag(name: &str, value: &str) -> ProgramArg {
        ProgramArg::Flag {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    fn positional(value: &str) -> ProgramArg {
        ProgramArg::Positional(value.to_string())
    }

    #[test]
    fn string_parameters_take_their_flag_text_literally() {
        let params = [spec("name", "string"), spec("excited", "bool")];
        let flags = [flag("name", "world"), flag("excited", "true")];
        let args = bind_args(&params, &flags).unwrap();
        assert_eq!(args[0], NamedArg::new("name", "\"world\""));
        assert_eq!(args[1], NamedArg::new("excited", "true"));
    }

    #[test]
    fn unknown_flags_pass_through_for_the_runtime_to_reject() {
        let params = [spec("seed", "u64")];
        let flags = [flag("nonsense", "1")];
        let args = bind_args(&params, &flags).unwrap();
        assert_eq!(args[0], NamedArg::new("nonsense", "1"));
    }

    #[test]
    fn positionals_fill_parameters_in_declaration_order() {
        let params = [spec("name", "string"), spec("excited", "bool")];
        let args = bind_args(&params, &[positional("world"), positional("true")]).unwrap();
        assert_eq!(args[0], NamedArg::new("name", "\"world\""));
        assert_eq!(args[1], NamedArg::new("excited", "true"));

        // Named flags win their parameter; positionals take what is left.
        let args = bind_args(&params, &[flag("excited", "true"), positional("world")]).unwrap();
        assert_eq!(args[0], NamedArg::new("excited", "true"));
        assert_eq!(args[1], NamedArg::new("name", "\"world\""));

        // Too many positionals with nowhere to go is an error here (no name to carry).
        assert!(
            bind_args(
                &params,
                &[positional("a"), positional("b"), positional("c")]
            )
            .is_err()
        );
    }

    #[test]
    fn a_final_list_of_strings_parameter_is_variadic() {
        let params = [spec("paths", "list<string>")];
        let args = bind_args(&params, &[positional("a.txt"), positional("b.txt")]).unwrap();
        assert_eq!(args, vec![NamedArg::new("paths", "[\"a.txt\", \"b.txt\"]")]);

        // No values at all: the variadic parameter defaults to the empty list.
        let args = bind_args(&params, &[]).unwrap();
        assert_eq!(args, vec![NamedArg::new("paths", "[]")]);

        // Earlier parameters are filled first; the rest spill into the variadic tail.
        let params = [spec("lines", "u64"), spec("paths", "list<string>")];
        let args = bind_args(
            &params,
            &[flag("lines", "2"), positional("a.txt"), positional("b.txt")],
        )
        .unwrap();
        assert_eq!(args[0], NamedArg::new("lines", "2"));
        assert_eq!(args[1], NamedArg::new("paths", "[\"a.txt\", \"b.txt\"]"));

        // A named flag for the list parameter coerces a single bare value into a list,
        // and then mixing in positionals is rejected as ambiguous.
        let params = [spec("paths", "list<string>")];
        let args = bind_args(&params, &[flag("paths", "a.txt")]).unwrap();
        assert_eq!(args, vec![NamedArg::new("paths", "[\"a.txt\"]")]);
        let args = bind_args(&params, &[flag("paths", "[\"a.txt\"]")]).unwrap();
        assert_eq!(args, vec![NamedArg::new("paths", "[\"a.txt\"]")]);
        assert!(bind_args(&params, &[flag("paths", "a.txt"), positional("b.txt")]).is_err());
    }

    #[test]
    fn only_required_fs_imports_demand_an_fs_root_grant() {
        let need = |interface: &str, required: bool| ImportNeed {
            slot: interface.to_string(),
            interface: interface.to_string(),
            version: "0.1.0".to_string(),
            required,
            authority_free: false,
        };
        // Required fs demands a grant.
        assert!(requires_fs(&[need("eo9:fs/fs", true)]));
        assert!(requires_fs(&[
            need("eo9:text/text", true),
            need("eo9:fs/fs", true),
        ]));
        // A types-only use of the fs interface (no functions) carries no authority and
        // does not demand a grant.
        assert!(!requires_fs(&[ImportNeed {
            slot: "eo9:fs/fs".to_string(),
            interface: "eo9:fs/fs".to_string(),
            version: "0.1.0".to_string(),
            required: true,
            authority_free: true,
        }]));
        // Optional fs is sealed with absence; other APIs never demand a grant.
        assert!(!requires_fs(&[need("eo9:fs/fs", false)]));
        assert!(!requires_fs(&[
            need("eo9:text/text", true),
            need("eo9:time/time", true),
        ]));
        assert!(!requires_fs(&[]));
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
