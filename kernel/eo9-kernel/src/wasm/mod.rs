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
#[cfg(feature = "wasm-codegen")]
pub mod codegen;
#[cfg(feature = "wasm-hello")]
pub mod hello;
#[cfg(any(feature = "wasm-hello", feature = "wasm-async", feature = "wasm-store"))]
pub mod providers;
#[cfg(feature = "wasm-store")]
pub mod runner;
#[cfg(feature = "wasm-seed")]
pub mod seed;
#[cfg(feature = "wasm-store")]
pub mod shell;
#[cfg(feature = "wasm-store")]
pub mod shellexec;
#[cfg(feature = "wasm-store")]
pub mod shellfs;
#[cfg(feature = "wasm-store")]
pub mod store;
#[cfg(feature = "wasm-store")]
pub mod wave;

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
    // With the compiler (`wasm-codegen`) linked in, wasmtime would otherwise try to infer
    // the host target through `cranelift-native`, which needs `std` and is disabled here.
    // The kernel is built *for* this triple, so `Triple::host()` equals it and execution of
    // both deserialized and on-target-compiled code is accepted as native.
    config.target("aarch64-unknown-none")?;
    config.wasm_component_model(true);
    // The component-model async ABI plus the two sub-features the eo9 guest SDK relies on
    // (stackful async lifts and the extra async built-ins behind waitable-set waits).
    // These are wasm features and therefore compile-relevant: the host-side precompile
    // configuration in xtask sets exactly the same flags so the embedded artifacts load.
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    // The OS-less tunables. These match xtask's `precompile_for_kernel` so deserialized
    // artifacts load, and — now that the compiler (`wasm-codegen`) is linked, which makes
    // wasmtime run its native-host compatibility check on every engine — they are also what
    // make this engine pass that check (no native signals, no virtual-memory reservations or
    // guards, no copy-on-write memory initialization).
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(true);
    // Without virtual memory wasmtime cannot flip page protections itself, so it asks the
    // embedder to "publish" code memory; on this machine that is D-cache clean + I-cache
    // invalidate over the range (see below), no page-permission flips.
    config.with_custom_code_memory(Some(Arc::new(BareMetalCodeMemory)));
    Engine::new(&config)
}

/// Executable-memory "publisher" for this kernel's flat identity map.
///
/// Code — whether deserialized from an AOT artifact today or emitted on-target by Cranelift
/// once `wasm-codegen` lands (plan/12 Decisions 26–27) — lands in an ordinary heap
/// allocation, and the identity map (src/mmu.rs) keeps all of RAM readable, writable, and
/// executable, so there are no page permissions to flip. Making the new instructions
/// visible to the fetch path is real cache maintenance, not just barriers: QEMU's TCG keeps
/// the instruction stream coherent with stores, but on real hardware the freshly written
/// bytes sit in the D-cache while the I-cache may hold stale lines, so we clean D to the
/// point of unification then invalidate I over the published range (W^X page protections
/// remain a separate MMU item, Decision 3).
struct BareMetalCodeMemory;

impl CustomCodeMemory for BareMetalCodeMemory {
    fn required_alignment(&self) -> usize {
        // The whole map is executable; no page-granularity requirement applies (until W^X).
        1
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        // SAFETY: the [ptr, ptr+len) range is the code memory wasmtime just wrote and is
        // about to execute; cache-maintenance ops over it have no effect beyond making the
        // I-fetch path observe those writes. A zero-length publish is a no-op.
        unsafe { publish_code_range(ptr, len) };
        Ok(())
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        Ok(())
    }
}

/// Make `[ptr, ptr+len)` coherent with the instruction-fetch path on aarch64: clean the
/// D-cache to the point of unification by line, then invalidate the I-cache to PoU by line,
/// with the barriers the architecture requires between and after. Line sizes come from
/// `CTR_EL0` (`DminLine`/`IminLine`, each `log2` of the line size in 32-bit words).
///
/// # Safety
/// `ptr`/`len` must describe a readable range that the caller owns; the ops are otherwise
/// side-effect-free.
unsafe fn publish_code_range(ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    let start = ptr as usize;
    let end = start + len;

    let ctr: usize;
    // SAFETY: CTR_EL0 is readable at EL1 and has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags))
    };
    let dminline = 4usize << ((ctr >> 16) & 0xf); // D-cache line, bytes
    let iminline = 4usize << (ctr & 0xf); // I-cache line, bytes

    // Clean D-cache to PoU by line, then ensure completion before invalidating I.
    let mut addr = start & !(dminline - 1);
    while addr < end {
        // SAFETY: `dc cvau` is a clean-by-VA op over owned memory.
        unsafe { core::arch::asm!("dc cvau, {}", in(reg) addr, options(nostack, preserves_flags)) };
        addr += dminline;
    }
    // SAFETY: ordering barrier only.
    unsafe { core::arch::asm!("dsb ish", options(nostack, preserves_flags)) };

    // Invalidate I-cache to PoU by line.
    addr = start & !(iminline - 1);
    while addr < end {
        // SAFETY: `ic ivau` is an invalidate-by-VA op over owned memory.
        unsafe { core::arch::asm!("ic ivau, {}", in(reg) addr, options(nostack, preserves_flags)) };
        addr += iminline;
    }
    // SAFETY: ordering + context-synchronization so the new instructions are fetched.
    unsafe { core::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
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
