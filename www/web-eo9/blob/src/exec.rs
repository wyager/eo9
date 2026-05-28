//! The component algebra in the browser.
//!
//! `eo9-component` — the real algebra crate (`load`/`describe`/`$`/`&`/`only`/`rename`/
//! `configure`), the exact code native Eo9 and the bare-metal kernel use — compiled to
//! wasm32 and run in the page. The [`algebra_demo`] export exercises it end to end: it
//! `load`s a raw component, `describe`s it, `restrict`s it with `only`, and then executes
//! the same component via Pulley against the browser root providers — so the page proves the
//! algebra (not a JS re-implementation) runs in the browser, on the same component it then
//! runs.
//!
//! This is the foundation of the full in-browser `eo9:exec` surface eosh imports: booting
//! eosh additionally needs the guest-facing `Linker` registration of component-algebra +
//! compile + task and a `/bin` store view, recorded as the remaining work in plan/18 D15.

use std::vec;

use eo9_component::{Component, ComponentKind, InterfaceRef, restrict};
use wasmtime::Store;
use wasmtime::component::{Component as WtComponent, Linker, Val};

use crate::providers::{self, WebState};
use crate::store::render_val;
use crate::{block_on, engine};

// Embedded by `cargo xtask build-web-vm`: the hello example as raw component bytes (the
// algebra works on these) and pre-AOT'd to pulley32 (Pulley executes these).
static HELLO_COMPONENT: &[u8] = include_bytes!("../artifacts/example-hello.wasm");
static HELLO_PULLEY: &[u8] = include_bytes!("../artifacts/example-hello.cwasm");

fn kind_name(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Binary => "binary",
        ComponentKind::Provider => "provider",
    }
}

/// Run the algebra over the embedded hello component, then execute it.
pub fn algebra_demo() -> wasmtime::Result<()> {
    // 1. The algebra, in the browser: load + describe a raw component.
    let component = Component::load(HELLO_COMPONENT.to_vec())
        .map_err(|err| wasmtime::Error::msg(format!("load failed: {err:?}")))?;
    let info = component.describe();
    crate::out_line(
        "component algebra (eo9-component — the real crate, compiled to wasm32 — running in your browser):",
    );
    crate::outf!("  describe: kind = {}", kind_name(info.kind));
    for import in &info.imports {
        crate::outf!(
            "    import {}@{}{}",
            import.interface,
            import.version,
            if import.required { "" } else { " (optional)" }
        );
    }
    for export in &info.exports {
        crate::outf!("    export {}", export.name);
    }

    // 2. `only` — restrict to exactly the capabilities hello needs; the restriction runs here
    //    and re-encodes a valid, sealed component.
    let allow = vec![
        InterfaceRef::any("eo9:text/text"),
        InterfaceRef::any("eo9:time/time"),
    ];
    let restricted = restrict(&component, &allow)
        .map_err(|err| wasmtime::Error::msg(format!("only failed: {err:?}")))?;
    crate::outf!(
        "  only eo9:text/text, eo9:time/time -> a sealed component of {} bytes (the algebra ran here)",
        restricted.save().len()
    );

    // 3. Execution: run the SAME component (its pre-AOT'd pulley form) against the browser
    //    root providers, so the algebra-described component is also shown to execute here.
    run_hello_pulley()?;
    crate::out_line("the component the algebra just described is the component that just ran.");
    Ok(())
}

fn run_hello_pulley() -> wasmtime::Result<()> {
    let engine = engine(false)?;
    // SAFETY: produced by `cargo xtask build-web-vm` with the matching configuration.
    let component = unsafe { WtComponent::deserialize(&engine, HELLO_PULLEY)? };
    run_component(
        &engine,
        &component,
        &[Val::String("browser".into()), Val::Bool(true)],
        "hello(name = \"browser\", excited = true)",
    )
}

