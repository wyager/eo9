//! Boot program selection: run a named component from the baked-in store, or eosh.
//!
//! The kernel command line (QEMU `-append`, surfaced through `/chosen/bootargs` — see
//! `crate::fdt`) selects what to run at boot:
//!
//! * `program=<name> [arg=value …]` — run that store entry headless against the kernel
//!   root providers, print its outcome, and power off (`program=eosh` starts the shell).
//! * `demo` — run the original demo sequence (seed canary, hello, the async demos).
//! * nothing — boot to the interactive eosh shell on the serial console.
//!
//! Headless arguments are matched against `main`'s named, typed parameters (the same
//! convention as `eo9 run` in usermode): `name="bare metal" excited=true`. The kernel
//! parses the scalar types (strings, booleans, integers, floats, chars); anything richer
//! needs the WAVE machinery and is reported as unsupported rather than guessed at.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use wasmtime::Store;
use wasmtime::component::{Component, Linker, Type, Val};

use super::providers::{self, KernelState};
use super::store::{StoreEntry, StoreImage};

/// The store image assembled and injected by `cargo xtask build-kernel <arch>`.
static STORE_IMAGE: &[u8] = include_bytes!(env!("EO9_STORE_IMAGE"));

/// Parse the boot arguments and run what they select. Returns `true` when the boot was
/// handled here (a headless program or the shell ran), `false` when the caller should run
/// the default demo sequence instead (the `demo` token, or a store image that fails to
/// parse).
pub fn boot(bootargs: Option<&str>) -> bool {
    let entries = match StoreImage::parse_static(STORE_IMAGE) {
        Ok(entries) => entries,
        Err(error) => {
            crate::kprintln!("store: FAILED to parse the baked-in image: {error}");
            return false;
        }
    };
    let names: Vec<&str> = entries.iter().map(|entry| entry.name).collect();
    let component_bytes: usize = entries.iter().map(|e| e.component.len()).sum();
    let artifact_bytes: usize = entries.iter().map(|e| e.artifact.len()).sum();
    crate::kprintln!(
        "store: {} components baked in ({} KiB components, {} KiB artifacts): {}",
        names.len(),
        component_bytes / 1024,
        artifact_bytes / 1024,
        names.join(", ")
    );

    let bootargs = bootargs.unwrap_or("");
    let (program, args) = parse_command_line(bootargs);

    // The bare `demo` token keeps the original boot sequence reachable:
    // `cargo xtask qemu aarch64 demo`.
    if program.is_none() && tokenize(bootargs).iter().any(|token| token == "demo") {
        return false;
    }

    match program.as_deref() {
        // The default boot program is the shell; `program=eosh` spells the same thing.
        None | Some("eosh") => {
            super::shell::boot_to_eosh(entries);
        }
        Some(program) => {
            crate::kprintln!("runner: selected `{program}` from the kernel command line");
            match entries.iter().find(|entry| entry.name == program) {
                Some(entry) => run_entry(entry, &args),
                None => crate::kprintln!(
                    "runner: `{program}` is not in the baked-in store (have: {})",
                    names.join(", ")
                ),
            }
        }
    }
    true
}

/// Split `/chosen/bootargs` into the selected program and its `key=value` arguments.
///
/// Tokens are whitespace-separated; a value may be double-quoted to contain spaces
/// (`name="bare metal"`). The `program=<name>` token selects the store entry; every
/// other `key=value` token becomes a named argument. Tokens without `=` are ignored.
fn parse_command_line(bootargs: &str) -> (Option<String>, Vec<(String, String)>) {
    let mut program = None;
    let mut args = Vec::new();
    for token in tokenize(bootargs) {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        let value = unquote(value);
        if key == "program" {
            program = Some(value);
        } else {
            args.push((key.to_string(), value));
        }
    }
    (program, args)
}

/// Whitespace tokenizer that keeps double-quoted spans (including their quotes) intact.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(core::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip one set of surrounding double quotes, if present.
fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

/// Run one store entry headless and report its outcome over serial.
fn run_entry(entry: &StoreEntry, args: &[(String, String)]) {
    crate::kprintln!(
        "runner: {} ({} byte artifact) with kernel text/time/entropy providers",
        entry.name,
        entry.artifact.len()
    );
    let start_us = crate::timer::uptime_us();
    match try_run(entry, args) {
        Ok(outcome) => {
            let elapsed_us = crate::timer::uptime_us() - start_us;
            crate::kprintln!("runner: {} outcome = {outcome}", entry.name);
            crate::kprintln!("runner: instantiate + main took {elapsed_us} us");
        }
        Err(error) => crate::kprintln!("runner: {} FAILED: {error:?}", entry.name),
    }
}

