//! Running precompiled wasm components on the bare-metal kernel.
//!
//! This is the "runtime half" of on-target execution (plan/12-kernel.md): wasmtime built
//! for the bare-metal target with `default-features = false, features = ["runtime",
//! "component-model"]`, i.e. no compiler, no std, no virtual memory, no signal handlers.
//! In that configuration wasmtime's custom platform layer needs exactly two symbols from
//! the embedder (the TLS accessors at the bottom of this file) plus a code-memory
//! publisher; linear memories are plain heap allocations with explicit bounds checks, and
//! traps are explicit checks in the generated code rather than CPU exceptions.
//!
//! The artifacts themselves are produced on the host by `cargo xtask build-kernel <arch>`
//! (Cranelift targeting this same bare-metal triple) and embedded via `include_bytes!`,
//! keeping the kernel image self-contained:
//!
//! * [`seed`] — a tiny hand-written component (kernel/seed/hello.wat), the canary that the
//!   platform/runtime layer itself works (`wasm-seed` feature).
//! * [`hello`] — the real `eo9-example-hello` program from the guest workspace, linked
//!   against the kernel's own root [`providers`] (`wasm-hello` feature).

#[cfg(feature = "wasm-async")]
pub mod async_demo;
#[cfg(feature = "wasm-codegen")]
pub mod codegen;
#[cfg(feature = "wasm-storedisk")]
pub mod diskcache;
#[cfg(feature = "wasm-hello")]
pub mod hello;
#[cfg(feature = "wasm-store")]
pub mod pci_provider;
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
pub mod svc;
#[cfg(feature = "wasm-store")]
pub mod wave;

use alloc::sync::Arc;
use alloc::task::Wake;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::pin;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use wasmtime::{Config, CustomCodeMemory, Engine};

/// The triple wasmtime knows this kernel build as. Precompiled artifacts and the on-target
/// compiler must agree with it; xtask's `precompile_for_kernel` uses the same string per
/// architecture.
#[cfg(target_arch = "aarch64")]
const NATIVE_TARGET: &str = "aarch64-unknown-none";
#[cfg(target_arch = "riscv64")]
const NATIVE_TARGET: &str = "riscv64gc-unknown-none-elf";
#[cfg(target_arch = "x86_64")]
const NATIVE_TARGET: &str = "x86_64-unknown-none";

/// Build the kernel's wasmtime engine.
///
/// The compile-relevant parts of this configuration (tunables, wasm features) must agree
/// with the host-side precompile configuration in xtask's `precompile_for_kernel`; the
/// rest of the defaults are computed identically on both sides because wasmtime derives
/// them from the same bare-metal target ([`NATIVE_TARGET`]).
pub fn new_engine() -> Result<Engine, wasmtime::Error> {
    let mut config = Config::new();
    // With the compiler (`wasm-codegen`) linked in, wasmtime would otherwise try to infer
    // the host target through `cranelift-native`, which needs `std` and is disabled here.
    // The kernel is built *for* this triple, so `Triple::host()` equals it and execution of
    // both deserialized and on-target-compiled code is accepted as native.
    config.target(NATIVE_TARGET)?;
    // x86_64 only: this kernel is compiled soft-float (`x86_64-unknown-none`), which wasmtime
    // refuses to load native code under by default, because Cranelift-generated code passes
    // floats in XMM registers. The one boundary where a float crosses in a register is a
    // float "libcall" (f32/f64 ceil/floor/trunc/nearest emitted when the compilation target
    // lacks SSE4.1) — and xtask's `precompile_for_kernel` enables SSE3..SSE4.2 for exactly
    // this target, so no artifact contains such a libcall (`x86_float_abi_ok`'s documented
    // safe condition (b)). The kernel's own Rust code never touches XMM state (soft-float
    // codegen), which is also why the trap entry does not save it. Verifying those enabled
    // ISA flags at load time needs a host-feature probe, which on bare metal is a CPUID read.
    #[cfg(target_arch = "x86_64")]
    // SAFETY: the precompile side guarantees no float libcalls exist in any artifact (see
    // above), and `x86_detect_host_feature` answers from CPUID, so a flag is only accepted
    // when the CPU really has the instruction set.
    unsafe {
        config.x86_float_abi_ok(true);
        config.detect_host_feature(x86_detect_host_feature);
    }
    // With the on-target compiler linked in, the engine's own ISA flags must mirror the
    // precompile set (xtask's `precompile_for_kernel`): SSE3..SSE4.2 enabled, so code the
    // kernel compiles on-target also emits float ceil/floor/trunc/nearest inline rather
    // than as float libcalls (the `x86_float_abi_ok` safe condition above), and so the
    // engine's flags agree with every embedded artifact's recorded flags. The CPUID probe
    // installed above still verifies the CPU actually has them at load time.
    #[cfg(all(target_arch = "x86_64", feature = "wasm-codegen"))]
    // SAFETY: enabling ISA flags only changes which instructions may be emitted; the CPUID
    // probe refuses to run anything the CPU does not support.
    unsafe {
        config.cranelift_flag_enable("has_sse3");
        config.cranelift_flag_enable("has_ssse3");
        config.cranelift_flag_enable("has_sse41");
        config.cranelift_flag_enable("has_sse42");
    }
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
    // Fuel metering. Compile-relevant (generated code carries the fuel decrements), so
    // xtask's `precompile_for_kernel` sets exactly the same flag. Every store on this
    // engine must be given fuel before guest code runs (`Store::set_fuel`); spawned shell
    // children additionally slice their pool with `fuel_async_yield_interval` so a
    // compute-bound child is preempted at quantum granularity instead of monopolizing the
    // drive loop (plan/12: child fuel / preemption).
    config.consume_fuel(true);
    // Without virtual memory wasmtime cannot flip page protections itself, so it asks the
    // embedder to "publish" code memory; on this machine that is D-cache clean + I-cache
    // invalidate over the range, then a W^X page-permission flip to executable/read-only
    // (see `BareMetalCodeMemory` below).
    config.with_custom_code_memory(Some(Arc::new(BareMetalCodeMemory)));
    Engine::new(&config)
}

