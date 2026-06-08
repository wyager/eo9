//! Sliced on-target codegen: `Component::new` on a fiber, yielding to the drive loop.
//!
//! ## The problem (board round 9, plan/09 D46 / plan/12)
//!
//! On-target Cranelift compiles ran synchronously inside the exec `compile` host call —
//! on the calling task's poll, inside the session drive loop's `call.poll()`. For the
//! whole compile the drive loop never passed: no children/services pumped, no
//! `wdt::pat()`, no `hb` heartbeat. On the board the DW-WDT then fired mid-compile for
//! any composition needing more than ~22 s of Cranelift (bench-measured: a 486 KiB
//! 4-component fusion reset the board at codegen+18.2 s, exactly 22.4 s after the last
//! drive-loop pat).
//!
//! ## The shape of the fix
//!
//! Compilation becomes cooperative without changing the WIT surface or the guests:
//!
//! * The compile runs on a [`wasmtime_internal_fiber::Fiber`] — the same stack-switching
//!   primitive the component-model-async machinery already uses on this target — so the
//!   whole synchronous `Component::new` call stack can be suspended wholesale.
//! * The vendored wasmtime's sequential compile pipeline invokes a progress callback
//!   once per compiled unit (function/trampoline/builtin — `set_compile_progress_callback`,
//!   vendor/README.md). Our callback suspends the fiber when the current slice has run
//!   ~[`SLICE_NS`]; between slices [`pump`] runs one cooperative scheduling pass:
//!   `drive_children()` + `drive_services()` + `wdt::pat()` + `wake_idle()`. Children
//!   and services genuinely run while Cranelift works (the checkout discipline —
//!   `ChildSlot::Polling` — makes the nested pass skip whatever task is currently being
//!   polled, so a child compiling from inside its own poll cannot be re-entered).
//! * The watchdog pat stays HONEST (loop-safety doctrine): it happens only in `pump`,
//!   i.e. only after a slice of compilation actually completed and a scheduling pass
//!   ran. A compile wedged inside one unit never finishes a slice, never pats, and the
//!   board still resets. There is no IRQ-side or unconditional patting.
//! * The calling task stays blocked (the WIT `compile` is sync — semantically it must),
//!   but everything else schedules: `hb` keeps printing on the board, other sessions
//!   keep serving, and the bench quiet-detector sees a live machine.
//!
//! The full upgrade — making `compile` WIT-async so even the *caller* can do other work
//! — is the recorded kernel-lane follow-up; it ripples through the guest SDK, eosh,
//! telnetd, and the usermode runtime, and this fiber is the mechanism it would reuse.
//!
//! ## Nesting and cancellation
//!
//! A pumped child can itself reach `compile` (e.g. a service restart compiling a
//! composition while a session compile is parked). The inner compile gets its own fiber
//! and its own slices; the suspend pointer is saved and restored around it, and ticks
//! always route to the INNERMOST live compile. Inner pumps only pat (children are
//! already being driven by the outer pump's pass — re-driving from deeper levels adds
//! stack depth for no scheduling win). The fiber is always driven to completion within
//! the host call — never dropped suspended (the no_std fiber cannot unwind a parked
//! stack), so cancellation semantics are exactly the pre-fiber ones.

use alloc::vec::Vec;

use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime_internal_fiber::{Fiber, FiberStack, Suspend};

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// One compile slice: how long the fiber runs before yielding to a scheduling pass.
/// Long enough that slice overhead (one stack switch + one pump pass) is noise, short
/// enough that children/services and the watchdog see passes hundreds of times per
/// second during a long compile (the DW-WDT budget is 22.4 s; the heartbeat 5 s).
const SLICE_NS: u64 = 5_000_000;

/// Fiber stack size. Today's compiles run on the kernel's main stack, so this is sized
/// with generous headroom over that proven footprint (heap-allocated; freed at return).
const FIBER_STACK_BYTES: usize = 2 * 1024 * 1024;

/// Throttle for the "still compiling" liveness line (all profiles — under QEMU TCG a
/// big fusion takes tens of seconds too, and a silent gap reads as a hang to the
/// scripted harnesses' quiet detectors).
const PROGRESS_PRINT_NS: u64 = 5_000_000_000;

type CompileOut = Result<Component, wasmtime::Error>;
type CompileSuspend = Suspend<(), (), CompileOut>;

/// The innermost live compile's suspend handle (0 = none). Single boot core: the only
/// concurrency is the cooperative nesting documented above, which saves/restores this
/// around inner compiles on the kernel stack.
static SUSPEND_PTR: AtomicUsize = AtomicUsize::new(0);
/// Uptime at which the current slice ends and the next tick suspends.
static SLICE_DEADLINE_NS: AtomicU64 = AtomicU64::new(0);
/// Units (functions/trampolines) completed since the compile started — diagnostics.
static UNITS: AtomicU64 = AtomicU64::new(0);
/// Live pump depth: 0 = no compile pumping; >= 1 = inner pumps skip child driving.
static PUMP_DEPTH: AtomicU32 = AtomicU32::new(0);
/// Next uptime at which the throttled progress line prints.
static NEXT_PROGRESS_NS: AtomicU64 = AtomicU64::new(0);

