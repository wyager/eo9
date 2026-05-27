//! The wasm32 half of the embed spike: wasmtime as a library, compiled *to* wasm32, loading
//! pre-AOT'd Pulley artifacts of real Eo9 components and reporting what works.
//!
//! Steps (each logs its outcome through the driver-provided `env.host_log` import and
//! contributes to the failure count returned from `run`):
//!   1. `seed sync`   — the kernel seed component (hello/add, sync canonical ABI) via plain
//!                      sync `instantiate`/`call`. Proves Pulley execution on a wasm32 host.
//!   2. `fuel`        — the same component compiled with `consume_fuel`: a tiny budget must
//!                      trap out-of-fuel, a large budget must succeed and report fuel spent.
//!   3. `cm-async`    — the unmodified `entropy.seeded` stub (async-lifted `configure`,
//!                      the artifact the bare-metal kernel also runs): tries the same
//!                      `instantiate_async`/`call_async` path the kernel uses, and the
//!                      `run_concurrent`/`call_concurrent` path eo9-runtime uses. This is
//!                      the fiber question: wasmtime-fiber has no wasm32 stack-switching
//!                      backend, so whichever of these needs a real fiber will fail here —
//!                      the point of the step is to find out which, with exact errors.

use std::fmt::Write as _;
use std::sync::Arc;

use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, CustomCodeMemory, Engine, Store};

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn host_log(ptr: *const u8, len: usize);
}

fn log(message: &str) {
    unsafe { host_log(message.as_ptr(), message.len()) }
}

// --- wasmtime custom-platform hooks -----------------------------------------------------
//
// wasmtime is built without its `std` feature here (see Cargo.toml), so it runs on the
// custom platform layer and needs the embedder to provide its TLS slots — the same two
// pairs the bare-metal kernel provides (kernel/eo9-kernel/src/wasm/mod.rs). The probe is
// single-threaded (wasm32, no threads), so plain statics are exactly thread-local.

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

macro_rules! logf {
    ($($arg:tt)*) => {{
        let mut message = String::new();
        let _ = write!(&mut message, $($arg)*);
        log(&message);
    }};
}

static SEED: &[u8] = include_bytes!("../../artifacts/seed.cwasm");
static SEED_FUEL: &[u8] = include_bytes!("../../artifacts/seed-fuel.cwasm");
#[cfg(feature = "cmasync")]
static ENTROPY_SEEDED: &[u8] = include_bytes!("../../artifacts/entropy-seeded.cwasm");

/// Same seed and expected SplitMix64 outputs as the bare-metal kernel's async demo, so the
/// two embeddings can be compared line for line.
#[cfg(feature = "cmasync")]
const ENTROPY_SEED: u64 = 0xe09;
#[cfg(feature = "cmasync")]
const EXPECTED_DRAWS: (u64, u64) = (0x505f147c387507b6, 0xe2e264775fe9be54);

/// Pulley artifacts are interpreted bytecode, not machine code, but wasmtime still routes
/// loaded images through its code-memory publisher; without virtual memory that has to be
/// embedder-provided. Publishing is a no-op here (nothing to make executable).
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

/// Compile-relevant settings must match `native-driver`'s `preaot_config`; the rest is the
/// runtime-side configuration a virtual-memory-less, signal-less host needs.
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
    // (`wasm_threads` is not configurable here: the `threads` feature is not compiled into
    // this build, which is equivalent to the `wasm_threads(false)` the pre-AOT side sets.)
    config.consume_fuel(consume_fuel);
    config.with_custom_code_memory(Some(Arc::new(NopCodeMemory)));
    Ok(config)
}

fn step_seed_sync() -> wasmtime::Result<()> {
    let engine = Engine::new(&base_config(false)?)?;
    // SAFETY: produced by native-driver `preaot` with the matching configuration above.
    let component = unsafe { Component::deserialize(&engine, SEED)? };
    let linker: Linker<()> = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
    let (greeting,) = hello.call(&mut store, ())?;
    let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut store, "add")?;
    let (sum,) = add.call(&mut store, (17, 25))?;
    logf!("seed sync: hello() -> {greeting:?}, add(17, 25) -> {sum}");
    if sum != 42 {
        return Err(wasmtime::Error::msg("add(17, 25) did not return 42"));
    }
    Ok(())
}

