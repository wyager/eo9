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
    let restricted =
        restrict(&component, &allow).map_err(|err| wasmtime::Error::msg(format!("only failed: {err:?}")))?;
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
    let mut linker: Linker<WebState> = Linker::new(&engine);
    providers::add_providers(&mut linker)?;
    crate::fs::add_fs_io(&mut linker)?;
    let mut store = Store::new(&engine, WebState::new());
    let instance = block_on(
        "instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;
    let index = instance
        .get_export_index(&mut store, None, "main")
        .ok_or_else(|| wasmtime::Error::msg("hello does not export `main`"))?;
    let main = instance
        .get_func(&mut store, index)
        .ok_or_else(|| wasmtime::Error::msg("`main` is not a function"))?;
    let outcome = block_on(
        "main",
        store.run_concurrent(async move |accessor| -> wasmtime::Result<Val> {
            let mut result = [Val::Bool(false)];
            main.call_concurrent(
                accessor,
                &[Val::String("browser".into()), Val::Bool(true)],
                &mut result,
            )
            .await?;
            Ok(result[0].clone())
        }),
    )???;
    crate::outf!(
        "  execution: hello(name = \"browser\", excited = true) -> {}",
        render_val(&outcome)
    );
    Ok(())
}
