//! Run the embedded async artifacts through the component-model-async machinery
//! (kernel milestone 3): real awaited operations against the kernel's root providers.
//!
//! Two artifacts, both host-precompiled by `cargo xtask build-kernel aarch64`:
//!
//! * **sleepy** (kernel/seed/sleepy.wat) — a hand-written async canary: an async-lifted
//!   `run` export that awaits `eo9:time/time.sleep` for 50 ms against the kernel's
//!   generic timer and returns the measured elapsed monotonic nanoseconds. Suspension and
//!   resumption of a guest task on bare metal, end to end.
//!
//! * **entropy.seeded** (the unmodified `eo9-stub-entropy-seeded` component from the
//!   guest workspace) — a real, SDK-built eo9 provider whose `configure` export uses the
//!   async canonical ABI exactly as on usermode: the kernel binds a seed through
//!   `eo9:entropy/seeded-config.configure` and then draws two values from the configured
//!   `eo9:entropy/entropy.get-u64`, which are deterministic for the seed.

use alloc::format;
use alloc::string::String;

use wasmtime::Store;
use wasmtime::component::{Component, Linker, Val};

use super::providers::{self, KernelState};

/// The host-precompiled async canary, injected by `cargo xtask build-kernel aarch64`.
static SLEEPY_CWASM: &[u8] = include_bytes!(env!("EO9_SLEEPY_CWASM"));
/// The host-precompiled, unmodified `entropy.seeded` stub from the guest workspace.
static ENTROPY_SEEDED_CWASM: &[u8] = include_bytes!(env!("EO9_ENTROPY_SEEDED_CWASM"));

/// The sleep the canary requests; the measured elapsed time must be at least this.
const SLEEPY_REQUESTED_NS: u64 = 50_000_000;
/// The seed bound through `configure` in the entropy.seeded demonstration.
const ENTROPY_SEED: u64 = 0xE09;

/// Run both async demonstrations, reporting over serial.
pub fn run() {
    crate::kprintln!(
        "async demo: {} byte sleepy canary and {} byte entropy.seeded stub embedded",
        SLEEPY_CWASM.len(),
        ENTROPY_SEEDED_CWASM.len()
    );

    crate::kprintln!(
        "async demo: sleepy.run() awaiting a {} ms sleep on the kernel timer",
        SLEEPY_REQUESTED_NS / 1_000_000
    );
    let start_us = crate::timer::uptime_us();
    match run_sleepy() {
        Ok(elapsed_ns) => {
            let total_us = crate::timer::uptime_us() - start_us;
            let verdict = if elapsed_ns >= SLEEPY_REQUESTED_NS {
                "ok (>= requested)"
            } else {
                "TOO SHORT"
            };
            crate::kprintln!(
                "async demo: sleepy.run() -> {elapsed_ns} ns elapsed across the await, {verdict}"
            );
            crate::kprintln!("async demo: sleepy instantiate + run took {total_us} us");
        }
        Err(error) => crate::kprintln!("async demo: sleepy FAILED: {error:?}"),
    }

    crate::kprintln!(
        "async demo: entropy.seeded configure(seed = {ENTROPY_SEED:#x}) then get-u64 x2 \
         (async-lifted configure, unmodified guest component)"
    );
    let start_us = crate::timer::uptime_us();
    match run_entropy_seeded() {
        Ok((first, second)) => {
            let total_us = crate::timer::uptime_us() - start_us;
            crate::kprintln!("async demo: entropy.seeded get-u64 -> {first:#018x}, {second:#018x}");
            crate::kprintln!("async demo: entropy.seeded instantiate + 3 calls took {total_us} us");
        }
        Err(error) => crate::kprintln!("async demo: entropy.seeded FAILED: {error:?}"),
    }
}

