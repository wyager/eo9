//! Run the real `eo9-example-hello` program on the bare-metal kernel (kernel milestone 2).
//!
//! The artifact is the unmodified component from the guest workspace
//! (guest/target/components/eo9-example-hello.wasm), precompiled on the host for
//! `aarch64-unknown-none` by `cargo xtask build-kernel aarch64` and embedded here. Its
//! imports — `eo9:text/text` and `eo9:time/time` (plus their `types` interfaces) — are
//! satisfied by the kernel's own root providers (see [`super::providers`]), so the program
//! greets over the PL011 serial console with a timestamp read from the machine's clock.
//!
//! `main`'s typed arguments (`name: string, excited: bool`) are fixed here for now;
//! feeding them from the QEMU `-append` command line is part of the "headless program
//! selection via kernel cmdline" milestone.

use alloc::format;
use alloc::string::{String, ToString};

use wasmtime::Store;
use wasmtime::component::{Component, Linker, Val};

use super::providers::{self, KernelState};

/// The host-precompiled hello program, injected by `cargo xtask build-kernel aarch64`.
static HELLO_CWASM: &[u8] = include_bytes!(env!("EO9_HELLO_CWASM"));

/// Arguments passed to the program's `main`.
const NAME_ARG: &str = "bare metal";
const EXCITED_ARG: bool = true;

/// Instantiate the hello program against the kernel providers, call `main`, and report.
pub fn run() {
    crate::kprintln!(
        "hello program: {} byte precompiled eo9-example-hello embedded in the image",
        HELLO_CWASM.len()
    );
    crate::kprintln!(
        "hello program: calling main(name: \"{NAME_ARG}\", excited: {EXCITED_ARG}) \
         with kernel text/time/entropy providers"
    );
    let start_us = crate::timer::uptime_us();
    match try_run() {
        Ok(outcome) => {
            let elapsed_us = crate::timer::uptime_us() - start_us;
            crate::kprintln!("hello program: outcome = {outcome}");
            crate::kprintln!("hello program: instantiate + main took {elapsed_us} us");
        }
        Err(error) => crate::kprintln!("hello program: FAILED: {error:?}"),
    }
}

fn try_run() -> Result<String, wasmtime::Error> {
    let engine = super::new_engine()?;

    // SAFETY: the artifact was produced by `cargo xtask build-kernel aarch64` with the
    // same wasmtime version, targeting exactly this machine and engine configuration, and
    // is embedded read-only in the kernel image.
    let component = unsafe { Component::deserialize(&engine, HELLO_CWASM)? };

    let mut linker: Linker<KernelState> = Linker::new(&engine);
    providers::add_providers(&mut linker)?;

    let mut store = Store::new(&engine, KernelState::new());
    let instance = super::block_on(
        "hello instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let main = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| wasmtime::Error::msg("component does not export `main`"))?;

    let params = [Val::String(NAME_ARG.to_string()), Val::Bool(EXCITED_ARG)];
    let mut results = [Val::Bool(false)];
    super::block_on(
        "hello main()",
        main.call_async(&mut store, &params, &mut results),
    )??;

    Ok(render_outcome(&results[0]))
}

/// Render `main`'s `result<program-success, program-failure>` value for the serial log,
/// mirroring the success/failure vocabulary the usermode runtime reports.
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
        Some(Val::Variant(case, Some(inner))) => format!("{case}({inner:?})"),
        Some(other) => format!("{other:?}"),
    }
}