fn try_run(entry: &StoreEntry, args: &[(String, String)]) -> Result<String, wasmtime::Error> {
    // `max-fuel=<units>` is an option of the runner itself — the headless counterpart of
    // usermode `eo9 run --max-fuel` — not an argument of the program: a hard budget on the
    // run's fuel; exhausting it ends the run with `abnormal(killed)`.
    let mut max_fuel: Option<u64> = None;
    let mut program_args: Vec<(String, String)> = Vec::new();
    for (key, value) in args {
        if key == "max-fuel" {
            max_fuel = Some(value.parse().map_err(|err| {
                wasmtime::Error::msg(format!(
                    "invalid max-fuel value `{value}` (fuel units expected): {err}"
                ))
            })?);
        } else {
            program_args.push((key.clone(), value.clone()));
        }
    }

    let engine = super::new_engine()?;

    // SAFETY: the artifact comes from the store image produced by `cargo xtask
    // build-kernel` with the same wasmtime version and engine configuration, embedded
    // read-only in the kernel image.
    let component = unsafe { Component::deserialize(&engine, entry.artifact)? };

    let mut linker: Linker<KernelState> = Linker::new(&engine);
    providers::add_providers(&mut linker)?;

    let mut store = Store::new(&engine, KernelState::new());
    // The engine meters fuel (see `new_engine`). A headless run gets the whole budget in
    // one pool: effectively unlimited by default, or exactly `max-fuel=<units>` when given.
    // No yield interval here — there is nothing to interleave with, and the long-standing
    // executor watchdog applies to wedged (pending) operations, not to running guest code.
    store.set_fuel(max_fuel.unwrap_or(u64::MAX))?;
    let instance = super::block_on(
        "instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let main = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| wasmtime::Error::msg("component does not export `main`"))?;
    let signature = main.ty(&store);

    let params = build_params(&signature, &program_args).map_err(wasmtime::Error::msg)?;
    let mut results: Vec<Val> = signature.results().map(|_| Val::Bool(false)).collect();
    let call = super::block_on("main()", main.call_async(&mut store, &params, &mut results))?;
    if let Err(error) = call {
        // Out of fuel is the budget being enforced, not a failure of the runner: report it
        // the way usermode reports an exhausted `--max-fuel` budget (abnormal / killed).
        if matches!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(wasmtime::Trap::OutOfFuel)
        ) {
            let budget = max_fuel.unwrap_or(u64::MAX);
            return Ok(format!(
                "abnormal(killed) — the fuel budget of {budget} units was exhausted"
            ));
        }
        return Err(error);
    }

    Ok(results
        .first()
        .map(render_outcome)
        .unwrap_or_else(|| "(no result)".to_string()))
}

/// Match the command-line arguments against `main`'s named, typed parameters.
fn build_params(
    signature: &wasmtime::component::types::ComponentFunc,
    args: &[(String, String)],
) -> Result<Vec<Val>, String> {
    let mut params = Vec::new();
    let mut used = alloc::vec![false; args.len()];
    for (name, ty) in signature.params() {
        let position = args.iter().position(|(key, _)| key == name);
        match position {
            Some(index) => {
                used[index] = true;
                let raw = &args[index].1;
                params.push(parse_scalar(&ty, raw).map_err(|err| {
                    format!("argument `{name}` (= `{raw}`) could not be parsed: {err}")
                })?);
            }
            None => return Err(format!("missing argument `{name}`")),
        }
    }
    if let Some(index) = used.iter().position(|used| !used) {
        return Err(format!("unknown argument `{}`", args[index].0));
    }
    Ok(params)
}

/// Parse one scalar argument value according to its WIT type.
fn parse_scalar(ty: &Type, raw: &str) -> Result<Val, String> {
    fn int<T: core::str::FromStr>(raw: &str) -> Result<T, String>
    where
        T::Err: core::fmt::Display,
    {
        raw.parse::<T>().map_err(|err| err.to_string())
    }
    Ok(match ty {
        Type::String => Val::String(raw.to_string()),
        Type::Bool => match raw {
            "true" => Val::Bool(true),
            "false" => Val::Bool(false),
            _ => return Err("expected `true` or `false`".to_string()),
        },
        Type::Char => {
            let mut chars = raw.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Val::Char(ch),
                _ => return Err("expected exactly one character".to_string()),
            }
        }
        Type::U8 => Val::U8(int(raw)?),
        Type::U16 => Val::U16(int(raw)?),
        Type::U32 => Val::U32(int(raw)?),
        Type::U64 => Val::U64(int(raw)?),
        Type::S8 => Val::S8(int(raw)?),
        Type::S16 => Val::S16(int(raw)?),
        Type::S32 => Val::S32(int(raw)?),
        Type::S64 => Val::S64(int(raw)?),
        Type::Float32 => Val::Float32(int(raw)?),
        Type::Float64 => Val::Float64(int(raw)?),
        other => {
            return Err(format!(
                "the kernel runner only parses scalar argument types, not {other:?}"
            ));
        }
    })
}

/// Render `main`'s `result<program-success, program-failure>` value for the serial log,
/// mirroring the usermode runtime's success/failure vocabulary.
fn render_outcome(value: &Val) -> String {
    match value {
        Val::Result(Ok(payload)) => format!("success({})", render_payload(payload.as_deref())),
        Val::Result(Err(payload)) => format!("failure({})", render_payload(payload.as_deref())),
        other => format!("{other:?}"),
    }
}

fn render_payload(payload: Option<&Val>) -> String {
    match payload {
        None => String::new(),
        Some(Val::Variant(case, None)) => case.clone(),
        Some(Val::Variant(case, Some(inner))) => format!("{case}({})", render_scalar(inner)),
        Some(other) => render_scalar(other),
    }
}

/// Render a scalar payload value plainly (numbers and strings as themselves); anything
/// non-scalar falls back to the debug form.
fn render_scalar(value: &Val) -> String {
    match value {
        Val::Bool(v) => v.to_string(),
        Val::U8(v) => v.to_string(),
        Val::U16(v) => v.to_string(),
        Val::U32(v) => v.to_string(),
        Val::U64(v) => v.to_string(),
        Val::S8(v) => v.to_string(),
        Val::S16(v) => v.to_string(),
        Val::S32(v) => v.to_string(),
        Val::S64(v) => v.to_string(),
        Val::Float32(v) => v.to_string(),
        Val::Float64(v) => v.to_string(),
        Val::Char(v) => v.to_string(),
        Val::String(v) => format!("{v:?}"),
        other => format!("{other:?}"),
    }
}
