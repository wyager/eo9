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

use std::sync::Arc;

use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, CustomCodeMemory, Engine, Store};

mod exec;
mod execsurface;
mod fs;
mod host;
mod providers;
mod store;

fn out(message: &str) {
    host::write_out(message)
}

#[macro_export]
macro_rules! outf {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut message = String::new();
        let _ = write!(&mut message, $($arg)*);
        $crate::out_line(&message);
    }};
}

/// Macro plumbing for [`outf!`] (callable from every module).
pub fn out_line(message: &str) {
    out(message)
}

/// Single-threaded polling executor (the same shape as the bare-metal kernel's
/// `block_on`). The fiberless guest calls complete without suspending, and the
/// genuinely-blocking host imports (sleep, read-line, fetch) park the *whole blob* via
/// JSPI before returning, so this loop only ever spins across host-future bookkeeping.
pub(crate) fn block_on<F: core::future::Future>(
    what: &str,
    future: F,
) -> wasmtime::Result<F::Output> {
    use core::task::{Context, Poll, Waker};
    const MAX_POLLS: u64 = 10_000_000;
    let mut future = core::pin::pin!(future);
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

/// Run the real component algebra (`eo9-component`) in the browser on a raw component —
/// load, describe, `only`-restrict — then execute the same component via Pulley. The
/// foundation of the in-browser `eo9:exec` surface eosh imports.
#[unsafe(no_mangle)]
pub extern "C" fn algebra_demo() -> i32 {
    report("algebra", exec::algebra_demo())
}

/// In-blob codegen demo: compile a raw component — and an algebra-fused composition — inside
/// the blob with the same vendored Cranelift + wasmtime compile layers the bare-metal kernel
/// uses for on-target codegen (Pulley as the emission target), then run what was just
/// compiled. Fully client-side: no server, no pre-AOT'd artifact.
#[unsafe(no_mangle)]
pub extern "C" fn compile_demo() -> i32 {
    #[cfg(feature = "inblob-codegen")]
    {
        report("in-blob codegen", exec::compile_demo())
    }
    #[cfg(not(feature = "inblob-codegen"))]
    {
        out("in-blob codegen is not built into this blob (feature `inblob-codegen` is off)");
        1
    }
}

/// Instantiate eosh against the in-browser exec/text/fs surface (the floor: the shell links).
#[unsafe(no_mangle)]
pub extern "C" fn eosh_instantiate() -> i32 {
    report("eosh", execsurface::boot_eosh_instantiate())
}

/// Boot eosh one-shot on a command line written into a `web_alloc` buffer by the page.
///
/// # Safety
/// `ptr`/`len` must describe a live [`web_alloc`] buffer the page filled with UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eosh_command(ptr: *const u8, len: usize) -> i32 {
    let command = unsafe { page_str(ptr, len) }.to_owned();
    report("eosh", execsurface::boot_eosh(&command))
}

/// Boot the interactive `eosh>` prompt: eosh reads command lines from the page terminal
/// (JSPI read-line) until end-of-input or `exit`.
#[unsafe(no_mangle)]
pub extern "C" fn eosh_boot() -> i32 {
    report("eosh", execsurface::boot_eosh_interactive())
}

// --- milestone 2: real programs from the HTTP store, awaits parked on the browser ---------

/// Allocate `len` bytes the page's JavaScript can write into (program names / arguments)
/// before calling [`run_program`]. Paired with [`web_free`].
#[unsafe(no_mangle)]
pub extern "C" fn web_alloc(len: usize) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buffer.as_mut_ptr();
    core::mem::forget(buffer);
    ptr
}

/// Release a buffer handed out by [`web_alloc`].
///
/// # Safety
/// `ptr`/`len` must be exactly what a single prior `web_alloc(len)` returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn web_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
    }
}

/// Decode a (pointer, length) pair handed in by the page.
///
/// # Safety
/// The range must be a live `web_alloc` buffer the page filled with UTF-8.
unsafe fn page_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    core::str::from_utf8(unsafe { core::slice::from_raw_parts(ptr, len) }).unwrap_or("")
}

/// The kernel's async sleep canary (stackful lift — reported as unsupported on this host).
#[unsafe(no_mangle)]
pub extern "C" fn run_sleepy() -> i32 {
    report("sleepy", store::run_sleepy())
}

/// Park the whole VM on a real browser timer through the same JSPI import the time
/// provider's `sleep` uses, and measure the elapsed monotonic time around it.
#[unsafe(no_mangle)]
pub extern "C" fn probe_sleep(ms: u32) -> i32 {
    let ms = ms.clamp(1, 10_000);
    let started = host::monotonic_ns();
    host::sleep_ms(f64::from(ms));
    let elapsed = host::monotonic_ns().saturating_sub(started);
    outf!(
        "park the VM: asked the browser for a {ms} ms timer; the VM was suspended and resumed \
         {:.1} ms later (measured by the same monotonic clock the time provider serves)",
        elapsed as f64 / 1_000_000.0
    );
    if elapsed >= u64::from(ms) * 1_000_000 {
        0
    } else {
        outf!("probe_sleep: the VM came back early — JSPI suspension did not happen");
        1
    }
}

/// Terminal-input round trip through the same JSPI import the text provider's `read-line`
/// uses: the blob parks until the visitor presses Enter, then echoes the line back.
#[unsafe(no_mangle)]
pub extern "C" fn probe_read_line() -> i32 {
    out("read-line: waiting for one line of terminal input (the blob is parked on your keyboard)…");
    match host::read_line(4096) {
        Some(line) => {
            outf!("read-line -> {line:?} (round-tripped through the suspended blob)");
            0
        }
        None => {
            out("read-line -> end of input");
            0
        }
    }
}

/// Fetch one of the page store's programs and run `main` with the given arguments
/// (`args` = unit-separator-joined fields written into a `web_alloc` buffer by the page).
///
/// # Safety
/// Both (pointer, length) pairs must describe live [`web_alloc`] buffers the page filled
/// with UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn run_program(
    name_ptr: *const u8,
    name_len: usize,
    args_ptr: *const u8,
    args_len: usize,
) -> i32 {
    let name = unsafe { page_str(name_ptr, name_len) }.to_owned();
    let args_joined = unsafe { page_str(args_ptr, args_len) }.to_owned();
    let args: Vec<&str> = if args_joined.is_empty() {
        Vec::new()
    } else {
        args_joined.split('\u{1f}').collect()
    };
    let result = store::run_program(&name, &args);
    report(&name, result)
}
