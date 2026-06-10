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
#[cfg(feature = "wasm-store")]
pub mod console_sink_provider;
#[cfg(feature = "wasm-storedisk")]
pub mod diskcache;
#[cfg(feature = "wasm-store")]
pub mod dma;
#[cfg(all(feature = "wasm-codegen", feature = "wasm-store"))]
pub mod fibercompile;
#[cfg(feature = "wasm-hello")]
pub mod hello;
// The gfx.simplefb root provider exists only where its framebuffer does: the Orange Pi
// 5 Plus board profile. QEMU builds stay provider-absent (gfx.mem is the composition
// there) and refuse `eo9:gfx` imports with the capability story.
#[cfg(all(feature = "wasm-store", feature = "board-opi5plus"))]
pub mod gfx_provider;
// The kexec root provider (network kexec): aarch64 only — the dance is aarch64 cache
// maintenance + a relocation stub. Other ports answer the `kexec` token with the
// missing-machinery story (runner::boot) until their dance exists.
#[cfg(all(feature = "wasm-store", target_arch = "aarch64"))]
pub mod kexec_provider;
#[cfg(feature = "wasm-store")]
pub mod pci_provider;
#[cfg(feature = "wasm-store")]
pub mod platform_provider;
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
    // Sliced on-target codegen (fibercompile): route the vendored compiler's
    // per-function progress ticks to the fiber scheduler so long compiles yield to
    // the drive loop (idempotent; process-wide).
    #[cfg(all(feature = "wasm-codegen", feature = "wasm-store"))]
    wasmtime::set_compile_progress_callback(Some(fibercompile::progress_tick));

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

/// Floor on an armed wake so we never program a zero/at-deadline timer.
const MIN_WAKE_NS: u64 = 100_000;

/// QEMU profiles only: the character-feed kick obligation. QEMU's per-chunk pause/resume
/// of the serial feed has been observed to wedge with the RX FIFO *empty* (the
/// paste-freeze bug, plan/12) — there is nothing to event on, so the scavenger's kick is
/// an honest timed obligation: after a second of total input silence one dummy data-
/// register read resumes the feed (`uart::scavenge_rx`). This entry is what arms the wake
/// that runs it; the board profile carries no feed to kick and no such entry.
const FEED_KICK_INTERVAL_NS: u64 = 1_000_000_000;

/// Armed delay when no deadline exists at all (no parked sleep, no maintenance
/// obligation). Unreachable today — every QEMU profile carries the feed-kick entry and
/// the board profile carries the watchdog-pat entry — but the arm stays bounded so a
/// future profile without either cannot sleep unwakeably; interrupts still end it early.
const NO_DEADLINE_ARM_NS: u64 = 60_000_000_000;

/// The earliest maintenance obligation at/after `now`: the board watchdog pat (the
/// DW-WDT must be patted well inside its ~22 s timeout; the 5 s heartbeat rides the same
/// pat) and the QEMU feed kick ([`FEED_KICK_INTERVAL_NS`]). These are real "time T →
/// task X" entries in the owner's executor model — NOT a re-poll cadence: a wake rated
/// against one of these that discovers actionable guest work is a stranded-work liveness
/// finding (the backstop detectors in [`idle_wait`]).
fn maintenance_deadline_ns(now: u64) -> u64 {
    let pat = crate::wdt::pat_deadline_ns(now);
    let kick = if cfg!(all(target_arch = "aarch64", feature = "board-opi5plus")) {
        u64::MAX
    } else {
        now.saturating_add(FEED_KICK_INTERVAL_NS)
    };
    pat.min(kick)
}

/// Earliest absolute uptime (ns) any parked future has asked the executor to wake for —
/// `u64::MAX` means "nothing time-bound is pending". [`SleepUntil`](providers) lowers it to
/// its deadline each poll via [`request_timer_wake`]; [`idle_wait`] consumes and resets it.
static NEXT_TIMER_WAKE_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Ask the executor's idle `wfi` to wake no later than `deadline_ns` (absolute uptime).
/// Takes the earliest across all callers in a drive pass.
pub(crate) fn request_timer_wake(deadline_ns: u64) {
    NEXT_TIMER_WAKE_NS.fetch_min(deadline_ns, Ordering::AcqRel);
}