/// Instantiate the sleepy canary against the kernel providers, await its `run` export,
/// and return the elapsed nanoseconds it measured around its sleep.
fn run_sleepy() -> Result<u64, wasmtime::Error> {
    let engine = super::new_engine()?;

    // SAFETY: the artifact was produced by `cargo xtask build-kernel aarch64` with the
    // same wasmtime version, targeting exactly this machine and engine configuration, and
    // is embedded read-only in the kernel image.
    let component = unsafe { Component::deserialize(&engine, SLEEPY_CWASM)? };

    let mut linker: Linker<KernelState> = Linker::new(&engine);
    providers::add_providers(&mut linker)?;

    let mut store = Store::new(&engine, KernelState::new());
    let instance = super::block_on(
        "sleepy instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let run = instance
        .get_func(&mut store, "run")
        .ok_or_else(|| wasmtime::Error::msg("sleepy component does not export `run`"))?;

    let mut results = [Val::Bool(false)];
    super::block_on(
        "sleepy run()",
        run.call_async(&mut store, &[], &mut results),
    )??;

    match results[0] {
        Val::U64(elapsed_ns) => Ok(elapsed_ns),
        ref other => Err(wasmtime::Error::msg(format!(
            "sleepy run() returned an unexpected value: {other:?}"
        ))),
    }
}

/// Instantiate the unmodified `entropy.seeded` component, bind a seed through its
/// async-lifted `configure` export, and draw two values from the configured capability.
fn run_entropy_seeded() -> Result<(u64, u64), wasmtime::Error> {
    let engine = super::new_engine()?;

    // SAFETY: as above — produced by xtask for exactly this engine configuration.
    let component = unsafe { Component::deserialize(&engine, ENTROPY_SEEDED_CWASM)? };

    // The seeded stub world imports nothing; an empty linker is the correct environment.
    let linker: Linker<KernelState> = Linker::new(&engine);
    let mut store = Store::new(&engine, KernelState::new());
    let instance = super::block_on(
        "entropy.seeded instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    // configure(seed) -> result<entropy-impl, string> on the exported seeded-config
    // interface: the compose-time configuration entry of the stub (async-lifted).
    let configure = exported_func(
        &instance,
        &mut store,
        "eo9:entropy/seeded-config@0.1.0",
        "configure",
    )?;
    let mut results = [Val::Bool(false)];
    super::block_on(
        "entropy.seeded configure()",
        configure.call_async(&mut store, &[Val::U64(ENTROPY_SEED)], &mut results),
    )??;
    let handle = match &results[0] {
        Val::Result(Ok(Some(handle))) => (**handle).clone(),
        Val::Result(Err(reason)) => {
            return Err(wasmtime::Error::msg(format!(
                "configure rejected the seed: {}",
                render_error_payload(reason.as_deref())
            )));
        }
        other => {
            return Err(wasmtime::Error::msg(format!(
                "configure returned an unexpected value: {other:?}"
            )));
        }
    };

    // get-u64(borrow<entropy-impl>) on the exported entropy interface, twice.
    let get_u64 = exported_func(
        &instance,
        &mut store,
        "eo9:entropy/entropy@0.1.0",
        "get-u64",
    )?;
    let mut first = [Val::Bool(false)];
    super::block_on(
        "entropy.seeded get-u64 (1)",
        get_u64.call_async(&mut store, core::slice::from_ref(&handle), &mut first),
    )??;
    let mut second = [Val::Bool(false)];
    super::block_on(
        "entropy.seeded get-u64 (2)",
        get_u64.call_async(&mut store, core::slice::from_ref(&handle), &mut second),
    )??;

    match (&first[0], &second[0]) {
        (Val::U64(a), Val::U64(b)) => Ok((*a, *b)),
        (a, b) => Err(wasmtime::Error::msg(format!(
            "get-u64 returned unexpected values: {a:?}, {b:?}"
        ))),
    }
}

/// Look up `func` inside the exported instance `interface` (e.g. a `configure` entry).
fn exported_func(
    instance: &wasmtime::component::Instance,
    store: &mut Store<KernelState>,
    interface: &str,
    func: &str,
) -> Result<wasmtime::component::Func, wasmtime::Error> {
    let interface_index = instance
        .get_export_index(&mut *store, None, interface)
        .ok_or_else(|| wasmtime::Error::msg(format!("component does not export `{interface}`")))?;
    let func_index = instance
        .get_export_index(&mut *store, Some(&interface_index), func)
        .ok_or_else(|| wasmtime::Error::msg(format!("`{interface}` does not export `{func}`")))?;
    instance
        .get_func(&mut *store, func_index)
        .ok_or_else(|| wasmtime::Error::msg(format!("`{interface}.{func}` is not a function")))
}

/// Render the `string` error payload of a `result<_, string>` for the serial log.
fn render_error_payload(payload: Option<&Val>) -> String {
    match payload {
        Some(Val::String(message)) => message.clone(),
        Some(other) => format!("{other:?}"),
        None => String::new(),
    }
}
