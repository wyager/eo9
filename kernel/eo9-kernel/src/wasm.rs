//! Run the embedded, host-precompiled seed wasm component (spike step 3).
//!
//! This is the "runtime half" of on-target execution (plan/12-kernel.md): wasmtime built
//! for `aarch64-unknown-none` with `default-features = false, features = ["runtime",
//! "component-model"]`, i.e. no compiler, no std, no virtual memory, no signal handlers.
//! In that configuration wasmtime's custom platform layer needs exactly two symbols from
//! the embedder (the TLS accessors at the bottom of this file); linear memories are plain
//! heap allocations with explicit bounds checks, and traps are explicit checks in the
//! generated code rather than CPU exceptions.
//!
//! The artifact itself is produced on the host by `cargo xtask build-kernel aarch64`
//! (Cranelift targeting `aarch64-unknown-none`) from kernel/seed/hello.wat and embedded
//! via `include_bytes!`, keeping the kernel image self-contained.

use alloc::string::String;
use alloc::sync::Arc;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, CustomCodeMemory, Engine, Store};

/// The host-precompiled seed component, injected by `cargo xtask build-kernel aarch64`.
static SEED_CWASM: &[u8] = include_bytes!(env!("EO9_SEED_CWASM"));

/// Deserialize, instantiate, and call the seed component, reporting over serial.
pub fn run_seed() {
    crate::kprintln!(
        "wasm seed: {} byte precompiled component embedded in the image",
        SEED_CWASM.len()
    );
    let start_us = crate::timer::uptime_us();
    match try_run_seed() {
        Ok((greeting, sum)) => {
            let elapsed_us = crate::timer::uptime_us() - start_us;
            crate::kprintln!("wasm seed: hello() -> \"{greeting}\"");
            crate::kprintln!("wasm seed: add(17, 25) -> {sum}");
            crate::kprintln!("wasm seed: deserialize + instantiate + 2 calls took {elapsed_us} us");
        }
        Err(error) => crate::kprintln!("wasm seed: FAILED: {error:?}"),
    }
}

fn try_run_seed() -> Result<(String, u32), wasmtime::Error> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Without virtual memory wasmtime cannot flip page protections itself, so it asks the
    // embedder to "publish" code memory; on this machine that is a no-op (see below).
    config.with_custom_code_memory(Some(Arc::new(BareMetalCodeMemory)));
    let engine = Engine::new(&config)?;

    // SAFETY: the artifact was produced by `cargo xtask build-kernel aarch64` with the
    // same wasmtime version, targeting exactly this machine and engine configuration, and
    // is embedded read-only in the kernel image.
    let component = unsafe { Component::deserialize(&engine, SEED_CWASM)? };

    let linker: Linker<()> = Linker::new(&engine);
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;

    let hello = instance.get_typed_func::<(), (String,)>(&mut store, "hello")?;
    let (greeting,) = hello.call(&mut store, ())?;

    let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut store, "add")?;
    let (sum,) = add.call(&mut store, (17, 25))?;

    Ok((greeting, sum))
}

/// Executable-memory "publisher" for a machine with the MMU off.
///
/// Precompiled code lands in an ordinary heap allocation; with the MMU disabled there are
/// no page permissions to change and nothing is non-executable, so publishing only needs a
/// barrier so the new instructions are visible before they are jumped to. A real port that
/// enables the MMU and caches must add proper cache maintenance (DC CVAU / IC IVAU over
/// the range) here.
struct BareMetalCodeMemory;

impl CustomCodeMemory for BareMetalCodeMemory {
    fn required_alignment(&self) -> usize {
        // No MMU, no page-granularity requirement; any placement is executable.
        1
    }

    fn publish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        // SAFETY: barriers have no side effects beyond ordering.
        unsafe { core::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
        Ok(())
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        Ok(())
    }
}

// --- wasmtime custom-platform hooks ------------------------------------------------------
//
// With `std`, virtual memory, native signals, and custom sync primitives all disabled,
// wasmtime's custom platform layer (`runtime/vm/sys/custom/capi.rs`) needs exactly two
// symbols from the embedder: the TLS accessors it uses to stash its per-"thread" activation
// pointer. The kernel runs a single core with interrupts masked, so one static cell is
// precisely thread-local.

static WASMTIME_TLS: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    WASMTIME_TLS.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(pointer: *mut u8) {
    WASMTIME_TLS.store(pointer, Ordering::Relaxed);
}
