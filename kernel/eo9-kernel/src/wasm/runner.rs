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
//! The bare `pci` token (combinable with any of the above) grants the `eo9:pci` root
//! provider for the boot; without it a program importing PCI is refused at instantiation.
//!
//! Headless arguments are matched against `main`'s named, typed parameters (the same
//! convention as `eo9 run` in usermode): `name="bare metal" excited=true`. The kernel
//! parses the scalar types (strings, booleans, integers, floats, chars); anything richer
//! needs the WAVE machinery and is reported as unsupported rather than guessed at.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use wasmtime::Store;
use wasmtime::component::{Component, Linker, Type, Val};

use super::providers::{self, KernelState};
use super::store::{StoreEntry, StoreImage};

/// The store image assembled and injected by `cargo xtask build-kernel <arch>`.
static STORE_IMAGE: &[u8] = include_bytes!(env!("EO9_STORE_IMAGE"));

/// The default boot config: a serial console, no services. Out of the box the boot
/// pipeline (kernel → init → eosh) behaves exactly as the direct console did: leaving
/// the shell with no services running ends init, and the machine powers off.
const DEFAULT_SERVICES_CONFIG: &str = "\
# the kernel's default boot: a serial console, no services.
console = eosh
";

/// The baked demo config (the `svcdemo` boot token): a long-running worker the
/// registry keeps alive and a one-shot banner whose output lands in its service log.
const DEMO_SERVICES_CONFIG: &str = "\
# the kernel's baked service demo (the `svcdemo` boot token).
worker = cruncher --seed 7 --rounds 900000000000 restart restart.always
banner = echo --text hello-from-a-service restart restart.never
console = eosh
";

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
    // The bare `pci` token grants the `eo9:pci` root provider for this boot (and only this
    // boot): linkers built for the headless runner and for shell children include it, so a
    // program that imports `eo9:pci/pci` can instantiate. Without the token such a program
    // is refused at instantiation with the capability story (PCI implies DMA, so it is
    // never linked by default; see `pci_provider` and `shellexec::missing_capability`).
    super::pci_provider::set_granted(tokenize(bootargs).iter().any(|token| token == "pci"));
    // The bare `gfx` token grants the `gfx.simplefb` root provider (the board's
    // firmware framebuffer) for this boot — the same grammar and never-by-default rule
    // as `pci`; raw physical scanout memory is an operator grant, not a default. On
    // kernels without the board framebuffer the token names what is missing instead of
    // being silently swallowed.
    #[cfg(feature = "board-opi5plus")]
    super::gfx_provider::set_granted(tokenize(bootargs).iter().any(|token| token == "gfx"));
    #[cfg(not(feature = "board-opi5plus"))]
    if tokenize(bootargs).iter().any(|token| token == "gfx") {
        crate::kprintln!(
            "gfx: this kernel has no display root provider (gfx.simplefb is the Orange Pi \
             5 Plus board profile's); the `gfx` token is ignored — compose `gfx.mem $ …` instead"
        );
    }
    // The `platform` token is the same idea for memory-mapped (non-PCI) devices —
    // never linked by default; `platform=<name>,…` narrows the grant to exactly those
    // regions of the machine's table (see `platform_provider`).
    super::platform_provider::set_granted_from_tokens(
        tokenize(bootargs).iter().map(String::as_str),
    );
    // The bare `storedisk` token claims a virtio-blk function for the kernel's own
    // persistent store: a disk-backed cache of on-target compile results (and nothing
    // else); see `diskcache`. Independent of the guest-facing `pci` grant above.
    #[cfg(feature = "wasm-storedisk")]
    if tokenize(bootargs).iter().any(|token| token == "storedisk") {
        super::diskcache::init();
    }
    let (program, args) = parse_command_line(bootargs);

    // The bare `demo` token keeps the original boot sequence reachable:
    // `cargo xtask qemu aarch64 demo`. The scheduling/preemption demonstration runs first
    // (it needs the store image), then main.rs continues with the original sequence
    // (seed, hello, the async demos, on-target codegen).
    if program.is_none() && tokenize(bootargs).iter().any(|token| token == "demo") {
        super::shellexec::preemption_demo(entries);
        return false;
    }

    match program.as_deref() {
        // The default boot runs init, the service supervisor (executor v2): it applies
        // the baked config — services, then the serial console — and the machine powers
        // off when init exits. The default config is just `console = eosh`, so the
        // out-of-the-box boot behaves exactly as the direct console did; the `svcdemo`
        // token swaps in the baked demo config (a worker under restart.always and a
        // one-shot banner) to demonstrate the service registry.
        None => {
            let config = if tokenize(bootargs).iter().any(|token| token == "svcdemo") {
                DEMO_SERVICES_CONFIG
            } else {
                DEFAULT_SERVICES_CONFIG
            };
            super::shell::boot_to_init(entries, config);
        }
        // `program=eosh` keeps the direct console (no supervisor, no svc grant) — the
        // pre-init boot pipeline, still useful for byte-for-byte comparisons.
        Some("eosh") => {
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
    // The eo9:pci root provider is opt-in per boot (the `pci` command-line token), never a
    // default grant — see `pci_provider`; eo9:platform is the same posture under its
    // own `platform` token.
    if super::pci_provider::granted() {
        super::pci_provider::add_pci(&mut linker)?;
    }
    if super::platform_provider::granted() {
        super::platform_provider::add_platform(&mut linker)?;
    }
    // The gfx.simplefb root provider follows the same opt-in rule (the `gfx` token).
    #[cfg(feature = "board-opi5plus")]
    if super::gfx_provider::granted() {
        super::gfx_provider::add_gfx(&mut linker)?;
    }

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

    // Executor contract (plan/03 D23): apply compose-time configuration, if the
    // artifact carries the `eo9:rt/configured` entrypoint, before the first entry. A
    // configuration the provider rejects is `bind`'s typed error -- the run is refused,
    // never trapped.
    if let Some(bind) = super::bind_entrypoint(&instance, &mut store) {
        let mut bind_results = vec![Val::Bool(false); super::bind_result_slots(&bind, &store)];
        super::block_on(
            "bind()",
            bind.call_async(&mut store, &[], &mut bind_results),
        )??;
        if let Some(refused) = super::configuration_refused(&bind_results) {
            return Err(wasmtime::Error::msg(format!(
                "compose-time configuration refused: {refused}"
            )));
        }
    }

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
            None => {
                // An unsupplied `option<…>` parameter binds to `none`, mirroring the
                // usermode runtime and the shell's argument completion.
                if matches!(ty, Type::Option(_)) {
                    params.push(Val::Option(None));
                    continue;
                }
                return Err(format!("missing argument `{name}`"));
            }
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
        Type::Option(option) => {
            // A literal `none` is the absent option; anything else is the inner value.
            if raw == "none" {
                Val::Option(None)
            } else {
                Val::Option(Some(alloc::boxed::Box::new(parse_scalar(
                    &option.ty(),
                    raw,
                )?)))
            }
        }
        other => {
            return Err(format!(
                "the kernel runner only parses scalar and option argument types, not {other:?}"
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