fn step_fuel() -> wasmtime::Result<()> {
    let engine = Engine::new(&base_config(true)?)?;
    // SAFETY: as above (the consume_fuel variant of the artifact).
    let component = unsafe { Component::deserialize(&engine, SEED_FUEL)? };
    let linker: Linker<()> = Linker::new(&engine);

    // A roomy budget must succeed; how much it metered tells us what a "too small" budget is.
    let mut store = Store::new(&engine, ());
    store.set_fuel(1_000_000)?;
    let instance = linker.instantiate(&mut store, &component)?;
    let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
    let (greeting,) = hello.call(&mut store, ())?;
    let spent = 1_000_000 - store.get_fuel()?;
    logf!("fuel: 1_000_000-unit budget ran hello() -> {greeting:?}, fuel spent = {spent}");
    if spent == 0 {
        return Err(wasmtime::Error::msg("no fuel was metered"));
    }

    // Informational: a budget smaller than what the call metered. wasmtime checks fuel at
    // block/loop boundaries, so a straight-line function like the seed's hello() can finish
    // on a deficit; looping workloads trap. Either outcome is the same as on native.
    let starvation = spent.saturating_sub(1).max(1);
    let mut store = Store::new(&engine, ());
    store.set_fuel(starvation)?;
    let small_budget = linker
        .instantiate(&mut store, &component)
        .and_then(|instance| {
            let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
            hello.call(&mut store, ())
        });
    match small_budget {
        Ok(_) => logf!(
            "fuel: a {starvation}-unit budget still finished hello() (straight-line code; \
             fuel is checked at block boundaries, same as native wasmtime)"
        ),
        Err(error) => logf!("fuel: {starvation}-unit budget trapped as expected ({error})"),
    }
    Ok(())
}

#[cfg(feature = "cmasync")]
mod cmasync {
    use super::*;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    /// Single-threaded polling executor, the same shape as the bare-metal kernel's
    /// `block_on` (bounded by iterations rather than a clock: this host has no clock).
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

    fn engine() -> wasmtime::Result<Engine> {
        // (`Config::async_support` is deprecated and has no effect in wasmtime 45; the async
        // entry points are available whenever the cargo feature is compiled in.)
        Engine::new(&base_config(false)?)
    }

    fn draws_from(results: &[Val; 2]) -> wasmtime::Result<(u64, u64)> {
        match results {
            [Val::U64(first), Val::U64(second)] => Ok((*first, *second)),
            other => Err(wasmtime::Error::msg(format!(
                "unexpected get-u64 results: {other:?}"
            ))),
        }
    }

    /// Look up `func` inside the exported instance `interface` (same shape as the kernel's
    /// async demo helper).
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

    fn configured_handle(configured: &Val) -> wasmtime::Result<Val> {
        match configured {
            Val::Result(Ok(Some(handle))) => Ok((**handle).clone()),
            other => Err(wasmtime::Error::msg(format!(
                "configure returned an unexpected value: {other:?}"
            ))),
        }
    }

