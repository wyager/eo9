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

#[cfg(feature = "wasm-async")]
pub mod async_demo;
#[cfg(feature = "wasm-hello")]
pub mod hello;
#[cfg(any(feature = "wasm-hello", feature = "wasm-async", feature = "wasm-store"))]
pub mod providers;
#[cfg(feature = "wasm-store")]
pub mod runner;
#[cfg(feature = "wasm-seed")]
pub mod seed;
#[cfg(feature = "wasm-store")]
pub mod store;

use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::pin;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, Poll, Waker};

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
    // The component-model async ABI plus the two sub-features the eo9 guest SDK relies on
    // (stackful async lifts and the extra async built-ins behind waitable-set waits).
    // These are wasm features and therefore compile-relevant: the host-side precompile
    // configuration in xtask sets exactly the same flags so the embedded artifacts load.
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
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

// The component-model-async ("concurrent") machinery keeps a second single-pointer TLS
// slot of its own, reached through the custom platform layer in the patched wasmtime
// (kernel/vendor/README.md). Same contract as `wasmtime_tls_get/set` above: one static
// cell is exactly thread-local on a single core with interrupts masked.

static WASMTIME_CONCURRENT_TLS: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());

#[unsafe(no_mangle)]
extern "C" fn wasmtime_concurrent_tls_get() -> *mut u8 {
    WASMTIME_CONCURRENT_TLS.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_concurrent_tls_set(pointer: *mut u8) {
    WASMTIME_CONCURRENT_TLS.store(pointer, Ordering::Relaxed);
}

// --- The kernel's executor ----------------------------------------------------------------

/// How long [`block_on`] lets a single wasm operation run before declaring it wedged.
/// Generous because QEMU TCG is slow; a healthy operation finishes in milliseconds.
const BLOCK_ON_WATCHDOG_NS: u64 = 30_000_000_000;

/// Drive a wasmtime future (`instantiate_async`, `call_async`, …) to completion on the
/// kernel's single thread.
///
/// This is a polling executor: every pending host operation on this machine is
/// time-driven (the only async root-provider operation today is `time.sleep`, whose
/// future re-arms its own waker on each poll), so the loop polls until the future
/// resolves, with a watchdog so a wedged guest cannot hang the boot. Once timer
/// interrupts (GIC) are wired up, the busy poll becomes a wait-for-interrupt.
pub fn block_on<F: Future>(what: &str, future: F) -> Result<F::Output, wasmtime::Error> {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Doorbell));
    let mut cx = Context::from_waker(&waker);
    let deadline = crate::timer::uptime_ns().saturating_add(BLOCK_ON_WATCHDOG_NS);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return Ok(value),
            Poll::Pending => {
                if crate::timer::uptime_ns() > deadline {
                    return Err(wasmtime::Error::msg(alloc::format!(
                        "{what} did not complete within the kernel executor's watchdog"
                    )));
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// Waker for [`block_on`]. The executor polls again on every loop iteration regardless,
/// but wasmtime's internal machinery only re-polls sub-futures whose waker was rung, so
/// this must be a real, cloneable waker for those wake-ups to be recorded.
struct Doorbell;

impl Wake for Doorbell {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}
