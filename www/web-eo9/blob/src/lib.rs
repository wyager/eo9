//! The in-browser Eo9 blob: the real runtime stack — wasmtime with the Pulley interpreter
//! backend — compiled to wasm32 and driven from the page's JavaScript.
//!
//! What runs here is exactly what runs on native Eo9 and on the bare-metal kernel: the same
//! pinned wasmtime (with the kernel's vendored component-model-async relaxation and the
//! opt-in fiberless execution path, since wasm32 has no fiber backend), deserializing the
//! same kind of pre-AOT'd `pulley32` artifacts of unmodified Eo9 components. Nothing on the
//! page is a JavaScript re-implementation of Eo9 semantics.
//!
//! Exports (each returns 0 on success, non-zero on failure, and reports through the
//! `env.host_write` import — one UTF-8 line per call):
//!   * `boot()` — describe what is loaded (artifact sizes, configuration) on the terminal.
//!   * `run_hello()` — the kernel seed component over the sync canonical ABI: `hello()` and
//!     `add(17, 25)`, executed by Pulley.
//!   * `run_fuel()` — the same component with fuel metering on, reporting fuel spent.
//!   * `run_entropy(seed_lo, seed_hi, count)` — the unmodified `entropy.seeded` stub (a real
//!     component-model-async Eo9 guest): async-lifted `configure(seed)`, then `count`
//!     `get-u64` draws via the same `run_concurrent`/`call_concurrent` path usermode
//!     eo9-runtime uses — running fiberlessly on this wasm32 host.

use std::fmt::Write as _;
use std::sync::Arc;

use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, CustomCodeMemory, Engine, Store};

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn host_write(ptr: *const u8, len: usize);
}

fn out(message: &str) {
    unsafe { host_write(message.as_ptr(), message.len()) }
}

macro_rules! outf {
    ($($arg:tt)*) => {{
        let mut message = String::new();
        let _ = write!(&mut message, $($arg)*);
        out(&message);
    }};
}

// --- wasmtime custom-platform hooks (same pair the bare-metal kernel provides) -----------

use core::sync::atomic::{AtomicPtr, Ordering};

static WASMTIME_TLS: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    WASMTIME_TLS.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(pointer: *mut u8) {
    WASMTIME_TLS.store(pointer, Ordering::Relaxed);
}

static WASMTIME_CONCURRENT_TLS: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

#[unsafe(no_mangle)]
extern "C" fn wasmtime_concurrent_tls_get() -> *mut u8 {
    WASMTIME_CONCURRENT_TLS.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_concurrent_tls_set(pointer: *mut u8) {
    WASMTIME_CONCURRENT_TLS.store(pointer, Ordering::Relaxed);
}

/// Pulley artifacts are interpreted bytecode; publishing code memory is a no-op on a host
/// with no virtual memory of its own.
struct NopCodeMemory;

impl CustomCodeMemory for NopCodeMemory {
    fn required_alignment(&self) -> usize {
        1
    }
    fn publish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        Ok(())
    }
    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        Ok(())
    }
}

// Pre-AOT'd pulley32 artifacts produced by `cargo xtask build-web-vm` (which writes
// blob/artifacts/ before building this crate). The seed component is the kernel's
// hello/add seed; entropy.seeded is the unmodified guest stub from `guest/stubs`.
static SEED: &[u8] = include_bytes!("../artifacts/seed.cwasm");
static SEED_FUEL: &[u8] = include_bytes!("../artifacts/seed-fuel.cwasm");
static ENTROPY_SEEDED: &[u8] = include_bytes!("../artifacts/entropy-seeded.cwasm");

/// Compile-relevant settings must match the xtask pre-AOT side (`preaot_for_web`).
fn base_config(consume_fuel: bool) -> wasmtime::Result<Config> {
    let mut config = Config::new();
    config.target("pulley32")?;
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(true);
    config.gc_support(false);
    config.consume_fuel(consume_fuel);
    config.with_custom_code_memory(Some(Arc::new(NopCodeMemory)));
    Ok(config)
}

fn engine(consume_fuel: bool) -> wasmtime::Result<Engine> {
    Engine::new(&base_config(consume_fuel)?)
}