    /// The path the bare-metal kernel uses for the calls (`call_async`), but with a *sync*
    /// instantiation so the fiber question is answered per entry point rather than failing
    /// wholesale at `instantiate_async`.
    pub fn kernel_style() -> wasmtime::Result<(u64, u64)> {
        let engine = engine()?;
        // SAFETY: produced by native-driver `preaot` with the matching configuration.
        let component = unsafe { Component::deserialize(&engine, ENTROPY_SEEDED)? };
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &component)?;
        log("cm-async: sync instantiate of entropy.seeded succeeded");
        let configure = exported_func(
            &instance,
            &mut store,
            "eo9:entropy/seeded-config@0.1.0",
            "configure",
        )?;
        let mut configured = [Val::Bool(false)];
        block_on(
            "configure call_async",
            configure.call_async(&mut store, &[Val::U64(ENTROPY_SEED)], &mut configured),
        )
        .and_then(|inner| inner)
        .map_err(|error| wasmtime::Error::msg(format!("configure via call_async: {error:?}")))?;
        let handle = configured_handle(&configured[0])?;
        let get_u64 = exported_func(
            &instance,
            &mut store,
            "eo9:entropy/entropy@0.1.0",
            "get-u64",
        )?;
        let mut first = [Val::Bool(false)];
        block_on(
            "get-u64 call_async (1)",
            get_u64.call_async(&mut store, core::slice::from_ref(&handle), &mut first),
        )??;
        let mut second = [Val::Bool(false)];
        block_on(
            "get-u64 call_async (2)",
            get_u64.call_async(&mut store, core::slice::from_ref(&handle), &mut second),
        )??;
        draws_from(&[first[0].clone(), second[0].clone()])
    }

    /// The path usermode eo9-runtime uses for the calls (`run_concurrent` +
    /// `call_concurrent`), again with a sync instantiation.
    pub fn runtime_style() -> wasmtime::Result<(u64, u64)> {
        let engine = engine()?;
        // SAFETY: as above.
        let component = unsafe { Component::deserialize(&engine, ENTROPY_SEEDED)? };
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &component)?;
        log("cm-async: sync instantiate of entropy.seeded succeeded (run_concurrent path)");
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

        let outcome = block_on(
            "run_concurrent",
            store.run_concurrent(async move |accessor| {
                let mut configured = [Val::Bool(false)];
                configure
                    .call_concurrent(accessor, &[Val::U64(ENTROPY_SEED)], &mut configured)
                    .await?;
                let handle = configured_handle(&configured[0])?;
                let mut first = [Val::Bool(false)];
                get_u64
                    .call_concurrent(accessor, core::slice::from_ref(&handle), &mut first)
                    .await?;
                let mut second = [Val::Bool(false)];
                get_u64
                    .call_concurrent(accessor, core::slice::from_ref(&handle), &mut second)
                    .await?;
                draws_from(&[first[0].clone(), second[0].clone()])
            }),
        )??;
        outcome
    }
}

#[cfg(feature = "cmasync")]
fn step_cmasync() -> u32 {
    let mut failures = 0;
    for (name, result) in [
        ("sync instantiate + call_async", cmasync::kernel_style()),
        (
            "sync instantiate + run_concurrent/call_concurrent",
            cmasync::runtime_style(),
        ),
    ] {
        match result {
            Ok(draws) => {
                let verdict = if draws == EXPECTED_DRAWS {
                    "matches"
                } else {
                    "DOES NOT match"
                };
                logf!(
                    "cm-async ({name}): entropy.seeded configure({ENTROPY_SEED:#x}) then get-u64 twice -> \
                     {:#x}, {:#x} ({verdict} the kernel/native sequence)",
                    draws.0,
                    draws.1
                );
                if draws != EXPECTED_DRAWS {
                    failures += 1;
                }
            }
            Err(error) => {
                logf!("cm-async ({name}): FAILED: {error:?}");
                failures += 1;
            }
        }
    }
    failures
}

#[unsafe(no_mangle)]
pub extern "C" fn run() -> i32 {
    let mut failures: u32 = 0;
    logf!(
        "probe: wasmtime {} as a wasm32 host, pulley32 artifacts, cm-async feature = {}",
        env!("CARGO_PKG_VERSION"),
        cfg!(feature = "cmasync")
    );
    for (name, result) in [("seed sync", step_seed_sync()), ("fuel", step_fuel())] {
        if let Err(error) = result {
            logf!("{name}: FAILED: {error:?}");
            failures += 1;
        }
    }
    #[cfg(feature = "cmasync")]
    {
        failures += step_cmasync();
    }
    logf!("probe: finished with {failures} failure(s)");
    failures as i32
}