/// Host-feature probe for the ISA flags the x86_64 artifacts are compiled with
/// (`precompile_for_kernel` enables SSE3..SSE4.2 so float libcalls are never emitted; see
/// [`new_engine`]). Answers from CPUID leaf 1 ECX; anything not listed reports "unknown" so
/// the engine fails closed instead of executing instructions the CPU may not have.
#[cfg(target_arch = "x86_64")]
fn x86_detect_host_feature(feature: &str) -> Option<bool> {
    // SAFETY: the CPUID instruction is unprivileged and always present in long mode.
    let ecx = unsafe { core::arch::x86_64::__cpuid(1) }.ecx;
    match feature {
        "sse3" => Some(ecx & (1 << 0) != 0),
        "ssse3" => Some(ecx & (1 << 9) != 0),
        "sse4.1" => Some(ecx & (1 << 19) != 0),
        "sse4.2" => Some(ecx & (1 << 20) != 0),
        _ => None,
    }
}

/// Executable-memory "publisher" for this kernel's identity map, enforcing W^X.
///
/// Code — whether deserialized from an AOT artifact or emitted on-target by Cranelift
/// (plan/12 Decisions 26–27) — lands in an ordinary heap allocation, which the MMU maps
/// writable-but-non-executable by default (the per-arch `mmu` module), so it cannot be
/// executed while wasmtime is writing it. Publishing does two things: (1) real cache /
/// instruction-stream maintenance (`mmu::flush_code_range`), so the instruction-fetch path
/// sees the freshly written bytes (QEMU's TCG keeps coherency anyway, but physical hardware
/// does not); then (2) flip the range to executable-and-read-only, so a code page is never
/// simultaneously writable and executable. Unpublishing flips it back to writable/non-exec
/// so the allocation can be reused. `required_alignment` is the page size, so wasmtime hands
/// us whole pages that never share with non-code data.
struct BareMetalCodeMemory;

impl CustomCodeMemory for BareMetalCodeMemory {
    fn required_alignment(&self) -> usize {
        // Page granularity: code regions are whole pages so the W^X permission flip never
        // touches adjacent non-code data.
        4096
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        // SAFETY: the [ptr, ptr+len) range is the code memory wasmtime just wrote and is
        // about to execute. Cache-maintain it while it is still the writable heap default,
        // then flip it to executable/read-only. A zero-length publish is a no-op.
        unsafe {
            crate::mmu::flush_code_range(ptr, len);
            crate::mmu::set_range_permissions(
                ptr as usize,
                len,
                crate::mmu::PagePerm::ReadExecOnly,
            );
        }
        Ok(())
    }

