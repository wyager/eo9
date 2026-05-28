//! The page's HTTP-backed program store and the runner for real Eo9 programs.
//!
//! `cargo xtask build-web-vm` pre-AOTs the example programs (and the kernel's async sleep
//! canary) to `pulley32` and installs them under `www/site/vm/store/`; the blob fetches an
//! artifact on demand (a JSPI-suspending `fetch`), deserializes it with the same engine
//! configuration, links the browser root providers, and runs the program — `main` with
//! typed arguments and a rendered three-way outcome, exactly the shape native `eo9 run`
//! and the bare-metal kernel use.

use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use wasmtime::Store;
use wasmtime::component::{Component, Linker, Val};

use crate::providers::{self, WebState};
use crate::{block_on, engine, host};

/// Argument shapes for the programs shipped in the page store. (The blob keeps this small
/// table rather than reflecting the component's type — these are the demo programs the
/// site serves; anything else is reported as unknown.)
const PROGRAMS: &[(&str, &[ArgKind])] = &[
    ("hello", &[ArgKind::Text, ArgKind::Flag]),
    ("cruncher", &[ArgKind::Number, ArgKind::Number]),
    ("outcomes", &[ArgKind::Text, ArgKind::Text]),
];

#[derive(Clone, Copy)]
enum ArgKind {
    Text,
    Flag,
    Number,
}

fn parse_args(kinds: &[ArgKind], raw: &[&str]) -> Result<Vec<Val>, String> {
    if raw.len() != kinds.len() {
        return Err(format!(
            "expected {} argument(s), got {}",
            kinds.len(),
            raw.len()
        ));
    }
    kinds
        .iter()
        .zip(raw)
        .map(|(kind, value)| match kind {
            ArgKind::Text => Ok(Val::String(value.to_string())),
            ArgKind::Flag => match *value {
                "true" => Ok(Val::Bool(true)),
                "false" => Ok(Val::Bool(false)),
                other => Err(format!("`{other}` is not a bool (use true/false)")),
            },
            ArgKind::Number => value
                .parse::<u64>()
                .map(Val::U64)
                .map_err(|_| format!("`{value}` is not an unsigned integer")),
        })
        .collect()
}

/// Render a component value the way the native CLI renders outcomes (close enough for the
/// terminal: variant names with payloads, strings quoted).
fn render_val(value: &Val) -> String {
    match value {
        Val::Bool(v) => v.to_string(),
        Val::U8(v) => v.to_string(),
        Val::S8(v) => v.to_string(),
        Val::U16(v) => v.to_string(),
        Val::S16(v) => v.to_string(),
        Val::U32(v) => v.to_string(),
        Val::S32(v) => v.to_string(),
        Val::U64(v) => v.to_string(),
        Val::S64(v) => v.to_string(),
        Val::Float32(v) => v.to_string(),
        Val::Float64(v) => v.to_string(),
        Val::Char(v) => format!("'{v}'"),
        Val::String(v) => format!("{v:?}"),
        Val::List(items) => {
            let inner: Vec<String> = items.iter().map(render_val).collect();
            format!("[{}]", inner.join(", "))
        }
        Val::Record(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", render_val(value)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Val::Tuple(items) => {
            let inner: Vec<String> = items.iter().map(render_val).collect();
            format!("({})", inner.join(", "))
        }
        Val::Variant(name, payload) => match payload {
            Some(payload) => format!("{name}({})", render_val(payload)),
            None => name.clone(),
        },
        Val::Enum(name) => name.clone(),
        Val::Option(value) => match value {
            Some(value) => format!("some({})", render_val(value)),
            None => "none".to_string(),
        },
        Val::Result(result) => match result {
            Ok(Some(value)) => format!("success({})", render_val(value)),
            Ok(None) => "success".to_string(),
            Err(Some(value)) => format!("failure({})", render_val(value)),
            Err(None) => "failure".to_string(),
        },
        Val::Flags(flags) => format!("{{{}}}", flags.join(", ")),
        // Handles and other opaque values (resources, futures, streams, maps) — none of the
        // page-store programs return these from `main`; show the debug form if one ever does.
        other => format!("{other:?}"),
    }
}

/// Deserialize a fetched artifact and prepare an instance linked against the browser root
/// providers.
fn instantiate(
    artifact: &[u8],
) -> wasmtime::Result<(Store<WebState>, wasmtime::component::Instance)> {
    let engine = engine(false)?;
    // SAFETY: produced by `cargo xtask build-web-vm` with the matching configuration and
    // served from the same origin as the blob itself.
    let component = unsafe { Component::deserialize(&engine, artifact)? };
    let mut linker: Linker<WebState> = Linker::new(&engine);
    providers::add_providers(&mut linker)?;
    let mut store = Store::new(&engine, WebState::new());
    let instance = linker.instantiate(&mut store, &component)?;
    Ok((store, instance))
}