/// Deliver *due* events to parked futures during a hot stretch (a runnable child or
/// service is keeping the drive loop away from [`idle_wait`]): wake the parked input
/// futures when console input has arrived (the edge) or a requested sleep deadline has
/// expired — and only then.
///
/// This replaces the hot branch's old unconditional `wake_idle()`. The blanket wake
/// was a self-sustaining spin: it rang every parked future's poll waker each pass, the
/// next pass's rung flags then read as "runnable", so one hot pass made every later
/// pass hot and the executor never slept again — under the station config drive-stats
/// measured 27.8 M passes, all hot, zero `idle_wait` entries, i.e. 100% CPU at an idle
/// prompt. Waking only on due events breaks the cycle while keeping the contract the
/// old wake served: the console's `read-line` cannot go deaf while something spins,
/// because input arrival itself raises the edge (IRQ drain, scavenger, injector).
pub(crate) fn deliver_due_events() {
    let now = crate::timer::uptime_ns();
    let deadline_due = NEXT_TIMER_WAKE_NS.load(Ordering::Acquire) <= now;
    if deadline_due {
        // Consumed: woken sleepers re-arm on their re-poll (and anything still in the
        // future is re-requested then, exactly as after an `idle_wait` wake).
        NEXT_TIMER_WAKE_NS.store(u64::MAX, Ordering::Release);
    }
    if deadline_due || crate::rxring::input_edge_pending() {
        wake_idle();
    }
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
///
/// Delivering these wakes also consumes the console input edge
/// (`rxring::clear_input_edge`): every parked console reader has now been rung and will
/// re-poll against the ring, so input published before this point is delivered.
pub(crate) fn wake_idle() {
    IDLE_WAKER.lock();
    // SAFETY: exclusive while `locked` is held.
    let wakers = core::mem::take(unsafe { &mut *IDLE_WAKER.wakers.get() });
    IDLE_WAKER.unlock();
    crate::rxring::clear_input_edge();
    for waker in wakers {
        waker.wake();
    }
}

// --- The drive-pass rung registry (area/34-fuel-yield-latency) ---------------------------

/// Per-poll waker for one child/service drive: wasmtime re-polls the sub-futures whose
/// waker was rung; in addition this records *whether* the waker was rung at all during
/// (or after) the poll, which is how the drive loops tell work that wants an immediate
/// re-poll — a fuel yield rings it synchronously — from work parked on a host future
/// (`read-line`/`time.sleep` register the executor's idle waker instead). Shared by
/// `shellexec::drive_children` and `svc::drive_services` so the executor's park gate
/// ([`pass_rung_pending`]) can scan one registry covering both.
pub(crate) struct RungWaker {
    pub(crate) rung: AtomicBool,
}

impl Wake for RungWaker {
    fn wake(self: Arc<Self>) {
        self.rung.store(true, Ordering::Release);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.rung.store(true, Ordering::Release);
    }
}

/// The wakers handed out during the current drive pass, one per still-running child or
/// service (checked back in by `drive_children`/`drive_services`). The executor's idle
/// step scans them just before halting the core: a flag that became true *after* its
/// check-in (a wake edge the pass status missed) means runnable work exists and parking
/// would strand it for a full timer cadence — the executor must re-poll instead, and the
/// miss is a loud liveness finding (owner doctrine: a timer rescuing work the event path
/// owed is a bug). Same single-core lock discipline as [`IdleWaker`].
struct PassWakers {
    locked: AtomicBool,
    wakers: UnsafeCell<Vec<Arc<RungWaker>>>,
}

// SAFETY: all access goes through the `locked` flag below, on the kernel's single core.
unsafe impl Sync for PassWakers {}

static PASS_WAKERS: PassWakers = PassWakers {
    locked: AtomicBool::new(false),
    wakers: UnsafeCell::new(Vec::new()),
};

impl PassWakers {
    fn lock(&self) {
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// Start a fresh drive pass: forget the previous pass's wakers (their slots were either
/// re-polled by this pass or are no longer running). Called by `drive_children` at the
/// top of a non-nested pass — the services pump appends to the same pass.
pub(crate) fn begin_drive_pass() {
    PASS_WAKERS.lock();
    // SAFETY: exclusive while `locked` is held.
    unsafe { &mut *PASS_WAKERS.wakers.get() }.clear();
    PASS_WAKERS.unlock();
}

/// Record a still-running child/service's poll waker for the park-gate scan.
pub(crate) fn register_pass_waker(waker: Arc<RungWaker>) {
    PASS_WAKERS.lock();
    // SAFETY: exclusive while `locked` is held.
    unsafe { &mut *PASS_WAKERS.wakers.get() }.push(waker);
    PASS_WAKERS.unlock();
}

/// Whether any of this pass's checked-in children/services has been rung since its poll
/// — runnable work the pass's `any_runnable` status missed. Consuming (the flags are
/// cleared): the caller reacts by re-polling everything, which re-establishes fresh
/// flags, so a stale registry can never wedge the executor hot.
pub(crate) fn pass_rung_pending() -> bool {
    PASS_WAKERS.lock();
    // SAFETY: exclusive while `locked` is held.
    let pending = unsafe { &*PASS_WAKERS.wakers.get() }
        .iter()
        .map(|waker| waker.rung.swap(false, Ordering::AcqRel))
        .fold(false, |any, rung| any | rung);
    PASS_WAKERS.unlock();
    pending
}

/// One idle step for a polling drive loop ([`block_on`] and the interactive shell): arm the
/// generic timer for the nearest pending wake, halt the core in `wfi` until that timer or a
/// UART RX interrupt fires, then re-drive any parked host-import future. This is what turns
/// the kernel's busy-poll into an idle wait — QEMU's vCPU (and a real core) sleeps between
/// polls instead of spinning.
///
/// The executor model (owner ruling, area/34): either there is runnable work and the drive
/// loop is polling it hot (it never calls this), or the core halts armed to the **earliest
/// real deadline** — a wake a parked future requested ([`request_timer_wake`]), or a
/// maintenance obligation ([`maintenance_deadline_ns`]: the board watchdog pat, the QEMU
/// feed kick). There is no re-poll cadence: a runnable child is detected by its rung waker
/// (check-in or the park gate below), input by its interrupt/edge, a sleep by its armed
/// deadline. A late wake rated against a *maintenance* deadline that discovers actionable
/// guest work means that work was stranded — every such discovery is a loud liveness
/// finding (the detectors below), because the event path owed it a wake.
///
/// IRQs are masked across the `wfi`: a timer or UART interrupt that becomes pending in the
/// window between the caller's last poll and the `wfi` then stays pending and still wakes the
/// `wfi` (architecturally, a masked-but-pending IRQ is a `wfi` wake-up event), so there is no
/// lost-wakeup race; unmasking afterwards takes the interrupt (`kirq` services + EOIs it).
pub(crate) fn idle_wait() -> WakeKind {
    // Board profile: every idle wake pats the hardware watchdog (the busy passes pat in
    // the drive loops, so neither a parked nor a hot kernel starves it). No-op on QEMU.
    // The pat runs before the backstop-detector rating below: the watchdog is a
    // dead-man's switch for the whole kernel, not a liveness probe, so the ordering is
    // immaterial to the detector — patting first just keeps the dead-man margin maximal.
    crate::wdt::pat();
    // The park gate (area/34-fuel-yield-latency): never halt the core on work the event
    // path already delivered.
    //
    // 1. A checked-in child/service whose waker was rung after its poll (a wake edge the
    //    pass's `any_runnable` missed) is runnable NOW; sleeping on it strands it for a
    //    full timer cadence. This cannot happen through the normal paths — a fuel yield
    //    rings during the poll and is seen at check-in — so a hit here is a drive-loop
    //    regression: recover hot AND log loudly (owner doctrine: a timer flushing work
    //    the event path owed is a bug, and this detector must catch a regression of
    //    exactly that bug).
    if pass_rung_pending() {
        liveness_finding(
            "stranded runnable at the park gate: a child or service became runnable \
             after its check-in and the executor was about to sleep on it",
            &LIVENESS_PARK_RUNNABLE,
        );
        #[cfg(feature = "drive-stats")]
        drive_stats::GATE_CATCH.fetch_add(1, Ordering::Relaxed);
        wake_idle();
        return WakeKind::Event;
    }
    // 2. Console input published since the parked readers were last woken (a mid-pass
    //    UART drain, or the console-sink injector — the USB keyboard path, which raises
    //    no interrupt at all). Deliver the wake now instead of riding the next timer:
    //    this is the event path doing its job, not a finding.
    if crate::rxring::input_edge_pending() {
        #[cfg(feature = "drive-stats")]
        drive_stats::EDGE_BOUNCE.fetch_add(1, Ordering::Relaxed);
        wake_idle();
        return WakeKind::Event;
    }
    let now = crate::timer::uptime_ns();
    let requested = NEXT_TIMER_WAKE_NS.swap(u64::MAX, Ordering::AcqRel);
    // The single armed timer points at the earliest real deadline: a parked future's
    // requested wake, or a maintenance obligation (watchdog pat / feed kick). No cap:
    // the old 10 ms-while-running interval existed to bound re-poll latency "in case a
    // fuel yield is somehow not detected" — that miss is now detected (check-in rung +
    // the park gate above) and any regression of it is a loud finding, so the cadence
    // would only ever hide bugs (owner doctrine).
    let maintenance = maintenance_deadline_ns(now);
    // Every profile carries a maintenance entry (the board's watchdog pat, every other
    // profile's feed kick), so the defensive NO_DEADLINE_ARM_NS arm below is unreachable
    // today — keep that fact loud in debug images.
    debug_assert!(
        maintenance != u64::MAX,
        "no maintenance deadline on this profile: the idle arm would fall back to the \
         defensive 60 s arm"
    );
    let target = requested.min(maintenance);
    // Maintenance-rated: no parked future's deadline decides this wake — anything
    // actionable a late wake then discovers was stranded. A requested deadline at or
    // before the maintenance entry makes the wake deadline-rated (the event is time —
    // SleepUntil, an IntxWait bound, a service restart).
    let maintenance_rated = requested == u64::MAX || maintenance < requested;
    let delay = if target == u64::MAX {
        NO_DEADLINE_ARM_NS
    } else {
        target.saturating_sub(now).max(MIN_WAKE_NS)
    };
    // Mask interrupts FIRST, then re-check the input edge inside the masked window, and
    // only then arm + halt. An RX interrupt taken between the unmasked gate checks at
    // the top of this function and this mask has already been fully serviced — FIFO
    // drained into the ring, edge raised, EOI'd — so nothing is left pending to end the
    // `wfi`: parking would strand that input for the entire armed window (the deleted
    // 10 ms cap used to bound this race silently; the masked re-check is what closes it
    // honestly). Rung wakers need no masked re-check: they are rung from poll context
    // on this same core, never from an interrupt handler. The bail re-publishes the
    // consumed deadline request so the bail is behaviorally a pure no-op.
    crate::timer::irq_mask();
    if crate::rxring::input_edge_pending() {
        crate::timer::irq_unmask();
        if requested != u64::MAX {
            request_timer_wake(requested);
        }
        #[cfg(feature = "drive-stats")]
        drive_stats::EDGE_BOUNCE.fetch_add(1, Ordering::Relaxed);
        wake_idle();
        return WakeKind::Event;
    }
    // Arm the timer wake, halt until an interrupt is pending, then unmask and take it —
    // the architecture-specific sequence lives in `timer::wait_for_interrupt_masked`,
    // which is also the compiler-level memory barrier that makes whatever the interrupt
    // handler wrote (the UART input ring) visible to the re-poll below.
    crate::timer::wait_for_interrupt_masked(delay);
    let woke = crate::timer::uptime_ns();
    // Early wake = an interrupt (UART RX, INTx, an earlier-armed timer) ended the halt
    // before the armed delay: event-driven, never a finding. Late + maintenance-rated =
    // no event arrived for the whole armed window; anything actionable it now discovers
    // was stranded while we slept (on this single core nothing changes guest-visible
    // state during the masked `wfi` except interrupts, and an interrupt would have woken
    // us early).
    let late = woke.saturating_sub(now) >= delay;
    // Idle-path UART scavenge (plan/12, the paste-freeze fix): rescue any receive bytes
    // the interrupt path missed and, on QEMU profiles, run the feed-kick obligation
    // (after a second of total input silence, nudge QEMU's character feed — which has
    // been observed to wedge under host load — back to life). Runs on every idle wake;
    // the feed-kick maintenance entry guarantees one at least about once a second on
    // QEMU even when nothing else is due.
    let scavenged = crate::uart::scavenge_rx(woke);
    let kind = if !late {
        WakeKind::Event
    } else if maintenance_rated {
        WakeKind::Backstop
    } else {
        WakeKind::Deadline
    };
    #[cfg(feature = "drive-stats")]
    match kind {
        WakeKind::Event => drive_stats::WAKE_EVENT.fetch_add(1, Ordering::Relaxed),
        WakeKind::Deadline => drive_stats::WAKE_DEADLINE.fetch_add(1, Ordering::Relaxed),
        WakeKind::Backstop => drive_stats::WAKE_BACKSTOP.fetch_add(1, Ordering::Relaxed),
    };
    // Detector probes (SPEC: actionable work a wake discovers that no event delivered is
    // a high-priority bug). The runnable-child probe lives in the drive loops (shell.rs),
    // which see the next pass's `any_runnable`; the park gate above covers post-check-in
    // rungs.
    //
    // Scavenged bytes are a finding on EVERY wake kind, not just maintenance-rated ones
    // (area/35 timer-crutch audit): the RX interrupt owed those bytes a delivery whatever
    // else woke the core — rating the report by wake kind made an Event- or Deadline-
    // rated rescue silent, exactly the rescuer shape the doctrine forbids.
    if scavenged > 0 {
        liveness_finding(
            "stranded input: an idle wake scavenged receive bytes the interrupt path \
             missed (possible benign race only within the same microsecond)",
            &LIVENESS_STRANDED_INPUT,
        );
    }
    if kind == WakeKind::Backstop {
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
static LIVENESS_PARK_RUNNABLE: AtomicU64 = AtomicU64::new(0);

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

/// Executor drive-loop counters (`drive-stats` feature; area/34-fuel-yield-latency).
/// Zero-cost when the feature is off — every increment site is cfg-gated. Dumped on
/// each consumed Ctrl-C and at session end, so a serial transcript carries the numbers.
#[cfg(feature = "drive-stats")]
pub(crate) mod drive_stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Drive-loop passes (one init/eosh loop iteration that reached the Pending arm).
    pub static PASSES: AtomicU64 = AtomicU64::new(0);
    /// Passes that stayed hot (`any_runnable` — re-poll without parking).
    pub static HOT_PASSES: AtomicU64 = AtomicU64::new(0);
    /// Individual child polls, and how many reported a rung waker (fuel yield).
    pub static CHILD_POLLS: AtomicU64 = AtomicU64::new(0);
    pub static CHILD_RUNG: AtomicU64 = AtomicU64::new(0);
    /// Individual service polls, and how many reported a rung waker.
    pub static SVC_POLLS: AtomicU64 = AtomicU64::new(0);
    pub static SVC_RUNG: AtomicU64 = AtomicU64::new(0);
    /// `idle_wait` outcomes by wake kind.
    pub static WAKE_EVENT: AtomicU64 = AtomicU64::new(0);
    pub static WAKE_DEADLINE: AtomicU64 = AtomicU64::new(0);
    pub static WAKE_BACKSTOP: AtomicU64 = AtomicU64::new(0);
    /// Park-gate catches (the liveness detector) and input-edge bounces.
    pub static GATE_CATCH: AtomicU64 = AtomicU64::new(0);
    pub static EDGE_BOUNCE: AtomicU64 = AtomicU64::new(0);

    /// Print one cumulative summary line (the consumer diffs successive dumps).
    pub fn dump(reason: &str) {
        crate::kprintln!(
            "drive-stats[{reason}]: passes={} hot={} child-polls={} child-rung={} \
             svc-polls={} svc-rung={} wake-event={} wake-deadline={} wake-backstop={} \
             gate-catch={} edge-bounce={}",
            PASSES.load(Ordering::Relaxed),
            HOT_PASSES.load(Ordering::Relaxed),
            CHILD_POLLS.load(Ordering::Relaxed),
            CHILD_RUNG.load(Ordering::Relaxed),
            SVC_POLLS.load(Ordering::Relaxed),
            SVC_RUNG.load(Ordering::Relaxed),
            WAKE_EVENT.load(Ordering::Relaxed),
            WAKE_DEADLINE.load(Ordering::Relaxed),
            WAKE_BACKSTOP.load(Ordering::Relaxed),
            GATE_CATCH.load(Ordering::Relaxed),
            EDGE_BOUNCE.load(Ordering::Relaxed),
        );
    }
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
                // Idle in `wfi` until the nearest pending wake (a sleep deadline, a
                // maintenance obligation) or a UART RX interrupt fires, then re-drive the
                // parked host future, instead of busy-spinning. A guest awaiting
                // `time.sleep`/`read-line` registered its waker rather than self-waking,
                // so this is what lets the core actually sleep.
                last_wake = idle_wait();
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