fn report(name: &str, result: wasmtime::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            outf!("{name}: failed: {error:?}");
            1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn boot() -> i32 {
    report(
        "boot",
        || -> wasmtime::Result<()> {
            // Building an engine exercises the whole configuration once up front.
            let _ = engine(false)?;
            out(
                "eo9 web vm: the pinned wasmtime (45.0.0, Eo9's vendored copy) compiled to wasm32, \
             Pulley interpreter, fiberless component-model-async",
            );
            outf!(
                "eo9 web vm: artifacts on board: seed {} B, seed+fuel {} B, entropy.seeded {} B \
             (pre-AOT'd to pulley32 at site build time)",
                SEED.len(),
                SEED_FUEL.len(),
                ENTROPY_SEEDED.len()
            );
            out("eo9 web vm: ready");
            Ok(())
        }(),
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn run_hello() -> i32 {
    report(
        "hello",
        || -> wasmtime::Result<()> {
            let engine = engine(false)?;
            // SAFETY: produced by `cargo xtask build-web-vm` with the matching configuration.
            let component = unsafe { Component::deserialize(&engine, SEED)? };
            let linker: Linker<()> = Linker::new(&engine);
            let mut store = Store::new(&engine, ());
            let instance = linker.instantiate(&mut store, &component)?;
            let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
            let (greeting,) = hello.call(&mut store, ())?;
            let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut store, "add")?;
            let (sum,) = add.call(&mut store, (17, 25))?;
            outf!("hello() -> {greeting:?}");
            outf!("add(17, 25) -> {sum}");
            Ok(())
        }(),
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn run_fuel() -> i32 {
    report(
        "fuel",
        || -> wasmtime::Result<()> {
            let engine = engine(true)?;
            // SAFETY: as above (the consume_fuel variant of the artifact).
            let component = unsafe { Component::deserialize(&engine, SEED_FUEL)? };
            let linker: Linker<()> = Linker::new(&engine);
            let mut store = Store::new(&engine, ());
            store.set_fuel(1_000_000)?;
            let instance = linker.instantiate(&mut store, &component)?;
            let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
            let (greeting,) = hello.call(&mut store, ())?;
            let spent = 1_000_000 - store.get_fuel()?;
            outf!("with a 1,000,000-unit fuel budget, hello() -> {greeting:?}");
            outf!("fuel metered for that call: {spent} units (same accounting as native Eo9)");
            Ok(())
        }(),
    )
}

mod entropy {
    use super::*;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    /// Single-threaded polling executor (the same shape as the bare-metal kernel's
    /// `block_on`); the fiberless guest calls complete without suspending, so this loop
    /// only spins across host-future bookkeeping.
    fn block_on<F: Future>(what: &str, future: F) -> wasmtime::Result<F::Output> {
        const MAX_POLLS: u64 = 10_000_000;
        let mut future = pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        for _ in 0..MAX_POLLS {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return Ok(value),
                Poll::Pending => core::hint::spin_loop(),
            }
        }
        Err(wasmtime::Error::msg(format!(
            "{what} did not complete within {MAX_POLLS} polls"
        )))
    }

    fn exported_func(
        instance: &wasmtime::component::Instance,
        store: &mut Store<()>,
        interface: &str,
        func: &str,
    ) -> wasmtime::Result<wasmtime::component::Func> {
        let interface_index = instance
            .get_export_index(&mut *store, None, interface)
            .ok_or_else(|| {
                wasmtime::Error::msg(format!("component does not export `{interface}`"))
            })?;
        let func_index = instance
            .get_export_index(&mut *store, Some(&interface_index), func)
            .ok_or_else(|| {
                wasmtime::Error::msg(format!("`{interface}` does not export `{func}`"))
            })?;
        instance
            .get_func(&mut *store, func_index)
            .ok_or_else(|| wasmtime::Error::msg(format!("`{interface}.{func}` is not a function")))
    }

    pub fn run(seed: u64, count: u32) -> wasmtime::Result<()> {
        let engine = engine(false)?;
        // SAFETY: produced by `cargo xtask build-web-vm` with the matching configuration.
        let component = unsafe { Component::deserialize(&engine, ENTROPY_SEEDED)? };
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &component)?;
        let configure = exported_func(
            &instance,
            &mut store,
            "eo9:entropy/seeded-config@0.1.0",
            "configure",
        )?;
        let get_u64 = exported_func(
            &instance,
            &mut store,
            "eo9:entropy/entropy@0.1.0",
            "get-u64",
        )?;

        let draws = block_on(
            "entropy.seeded",
            store.run_concurrent(async move |accessor| -> wasmtime::Result<Vec<u64>> {
                let mut configured = [Val::Bool(false)];
                configure
                    .call_concurrent(accessor, &[Val::U64(seed)], &mut configured)
                    .await?;
                let handle = match &configured[0] {
                    Val::Result(Ok(Some(handle))) => (**handle).clone(),
                    other => {
                        return Err(wasmtime::Error::msg(format!(
                            "configure returned an unexpected value: {other:?}"
                        )));
                    }
                };
                let mut draws = Vec::new();
                for _ in 0..count {
                    let mut result = [Val::Bool(false)];
                    get_u64
                        .call_concurrent(accessor, core::slice::from_ref(&handle), &mut result)
                        .await?;
                    match result[0] {
                        Val::U64(value) => draws.push(value),
                        ref other => {
                            return Err(wasmtime::Error::msg(format!(
                                "get-u64 returned an unexpected value: {other:?}"
                            )));
                        }
                    }
                }
                Ok(draws)
            }),
        )???;

        outf!(
            "entropy.seeded (the unmodified Eo9 guest stub), configure(seed = {seed:#x}) — an \
             async-lifted call running fiberlessly on this wasm32 host:"
        );
        for (index, value) in draws.iter().enumerate() {
            outf!("  get-u64 #{index} -> {value:#018x}");
        }
        out(
            "same seed, same sequence — on this page, on native Eo9, and on the bare-metal kernel.",
        );
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run_entropy(seed_lo: u32, seed_hi: u32, count: u32) -> i32 {
    let seed = (u64::from(seed_hi) << 32) | u64::from(seed_lo);
    let count = count.clamp(1, 64);
    report("entropy", entropy::run(seed, count))
}