/// The progress callback registered with the vendored wasmtime (`new_engine`): runs on
/// the FIBER stack, once per compiled unit of the sequential pipeline. Suspends the
/// fiber when the slice budget is spent; a no-op for compiles outside a fiber (the
/// boot-time codegen demo's 300-byte seed).
pub(super) fn progress_tick() {
    UNITS.fetch_add(1, Ordering::Relaxed);
    let raw = SUSPEND_PTR.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    if crate::timer::uptime_ns() < SLICE_DEADLINE_NS.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: `raw` was stored by the innermost live [`compile`] from the `&mut
    // Suspend` its fiber closure received, is cleared before that closure returns, and
    // is saved/restored around nested compiles — so it points at the suspend handle of
    // exactly the fiber this tick is executing on (we are ON its stack right now). The
    // original reference is not used concurrently: the closure converted it to a raw
    // pointer at entry and only this function dereferences it. Single boot core.
    let suspend = unsafe { &mut *(raw as *mut CompileSuspend) };
    suspend.suspend(());
    // Resumed: [`compile`] re-armed the slice deadline before resuming.
}

/// Compile `bytes` on a fiber, pumping the cooperative scheduler between slices.
/// Returns the component plus the number of slices the compile yielded (diagnostics:
/// `> 0` proves the drive loop interleaved).
pub(super) fn compile(engine: &Engine, bytes: &[u8]) -> (CompileOut, u64) {
    let stack = match FiberStack::new(FIBER_STACK_BYTES, false) {
        Ok(stack) => stack,
        Err(error) => {
            return (
                Err(wasmtime::Error::msg(alloc::format!(
                    "allocating the compile fiber stack failed: {error:?}"
                ))),
                0,
            );
        }
    };
    // Save the outer compile's state (nesting): ticks must route to the innermost
    // fiber while it lives, and back to the outer one afterwards.
    let outer_suspend = SUSPEND_PTR.load(Ordering::Relaxed);
    let outer_deadline = SLICE_DEADLINE_NS.load(Ordering::Relaxed);
    if outer_suspend == 0 {
        UNITS.store(0, Ordering::Relaxed);
        NEXT_PROGRESS_NS.store(
            crate::timer::uptime_ns() + PROGRESS_PRINT_NS,
            Ordering::Relaxed,
        );
    }

    let engine = engine.clone();
    let bytes: Vec<u8> = bytes.to_vec();
    let fiber = match Fiber::<(), (), CompileOut>::new(stack, move |_resume, suspend| {
        // Publish the suspend handle as a raw pointer for [`progress_tick`]; the
        // reference itself is not touched again until the closure returns.
        let suspend_raw = suspend as *mut CompileSuspend;
        SUSPEND_PTR.store(suspend_raw as usize, Ordering::Relaxed);
        let result = Component::new(&engine, &bytes);
        SUSPEND_PTR.store(0, Ordering::Relaxed);
        result
    }) {
        Ok(fiber) => fiber,
        Err(error) => {
            return (
                Err(wasmtime::Error::msg(alloc::format!(
                    "creating the compile fiber failed: {error:?}"
                ))),
                0,
            );
        }
    };

    let depth = PUMP_DEPTH.fetch_add(1, Ordering::Relaxed);
    let mut slices: u64 = 0;
    let result = loop {
        SLICE_DEADLINE_NS.store(crate::timer::uptime_ns() + SLICE_NS, Ordering::Relaxed);
        match fiber.resume(()) {
            // Finished (the closure cleared SUSPEND_PTR itself).
            Ok(result) => break result,
            // Slice expired: one cooperative scheduling pass, then resume.
            Err(()) => {
                slices += 1;
                pump(depth);
            }
        }
    };
    PUMP_DEPTH.fetch_sub(1, Ordering::Relaxed);
    // Restore the outer compile's routing (no-op for the outermost).
    SUSPEND_PTR.store(outer_suspend, Ordering::Relaxed);
    SLICE_DEADLINE_NS.store(outer_deadline, Ordering::Relaxed);
    (result, slices)
}

/// One cooperative scheduling pass between compile slices: pat the watchdog (honest —
/// a slice of real compilation just completed AND the scheduler is running), drive
/// every other task, wake input-parked futures, and keep the console honest about the
/// long compile.
fn pump(depth: u32) {
    crate::wdt::pat();
    if depth == 0 {
        // Drive children + services exactly as the session drive loops do between
        // root polls. The checkout discipline (ChildSlot::Polling / SRun::Polling)
        // skips whatever task this compile is running inside, so the pass cannot
        // re-enter it. CURRENT_PARENT is saved/restored so a spawn performed later in
        // the interrupted poll still records the right parent.
        let saved_parent = super::shellexec::save_current_parent();
        let _ = super::shellexec::drive_children();
        let _ = super::svc::drive_services();
        super::shellexec::restore_current_parent(saved_parent);
        super::wake_idle();
    }
    let now = crate::timer::uptime_ns();
    if now >= NEXT_PROGRESS_NS.load(Ordering::Relaxed) {
        NEXT_PROGRESS_NS.store(now + PROGRESS_PRINT_NS, Ordering::Relaxed);
        crate::kprintln!(
            "codegen: still compiling ({} functions so far)",
            UNITS.load(Ordering::Relaxed)
        );
    }
}
