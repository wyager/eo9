//! Running precompiled wasm components on the bare-metal kernel.
//!
//! This is the "runtime half" of on-target execution (plan/12-kernel.md): wasmtime built
//! for `aarch64-unknown-none` with `default-features = false, features = ["runtime",
//! "component-model"]`, i.e. no compiler, no std, no virtual memory, no signal handlers.
//! In that configuration wasmtime's custom platform layer needs exactly two symbols from
//! the embedder (the TLS accessors at the bottom of this file) plus a code-memory
//! publisher; linear memories are plain heap allocations with explicit bounds checks, and
//! traps are explicit checks in the generated code rather than CPU exceptions.
//!
//! The artifacts themselves are produced on the host by `cargo xtask build-kernel aarch64`
//! (Cranelift targeting `aarch64-unknown-none`) and embedded via `include_bytes!`, keeping
//! the kernel image self-contained:
//!
//! * [`seed`] — a tiny hand-written component (kernel/seed/hello.wat), the canary that the
//!   platform/runtime layer itself works (`wasm-seed` feature).
//! * [`hello`] — the real `eo9-example-hello` program from the guest workspace, linked
//!   against the kernel's own root [`providers`] (`wasm-hello` feature).

#[cfg(feature = "wasm-hello")]
pub mod hello;
#[cfg(feature = "wasm-hello")]
pub mod providers;
#[cfg(feature = "wasm-seed")]
pub mod seed;

use alloc::sync::Arc;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use wasmtime::{Config, CustomCodeMemory, Engine};

/// Build the kernel's wasmtime engine.
///
/// The compile-relevant parts of this configuration (tunables, wasm features) must agree
/// with the host-side precompile configuration in xtask's `precompile_for_kernel`; the
/// rest of the defaults are computed identically on both sides because wasmtime derives
/// them from the `aarch64-unknown-none` target.
pub fn new_engine() -> Result<Engine, wasmtime::Error> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Without virtual memory wasmtime cannot flip page protections itself, so it asks the
    // embedder to "publish" code memory; on this machine that is a no-op (see below).
    config.with_custom_code_memory(Some(Arc::new(BareMetalCodeMemory)));
    Engine::new(&config)
}

/// Executable-memory "publisher" for this kernel's flat identity map.
///
/// Precompiled code lands in an ordinary heap allocation, and the identity map
/// (src/mmu.rs) keeps all of RAM readable, writable, and executable, so there are no page
/// permissions to flip; publishing only needs barriers so the new instructions are visible
/// before they are jumped to. QEMU's TCG keeps the instruction stream coherent with memory
/// writes, but real hardware (and any future W^X mapping) needs D-cache clean + I-cache
/// invalidate over the range here.
struct BareMetalCodeMemory;

impl CustomCodeMemory for BareMetalCodeMemory {
    fn required_alignment(&self) -> usize {
        // The whole map is executable; no page-granularity requirement applies.
        1
    }

    fn publish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        // SAFETY: barriers have no side effects beyond ordering. (Cache maintenance is not
        // needed under QEMU TCG; see the type-level docs.)
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