fn top_level_func(
    store: &mut Store<WebState>,
    instance: &wasmtime::component::Instance,
    name: &str,
) -> wasmtime::Result<wasmtime::component::Func> {
    let index = instance
        .get_export_index(&mut *store, None, name)
        .ok_or_else(|| wasmtime::Error::msg(format!("the program does not export `{name}`")))?;
    instance
        .get_func(&mut *store, index)
        .ok_or_else(|| wasmtime::Error::msg(format!("`{name}` is not a function")))
}

/// Run the kernel's async sleep canary (`sleepy`): a hand-written component whose `run`
/// export uses the Component Model's **stackful** async lift (it blocks mid-guest-frame on
/// a sync-lowered `time.sleep`). The bare-metal kernel runs it on its fiber backend; this
/// wasm32 host has no fibers, so the fiberless path refuses the stackful shape — which the
/// page reports honestly. Eo9's real guests use the callback ABI and do run here (see
/// [`run_program`]); the awaited-timer mechanics themselves are demonstrated by
/// `probe_sleep` and by `read-line`.
pub fn run_sleepy() -> wasmtime::Result<()> {
    fn attempt() -> wasmtime::Result<(u64, u64)> {
        let artifact = host::fetch_artifact("sleepy").map_err(wasmtime::Error::msg)?;
        let (mut store, instance) = instantiate(&artifact)?;
        let run = top_level_func(&mut store, &instance, "run")?;

        let started = host::monotonic_ns();
        let elapsed_guest = block_on(
            "sleepy.run",
            store.run_concurrent(async move |accessor| -> wasmtime::Result<u64> {
                let mut result = [Val::Bool(false)];
                run.call_concurrent(accessor, &[], &mut result).await?;
                match result[0] {
                    Val::U64(value) => Ok(value),
                    ref other => Err(wasmtime::Error::msg(format!(
                        "sleepy.run returned an unexpected value: {other:?}"
                    ))),
                }
            }),
        )???;
        Ok((elapsed_guest, host::monotonic_ns().saturating_sub(started)))
    }

    match attempt() {
        Ok((elapsed_guest, elapsed_here)) => {
            crate::outf!(
                "sleepy.run() measured {:.1} ms across its await; the page measured {:.1} ms",
                elapsed_guest as f64 / 1_000_000.0,
                elapsed_here as f64 / 1_000_000.0
            );
            Ok(())
        }
        Err(error) => {
            crate::outf!(
                "sleepy uses the Component Model's *stackful* async lift — it blocks in the \
                 middle of a guest frame, which needs a fiber backend. wasm32 has none, so the \
                 fiberless web VM cannot run that shape yet (the bare-metal kernel can)."
            );
            crate::outf!(
                "Eo9's own guests use the callback ABI instead, and those run here — see the \
                 program store above; the awaited-timer / awaited-input mechanics are the \
                 \"park the VM\" and \"read a line\" demos."
            );
            crate::outf!("(wasmtime's refusal: {error})");
            Err(wasmtime::Error::msg(
                "stackful async lift is not runnable on the fiberless wasm32 host",
            ))
        }
    }
}

/// Fetch and run one of the page store's example programs with typed arguments.
pub fn run_program(name: &str, raw_args: &[&str]) -> wasmtime::Result<()> {
    let Some((_, kinds)) = PROGRAMS.iter().find(|(known, _)| *known == name) else {
        return Err(wasmtime::Error::msg(format!(
            "`{name}` is not one of the programs this page serves"
        )));
    };
    let args = parse_args(kinds, raw_args).map_err(wasmtime::Error::msg)?;

    let artifact = host::fetch_artifact(name).map_err(wasmtime::Error::msg)?;
    crate::outf!(
        "{name}: fetched {} bytes of pulley32 artifact from /vm/store/, instantiating against \
         the browser root providers (text -> this terminal, time -> the browser clocks, \
         entropy -> crypto.getRandomValues)",
        artifact.len()
    );
    let (mut store, instance) = instantiate(&artifact)?;
    let main = top_level_func(&mut store, &instance, "main")?;

    let started = host::monotonic_ns();
    let outcome = block_on(
        "main",
        store.run_concurrent(async move |accessor| -> wasmtime::Result<Val> {
            let mut result = [Val::Bool(false)];
            main.call_concurrent(accessor, &args, &mut result).await?;
            Ok(result[0].clone())
        }),
    )???;
    let elapsed = host::monotonic_ns().saturating_sub(started);

    crate::outf!("{name}: outcome = {}", render_val(&outcome));
    crate::outf!("{name}: ran in {:.1} ms (Pulley interpreter)", elapsed as f64 / 1_000_000.0);
    Ok(())
}