/// Instantiate an already-built component against the browser root providers and run `main`
/// with the given arguments, printing the rendered outcome.
fn run_component(
    engine: &wasmtime::Engine,
    component: &WtComponent,
    args: &[Val],
    label: &str,
) -> wasmtime::Result<()> {
    let mut linker: Linker<WebState> = Linker::new(engine);
    providers::add_providers(&mut linker)?;
    crate::fs::add_fs_io(&mut linker)?;
    let mut store = Store::new(engine, WebState::new());
    let instance = block_on(
        "instantiation",
        linker.instantiate_async(&mut store, component),
    )??;
    let index = instance
        .get_export_index(&mut store, None, "main")
        .ok_or_else(|| wasmtime::Error::msg("the program does not export `main`"))?;
    let main = instance
        .get_func(&mut store, index)
        .ok_or_else(|| wasmtime::Error::msg("`main` is not a function"))?;
    let args = args.to_vec();
    let outcome = block_on(
        "main",
        store.run_concurrent(async move |accessor| -> wasmtime::Result<Val> {
            let mut result = [Val::Bool(false)];
            main.call_concurrent(accessor, &args, &mut result).await?;
            Ok(result[0].clone())
        }),
    )???;
    crate::outf!("  execution: {label} -> {}", render_val(&outcome));
    Ok(())
}

/// In-blob codegen (the fully self-hosted browser VM): compile components *inside the blob*
/// with the same vendored Cranelift + wasmtime compile layers the bare-metal kernel uses for
/// on-target codegen — emitting Pulley bytecode, the blob's execution target — then run what
/// was just compiled. No server, no pre-AOT'd artifact: compose -> compile -> run is entirely
/// client-side, the same story as native Eo9 and the bare-metal kernel (just interpreted).
#[cfg(feature = "inblob-codegen")]
pub fn compile_demo() -> wasmtime::Result<()> {
    use crate::host;

    // 1. A plain program: compile the embedded raw hello component in-blob and run it.
    crate::outf!(
        "in-blob codegen: compiling the raw hello component ({} bytes) with Cranelift -> Pulley, \
         inside this blob…",
        HELLO_COMPONENT.len()
    );
    let engine = engine(false)?;
    let started = host::monotonic_ns();
    let compiled = WtComponent::new(&engine, HELLO_COMPONENT)?;
    let compile_ms = host::monotonic_ns().saturating_sub(started) as f64 / 1e6;
    crate::outf!("in-blob codegen: hello compiled in {compile_ms:.1} ms (client-side, no server)");
    run_component(
        &engine,
        &compiled,
        &[Val::String("compiled-here".into()), Val::Bool(true)],
        "hello(name = \"compiled-here\", excited = true), compiled in-blob",
    )?;

    // 2. A fused composition: `entropy.seeded $ rng` composed by the algebra, compiled
    //    in-blob, and run — the operation that previously needed the server's compiler (or
    //    the bare-metal kernel's on-target Cranelift).
    let seeded_raw = crate::execsurface::bin_raw("entropy.seeded")
        .ok_or_else(|| wasmtime::Error::msg("entropy.seeded is not in /bin"))?;
    let rng_raw = crate::execsurface::bin_raw("rng")
        .ok_or_else(|| wasmtime::Error::msg("rng is not in /bin"))?;
    let seeded = Component::load(seeded_raw.to_vec())
        .map_err(|err| wasmtime::Error::msg(format!("load entropy.seeded failed: {err:?}")))?;
    let rng = Component::load(rng_raw.to_vec())
        .map_err(|err| wasmtime::Error::msg(format!("load rng failed: {err:?}")))?;
    let fused = eo9_component::compose(&seeded, &rng)
        .map_err(|err| wasmtime::Error::msg(format!("compose failed: {err:?}")))?;
    let fused_bytes = fused.executable_bytes();
    crate::outf!(
        "in-blob codegen: composed `entropy.seeded $ rng` with the algebra ({} fused bytes), \
         compiling…",
        fused_bytes.len()
    );
    let started = host::monotonic_ns();
    let compiled = WtComponent::new(&engine, &fused_bytes)?;
    let compile_ms = host::monotonic_ns().saturating_sub(started) as f64 / 1e6;
    crate::outf!("in-blob codegen: the fused composition compiled in {compile_ms:.1} ms");
    run_component(
        &engine,
        &compiled,
        &[Val::U64(3)],
        "entropy.seeded $ rng --count 3, fused and compiled in-blob",
    )?;
    crate::out_line(
        "compose -> compile -> run, all inside the blob: the browser VM is self-hosted.",
    );
    Ok(())
}