    fn unpublish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        // Return the pages to the writable, non-executable heap default so the allocation can
        // be reused. SAFETY: wasmtime is done executing this region when it unpublishes.
        unsafe {
            crate::mmu::set_range_permissions(
                ptr as usize,
                len,
                crate::mmu::PagePerm::ReadWriteNoExec,
            );
        }
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

/// Safety-net `wfi` interval used when work is still in flight (a child is running) but no
/// nearer wake was requested: bounds re-poll latency so a compute-bound child whose fuel
/// yield is somehow not detected as runnable still advances. Sleep deadlines (via
/// [`request_timer_wake`]) and serial input (the UART RX interrupt) wake the core directly,
/// so this is only a backstop, not the normal cadence.
const IDLE_WAKE_INTERVAL_NS: u64 = 10_000_000;

/// Backstop `wfi` interval when nothing is running (the bare eosh prompt with no children):
/// input arrives via the UART RX interrupt, so the core need only wake about once a second
/// as a liveness backstop — this is what drops idle host CPU from the old ~1% toward ~0%.
const IDLE_BACKSTOP_NS: u64 = 1_000_000_000;

/// Floor on an armed wake so we never program a zero/at-deadline timer.
const MIN_WAKE_NS: u64 = 100_000;

/// Earliest absolute uptime (ns) any parked future has asked the executor to wake for —
/// `u64::MAX` means "nothing time-bound is pending". [`SleepUntil`](providers) lowers it to
/// its deadline each poll via [`request_timer_wake`]; [`idle_wait`] consumes and resets it.
static NEXT_TIMER_WAKE_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Ask the executor's idle `wfi` to wake no later than `deadline_ns` (absolute uptime).
/// Takes the earliest across all callers in a drive pass.
pub(crate) fn request_timer_wake(deadline_ns: u64) {
    NEXT_TIMER_WAKE_NS.fetch_min(deadline_ns, Ordering::AcqRel);
}

/// Where parked host-import futures ([`providers`]' `read-line`/`time.sleep`) leave the
/// wakers they want woken. [`wake_idle`] drains and wakes **all** of them after each
/// `wfi`, so wasmtime re-polls every parked future on the next loop. This must hold
/// every parked future, not just the most recent one: the console's `read-line` and a
/// service's `time.sleep` can be parked at the same time, and a single last-write-wins
/// slot would silently drop one of the two wakers — the dropped future is never
/// re-polled again (wasmtime only re-polls rung sub-futures), which for `read-line`
/// means a console that echoes nothing forever. The list stays tiny (one entry per
/// concurrently parked host future) and is drained on every wake, so it cannot grow.
/// Single-core: the lock is uncontended (the IRQ handler never touches it), but kept
/// explicit so the access is sound.
struct IdleWaker {
    locked: AtomicBool,
    wakers: UnsafeCell<Vec<Waker>>,
}

// SAFETY: all access goes through the `locked` flag below, on the kernel's single core.
unsafe impl Sync for IdleWaker {}

static IDLE_WAKER: IdleWaker = IdleWaker {
    locked: AtomicBool::new(false),
    wakers: UnsafeCell::new(Vec::new()),
};

impl IdleWaker {
    fn lock(&self) {
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// Register a waker to re-drive after the next `wfi` (called by a parked host future).
/// A waker that would re-poll the same future as one already registered is skipped
/// (`will_wake`), so a future re-registering across passes does not accumulate clones.
pub(crate) fn register_idle_waker(waker: &Waker) {
    IDLE_WAKER.lock();
    // SAFETY: exclusive while `locked` is held.
    let wakers = unsafe { &mut *IDLE_WAKER.wakers.get() };
    if !wakers.iter().any(|known| known.will_wake(waker)) {
        wakers.push(waker.clone());
    }
    IDLE_WAKER.unlock();
}

/// Wake (and clear) every registered idle waker, so wasmtime re-polls the parked futures.
/// Also called by the session drive loops on busy passes (a runnable child or service
/// keeps the loop hot, skipping `idle_wait`): the console's `read-line` parks on these
/// wakers, and without the wake it would never be re-polled — a spinning service must
/// not deafen the prompt.
pub(crate) fn wake_idle() {
    IDLE_WAKER.lock();
    // SAFETY: exclusive while `locked` is held.
    let wakers = core::mem::take(unsafe { &mut *IDLE_WAKER.wakers.get() });
    IDLE_WAKER.unlock();
    for waker in wakers {
        waker.wake();
    }
}

/// One idle step for a polling drive loop ([`block_on`] and the interactive shell): arm the
/// generic timer for the nearest pending wake, halt the core in `wfi` until that timer or a
/// UART RX interrupt fires, then re-drive any parked host-import future. This is what turns
/// the kernel's busy-poll into an idle wait — QEMU's vCPU (and a real core) sleeps between
/// polls instead of spinning.
///
/// The wake delay is the earliest of: a sleep deadline a parked future requested
/// ([`request_timer_wake`]), capped by a backstop. The backstop is short
/// ([`IDLE_WAKE_INTERVAL_NS`]) when a child is still running (so it keeps getting turns even
/// if its fuel-yield wake is missed) and long ([`IDLE_BACKSTOP_NS`]) when nothing is running
/// — at the bare prompt, input arrives as a UART interrupt, so the core can sleep ~1 s at a
/// time and idle near 0% instead of waking every 10 ms.
///
/// IRQs are masked across the `wfi`: a timer or UART interrupt that becomes pending in the
/// window between the caller's last poll and the `wfi` then stays pending and still wakes the
/// `wfi` (architecturally, a masked-but-pending IRQ is a `wfi` wake-up event), so there is no
/// lost-wakeup race; unmasking afterwards takes the interrupt (`kirq` services + EOIs it).
pub(crate) fn idle_wait(child_running: bool) -> WakeKind {
    let now = crate::timer::uptime_ns();
    let requested = NEXT_TIMER_WAKE_NS.swap(u64::MAX, Ordering::AcqRel);
    let cap = if child_running {
        IDLE_WAKE_INTERVAL_NS
    } else {
        IDLE_BACKSTOP_NS
    };
    // Cap-rated: the backstop, not a requested deadline, decides when we wake. A
    // requested deadline at or inside the cap makes the wake deadline-rated (the event
    // is time — SleepUntil, an IntxWait bound, a service restart).
    let cap_rated = requested == u64::MAX || requested.saturating_sub(now) > cap;
    let delay = if requested == u64::MAX {
        cap
    } else {
        requested.saturating_sub(now).clamp(MIN_WAKE_NS, cap)
    };
    // Mask interrupts, arm the timer wake, halt until an interrupt is pending, then unmask
    // and take it — the architecture-specific sequence lives in `timer::wait_for_interrupt`,
    // which is also the compiler-level memory barrier that makes whatever the interrupt
    // handler wrote (the UART input ring) visible to the re-poll below.
    crate::timer::wait_for_interrupt(delay);
    let woke = crate::timer::uptime_ns();
    // Early wake = an interrupt (UART RX, INTx, an earlier-armed timer) ended the halt
    // before the armed delay: event-driven, never a finding. Late + cap-rated = the
    // backstop fired; anything actionable it now discovers was stranded while we slept
    // (on this single core nothing changes guest-visible state during the masked `wfi`
    // except interrupts, and an interrupt would have woken us early).
    let late = woke.saturating_sub(now) >= delay;
    // Idle-path UART scavenge (plan/12, the paste-freeze fix): rescue any receive bytes
    // the interrupt path missed and, after a second of total input silence, nudge QEMU's
    // character feed (which has been observed to wedge under host load) back to life.
    // Runs on every idle wake — at least about once a second via the backstop above.
    let scavenged = crate::uart::scavenge_rx(woke);
    let kind = if !late {
        WakeKind::Event
    } else if cap_rated {
        WakeKind::Backstop
    } else {
        WakeKind::Deadline
    };
    if kind == WakeKind::Backstop {
        // Detector probes (SPEC: every backstop firing that discovers actionable work no
        // event delivered is a high-priority bug). The runnable-child probe lives in the
        // drive loops (shell.rs), which see the next pass's `any_runnable`.
        if scavenged > 0 {
            liveness_finding(
                "stranded input: the idle backstop scavenged receive bytes the interrupt \
                 path missed (possible benign race only within the same microsecond)",
                &LIVENESS_STRANDED_INPUT,
            );
        }
        let pending = crate::pci::intx_pending_total();
        if pending > 0 {
            liveness_finding(
                "stranded intx: deliveries were pending across an entire idle backstop",
                &LIVENESS_STRANDED_INTX,
            );
        }
    }
    wake_idle();
    kind
}

/// Why [`idle_wait`] returned: an interrupt (event-driven, the healthy path), the armed
/// deadline (the event is time), or the liveness backstop (no event arrived for a full
/// cap — anything the next pass finds runnable was stranded; see the backstop audit).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeKind {
    Event,
    Deadline,
    Backstop,
}

/// Per-kind running counts of backstop findings, and the rate-limited `liveness:` line.
/// Print on the first find and every 16th after, so a stranded-work regression is loud in
/// every transcript without flooding a wedged console.
static LIVENESS_STRANDED_INPUT: AtomicU64 = AtomicU64::new(0);
static LIVENESS_STRANDED_INTX: AtomicU64 = AtomicU64::new(0);
static LIVENESS_STRANDED_RUNNABLE: AtomicU64 = AtomicU64::new(0);

fn liveness_finding(what: &str, counter: &AtomicU64) {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(16) {
        crate::kprintln!("liveness: {what} (n={n})");
    }
}

/// The drive loops report a runnable child/service discovered immediately after a
/// [`WakeKind::Backstop`] wake — the work was runnable while the core slept the full cap.
pub(crate) fn liveness_stranded_runnable() {
    liveness_finding(
        "stranded runnable: a child or service was runnable across an entire idle backstop",
        &LIVENESS_STRANDED_RUNNABLE,
    );
}

/// Drive a wasmtime future (`instantiate_async`, `call_async`, …) to completion on the
/// kernel's single thread.
///
/// This is a polling executor: every pending host operation on this machine is time- or
/// input-driven (`time.sleep` against the generic timer, `read-line` against the PL011),
/// and those futures re-arm their waker on each poll, so the loop re-polls the top future
/// until it resolves, with a watchdog so a wedged guest cannot hang the boot. Between polls
/// the core idles in `wfi` (woken by a short generic-timer interrupt forwarded through the
/// GIC) rather than spinning, so an idle kernel — at the eosh prompt, or waiting out a
/// guest sleep — no longer pins a host CPU.
pub fn block_on<F: Future>(what: &str, future: F) -> Result<F::Output, wasmtime::Error> {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Doorbell));
    let mut cx = Context::from_waker(&waker);
    let deadline = crate::timer::uptime_ns().saturating_add(BLOCK_ON_WATCHDOG_NS);
    let mut last_wake = WakeKind::Event;
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => {
                if last_wake == WakeKind::Backstop {
                    // The future was ready only after a full backstop with no event: its
                    // wake edge was missed (it should have rung a waker or requested a
                    // timer wake).
                    liveness_stranded_runnable();
                }
                return Ok(value);
            }
            Poll::Pending => {
                if crate::timer::uptime_ns() > deadline {
                    return Err(wasmtime::Error::msg(alloc::format!(
                        "{what} did not complete within the kernel executor's watchdog"
                    )));
                }
                // Idle in `wfi` until the nearest pending wake (a sleep deadline) or a UART
                // RX interrupt fires, then re-drive the parked host future, instead of
                // busy-spinning. block_on drives a single future with no sibling children, so
                // `child_running = false`: it sleeps to the requested deadline (or the long
                // backstop). A guest awaiting `time.sleep`/`read-line` registered its waker
                // rather than self-waking, so this is what lets the core actually sleep.
                last_wake = idle_wait(false);
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

/// The `eo9:rt/configured.bind` export of a configured composition, if the component
/// carries one (see plan/03 D23 and wit/rt/rt.wit). Plain programs do not export it.
/// The executor contract: call it once after instantiation, before the first entry into
/// the program, so every compose-time configuration baked into the artifact is applied.
pub(crate) fn bind_entrypoint<T>(
    instance: &wasmtime::component::Instance,
    store: &mut wasmtime::Store<T>,
) -> Option<wasmtime::component::Func> {
    let configured = instance.get_export_index(&mut *store, None, "eo9:rt/configured@0.1.0")?;
    let bind = instance.get_export_index(&mut *store, Some(&configured), "bind")?;
    instance.get_func(&mut *store, bind)
}

/// The number of `Val` slots `bind`'s results need: 1 for the current
/// `func() -> result<_, string>` signature, 0 for artifacts composed before the error
/// channel existed (their configure errors still trap -- pre-existing behavior for
/// pre-existing bytes).
pub(crate) fn bind_result_slots<T>(
    bind: &wasmtime::component::Func,
    store: &wasmtime::Store<T>,
) -> usize {
    bind.ty(store).results().len()
}

/// The provider's configure-error text, if `bind`'s results carry one (the typed
/// pre-run refusal of a configuration the provider rejected).
pub(crate) fn configuration_refused(
    bind_results: &[wasmtime::component::Val],
) -> Option<alloc::string::String> {
    use alloc::borrow::ToOwned;
    match bind_results.first() {
        Some(wasmtime::component::Val::Result(Err(err))) => Some(match err.as_deref() {
            Some(wasmtime::component::Val::String(msg)) => msg.clone(),
            _ => "the provider rejected its baked configuration".to_owned(),
        }),
        _ => None,
    }
}
