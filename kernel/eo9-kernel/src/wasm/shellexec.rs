//! The shell session's execution providers (kernel side): `eo9:exec/component-algebra`,
//! `compile`, and `task`.
//!
//! The semantics mirror the usermode runtime (`crates/eo9-runtime`), restricted to what
//! the bare-metal kernel can honestly do today (plan/12-kernel.md Decision 21):
//!
//! * **component-algebra** — `load` recognises the components baked into the read-only
//!   store image by content (and, with `wasm-codegen`, validates arbitrary component bytes
//!   too); `describe` replays the metadata xtask computed at image-assembly time for store
//!   entries and decodes fused results with the same `eo9-component` crate usermode uses;
//!   `save` returns the bytes. With `wasm-codegen` the combinators (`compose`, `extend`,
//!   `restrict`, `rename`, `configure`) run the real `eo9-component` algebra, producing a
//!   fused component compiled on-target; without it they fail with a clear error.
//! * **compile** — for a pristine store entry, a content lookup of its baked-in host-AOT
//!   artifact (the fast path); for a fused algebra result, on-target Cranelift codegen
//!   (`Component::new`, plan/12 Decision 29). Providers are rejected with `not-a-binary`.
//! * **task** — `spawn` instantiates the artifact against the full session environment —
//!   the kernel root providers (text/time/entropy) plus the read-only store filesystem,
//!   io buffers, and the whole `eo9:exec` surface, the same inherit-everything default as
//!   usermode (restrict with `only`) — and binds `main`'s WAVE arguments against its
//!   signature; the child then executes on the shell's drive loop (`drive_children`),
//!   interleaved with the shell itself, exactly as usermode children execute inside their
//!   parent's resume — wasmtime forbids re-entering the event loop from a host function.
//!   `wait`/`runnable`/`kill` observe the child; `resume` (guest-directed fuel donation) is
//!   unsupported, as in usermode (E5).
//!
//! Children are fuel-metered (the engine enables `consume_fuel`, matched by xtask's
//! precompile configuration): instantiation runs on a small bounded budget, and the call to
//! `main` runs from an effectively-infinite pool sliced by [`FUEL_QUANTUM`] — every poll of
//! a child executes at most one quantum and then yields, so a compute-bound (or
//! deliberately spinning) child is preempted and the other children plus the shell keep
//! making progress. This is the same regime as the usermode runtime (`crates/eo9-runtime`).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};

use wasmtime::component::{
    Accessor, Component, ComponentType, Lift, Linker, Lower, Resource, ResourceType, Type, Val,
};
use wasmtime::{Engine, Result, Store, StoreContextMut};

use super::providers::{self, KernelState};
use super::wave;

/// Boxed future shape for `func_wrap_concurrent` closures.
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

/// How much fuel a child executes per poll before yielding back to the drive loop — the
/// preemption quantum. Same value as the usermode runtime's `FUEL_QUANTUM`
/// (crates/eo9-runtime/src/task.rs) so a "slice" means the same thing on both targets.
pub const FUEL_QUANTUM: u64 = 10_000;

/// Fuel budget for a child's instantiation (start-time code), mirroring usermode
/// `SPAWN_FUEL`: enough for the trivial start sections eo9 components have, small enough
/// that a hostile component cannot burn unbounded CPU before `spawn` even returns.
pub const SPAWN_FUEL: u64 = 4 * FUEL_QUANTUM;

// -----------------------------------------------------------------------------------------
// Host resource representations
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:exec/component-algebra.component`.
pub struct AlgComponentRes;
/// Host representation of `eo9:exec/images.image`.
pub struct ExecImageRes;
/// Host representation of `eo9:exec/task.task`.
pub struct ChildTaskRes;

// -----------------------------------------------------------------------------------------
// WIT-shaped host types (eo9:exec)
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
pub enum WitComponentKind {
    #[component(name = "binary")]
    Binary,
    #[component(name = "provider")]
    Provider,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
pub struct WitImportNeed {
    pub slot: String,
    #[component(name = "interface")]
    pub interface: String,
    pub version: String,
    pub required: bool,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
pub struct WitExportSlot {
    pub name: String,
    #[component(name = "interface")]
    pub interface: String,
    pub version: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
pub struct WitArgSpec {
    pub name: String,
    pub ty: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
pub struct WitComponentInfo {
    pub kind: WitComponentKind,
    pub imports: Vec<WitImportNeed>,
    pub exports: Vec<WitExportSlot>,
    pub args: Vec<WitArgSpec>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
#[allow(dead_code)]
struct WitInterfaceRef {
    #[component(name = "interface")]
    interface: String,
    version: Option<String>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitNamedArg {
    name: String,
    value: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitLoadError {
    #[component(name = "invalid-component")]
    InvalidComponent(String),
    #[component(name = "not-an-eo9-module")]
    NotAnEo9Module(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitComposeError {
    #[component(name = "not-a-provider")]
    NotAProvider,
    #[component(name = "type-mismatch")]
    TypeMismatch(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitRestrictError {
    #[component(name = "required-outside-allow-list")]
    RequiredOutsideAllowList(Vec<String>),
    #[component(name = "invalid-allow-list")]
    InvalidAllowList(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitRenameError {
    #[component(name = "no-such-slot")]
    NoSuchSlot(String),
    #[component(name = "slot-collision")]
    SlotCollision(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitConfigureError {
    #[component(name = "not-a-provider")]
    NotAProvider,
    #[component(name = "no-config-interface")]
    NoConfigInterface,
    #[component(name = "invalid-args")]
    InvalidArgs(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
#[allow(dead_code)]
struct WitCompileOpts {
    #[component(name = "debug-info")]
    debug_info: bool,
    #[component(name = "safepoint-maps")]
    safepoint_maps: bool,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitCompileError {
    #[component(name = "not-a-binary")]
    NotABinary,
    #[component(name = "not-closed")]
    NotClosed(Vec<String>),
    #[component(name = "codegen")]
    Codegen(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitWaveValue {
    ty: String,
    value: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitAbnormalExit {
    #[component(name = "trapped")]
    Trapped(String),
    #[component(name = "killed")]
    Killed,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitProgramOutcome {
    #[component(name = "success")]
    Success(WitWaveValue),
    #[component(name = "failure")]
    Failure(WitWaveValue),
    #[component(name = "abnormal")]
    Abnormal(WitAbnormalExit),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
#[allow(dead_code)]
struct WitSpawnLimits {
    #[component(name = "max-memory")]
    max_memory: Option<u64>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitSpawnError {
    #[component(name = "bad-arguments")]
    BadArguments(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitResumeOutcome {
    #[component(name = "out-of-fuel")]
    OutOfFuel,
    #[component(name = "blocked")]
    Blocked,
    #[component(name = "done")]
    Done(WitProgramOutcome),
}

// -----------------------------------------------------------------------------------------
// Outcomes
// -----------------------------------------------------------------------------------------

/// A child program's final outcome (kernel-side mirror of the usermode `Outcome`).
#[derive(Clone)]
pub enum KOutcome {
    Success { ty: String, value: String },
    Failure { ty: String, value: String },
    Trapped(String),
    Killed,
}

fn wit_outcome(outcome: &KOutcome) -> WitProgramOutcome {
    match outcome {
        KOutcome::Success { ty, value } => WitProgramOutcome::Success(WitWaveValue {
            ty: ty.clone(),
            value: value.clone(),
        }),
        KOutcome::Failure { ty, value } => WitProgramOutcome::Failure(WitWaveValue {
            ty: ty.clone(),
            value: value.clone(),
        }),
        KOutcome::Trapped(reason) => {
            WitProgramOutcome::Abnormal(WitAbnormalExit::Trapped(reason.clone()))
        }
        KOutcome::Killed => WitProgramOutcome::Abnormal(WitAbnormalExit::Killed),
    }
}

/// Render a completed `main` return value into a [`KOutcome`] (the same rule as the
/// usermode runtime's `wave::render_outcome`).
fn render_outcome(result_ty: Option<&Type>, val: Option<&Val>) -> KOutcome {
    let render = |ty: Option<Type>, payload: Option<&Val>| -> (String, String) {
        match (ty, payload) {
            (Some(ty), Some(val)) => (wave::type_text(&ty), wave::render(val)),
            _ => (String::new(), String::new()),
        }
    };
    match (result_ty, val) {
        (Some(Type::Result(result_ty)), Some(Val::Result(result_val))) => match result_val {
            Ok(payload) => {
                let (ty, value) = render(result_ty.ok(), payload.as_deref());
                KOutcome::Success { ty, value }
            }
            Err(payload) => {
                let (ty, value) = render(result_ty.err(), payload.as_deref());
                KOutcome::Failure { ty, value }
            }
        },
        (Some(ty), Some(val)) => KOutcome::Success {
            ty: wave::type_text(ty),
            value: wave::render(val),
        },
        _ => KOutcome::Success {
            ty: String::new(),
            value: String::new(),
        },
    }
}

// -----------------------------------------------------------------------------------------
// The child registry (shared between the host functions and the shell drive loop)
// -----------------------------------------------------------------------------------------

/// A minimal spinlock for the single-core kernel: it exists to make the static child
/// registry `Sync`; with one core and the lock never held across a yield it cannot
/// actually contend.
struct KLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for KLock<T> {}

impl<T> KLock<T> {
    const fn new(value: T) -> Self {
        KLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        // SAFETY: the flag gives exclusive access; the paths that take this lock never
        // re-enter it (children cannot reach the exec surface).
        let result = f(unsafe { &mut *self.value.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

/// One spawned child.
enum ChildSlot {
    /// Still executing: the drive future owns the child's store and the one call to `main`.
    Running(Pin<Box<dyn Future<Output = KOutcome> + Send>>),
    /// Temporarily checked out by [`drive_children`], which polls the drive future *without*
    /// holding the registry lock — that is what lets the child itself reach the registry
    /// (a nested eosh spawning, waiting on, or killing its own children) without
    /// deadlocking on the single-core spinlock (plan/12 D36).
    Polling,
    /// Finished (or killed); later observations see this outcome.
    Done(KOutcome),
}

/// The child registry: task rep → child. Static because the shell's drive loop must reach
/// the children while the shell's own store is mutably borrowed by its in-flight call.
static CHILDREN: KLock<Vec<Option<ChildSlot>>> = KLock::new(Vec::new());

/// Waker used when polling child drives: wake-ups only need to be *recordable* (wasmtime
/// re-polls the sub-futures whose waker was rung); the drive loop polls every iteration.
struct ChildWaker;

impl alloc::task::Wake for ChildWaker {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

/// Reset the registry (called once when a shell session starts, so reps are dense and a
/// stale handle from a previous session cannot alias a new child).
pub fn reset_children() {
    CHILDREN.with(|children| children.clear());
}

/// What one registry index held when [`drive_children`] looked at it.
enum Taken {
    /// Past the end of the registry.
    End,
    /// Nothing runnable here (empty, finished, or already checked out).
    Skip,
    /// A runnable child, checked out for one poll.
    Run(Pin<Box<dyn Future<Output = KOutcome> + Send>>),
}

/// Poll every running child once. Called from the shell's drive loop between polls of the
/// shell itself — the bare-metal counterpart of children executing inside their parent's
/// resume in usermode.
///
/// Each child is *checked out* of the registry (its slot set to [`ChildSlot::Polling`]),
/// polled with the lock released, and then checked back in. Holding the lock across the
/// poll would deadlock the moment a child reaches the exec surface itself — a nested eosh
/// spawning a grandchild, waiting on it, or killing it all take the same lock (plan/12
/// D36). Children spawned *during* this pass land at the end of the registry and get their
/// first poll in the same pass (the index re-checks the length each iteration); slots are
/// never removed or reordered, so the index stays valid across the unlocked poll.
pub fn drive_children() {
    let mut index = 0usize;
    loop {
        let taken = CHILDREN.with(|children| {
            if index >= children.len() {
                return Taken::End;
            }
            match &mut children[index] {
                Some(slot @ ChildSlot::Running(_)) => {
                    match core::mem::replace(slot, ChildSlot::Polling) {
                        ChildSlot::Running(drive) => Taken::Run(drive),
                        // `slot` matched Running above; replace returned that same value.
                        _ => unreachable!("checked-out slot was not running"),
                    }
                }
                _ => Taken::Skip,
            }
        });

        let mut drive = match taken {
            Taken::End => break,
            Taken::Skip => {
                index += 1;
                continue;
            }
            Taken::Run(drive) => drive,
        };

        // Poll with the registry unlocked. With fuel slicing (`spawn_child`) this runs at
        // most one quantum of guest code before yielding back here.
        let waker = Waker::from(Arc::new(ChildWaker));
        let mut cx = Context::from_waker(&waker);
        let polled = drive.as_mut().poll(&mut cx);

        CHILDREN.with(|children| {
            let slot = &mut children[index];
            match slot {
                // Normal case: still checked out to us — check it back in.
                Some(ChildSlot::Polling) => {
                    *slot = Some(match polled {
                        Poll::Ready(outcome) => ChildSlot::Done(outcome),
                        Poll::Pending => ChildSlot::Running(drive),
                    });
                }
                // The child was killed (slot now Done) or its handle was dropped (slot now
                // None) while we were polling it: keep that state; dropping the checked-out
                // drive future here releases the child's store and any in-flight work.
                _ => {}
            }
        });
        index += 1;
    }
}

// -----------------------------------------------------------------------------------------
// The boot-time scheduling demonstration (`cargo xtask qemu aarch64 demo`)
// -----------------------------------------------------------------------------------------

/// Demonstrate child preemption headlessly, with the same spawn / drive / kill machinery the
/// interactive shell uses: three cruncher children — a short computation, a long one, and a
/// deliberate spinner (`u64::MAX` rounds) — share one drive loop. The short child finishes
/// while the long one is still mid-computation (every poll runs at most [`FUEL_QUANTUM`]
/// fuel, so the loop interleaves them), and the spinner — which before fuel metering would
/// have monopolized the machine forever — is killed cleanly while still spinning.
pub fn preemption_demo(entries: &'static [super::store::StoreEntry]) {
    crate::kprintln!(
        "sched demo: three cruncher children on one drive loop (short 200k rounds, long 2M \
         rounds, spinner u64::MAX rounds), preempted every {FUEL_QUANTUM} fuel"
    );
    if let Err(error) = try_preemption_demo(entries) {
        crate::kprintln!("sched demo: FAILED: {error:?}");
    }
    // Leave a clean registry behind for whatever runs next.
    reset_children();
}

fn try_preemption_demo(entries: &'static [super::store::StoreEntry]) -> Result<()> {
    let cruncher = entries
        .iter()
        .find(|entry| entry.name == "cruncher")
        .ok_or_else(|| wasmtime::Error::msg("the baked-in store has no `cruncher` entry"))?;

    let engine = super::new_engine()?;
    // SAFETY: the artifact comes from the store image produced by `cargo xtask build-kernel`
    // with the same wasmtime version and engine configuration.
    let component = unsafe { Component::deserialize(&engine, cruncher.artifact)? };

    reset_children();
    let spawn = |seed: u64, rounds: u64| -> Result<u32> {
        let args = [
            WitNamedArg {
                name: String::from("seed"),
                value: format!("{seed}"),
            },
            WitNamedArg {
                name: String::from("rounds"),
                value: format!("{rounds}"),
            },
        ];
        spawn_child(&engine, entries, &component, &args, None).map_err(|err| {
            wasmtime::Error::msg(match err {
                WitSpawnError::BadArguments(msg) => format!("spawn failed (bad arguments): {msg}"),
                WitSpawnError::Internal(msg) => format!("spawn failed: {msg}"),
            })
        })
    };
    let short = spawn(9, 200_000)?;
    let long = spawn(9, 2_000_000)?;
    let spinner = spawn(9, u64::MAX)?;

    let outcome_of = |rep: u32| -> Option<KOutcome> {
        CHILDREN.with(|children| match children.get(rep as usize) {
            Some(Some(ChildSlot::Done(outcome))) => Some(outcome.clone()),
            _ => None,
        })
    };
    let label = |outcome: &KOutcome| -> String {
        match outcome {
            KOutcome::Success { value, .. } => format!("success({value})"),
            KOutcome::Failure { value, .. } => format!("failure({value})"),
            KOutcome::Trapped(reason) => format!("abnormal(trapped({reason}))"),
            KOutcome::Killed => String::from("abnormal(killed)"),
        }
    };

    // Drive until the long child finishes, reporting interleaving evidence along the way.
    // The bound exists so a regression cannot wedge the boot demo.
    const MAX_TURNS: u64 = 5_000_000;
    let mut turns: u64 = 0;
    let mut short_done = false;
    loop {
        drive_children();
        turns += 1;
        if turns > MAX_TURNS {
            return Err(wasmtime::Error::msg(
                "the scheduling demo exceeded its turn bound",
            ));
        }
        if !short_done {
            if let Some(outcome) = outcome_of(short) {
                short_done = true;
                crate::kprintln!(
                    "sched demo: short finished after {turns} turns -> {} (long still \
                     running: {}, spinner still running: {})",
                    label(&outcome),
                    outcome_of(long).is_none(),
                    outcome_of(spinner).is_none()
                );
            }
        }
        if let Some(outcome) = outcome_of(long) {
            crate::kprintln!(
                "sched demo: long finished after {turns} turns -> {} (spinner still \
                 running: {})",
                label(&outcome),
                outcome_of(spinner).is_none()
            );
            break;
        }
    }

    // The spinner would run forever; kill it exactly the way the shell's `task.kill` does
    // and confirm the registry reports the kill.
    let was_running = CHILDREN.with(|children| match children.get_mut(spinner as usize) {
        Some(slot) => match slot {
            Some(ChildSlot::Running(_) | ChildSlot::Polling) => {
                *slot = Some(ChildSlot::Done(KOutcome::Killed));
                true
            }
            _ => false,
        },
        None => false,
    });
    let spinner_outcome = outcome_of(spinner).unwrap_or(KOutcome::Killed);
    crate::kprintln!(
        "sched demo: killed the spinner after {turns} turns -> {} (was still running: \
         {was_running})",
        label(&spinner_outcome)
    );
    crate::kprintln!(
        "sched demo: a compute-bound or spinning child no longer takes the machine; every \
         child runs in {FUEL_QUANTUM}-fuel slices on the shared drive loop"
    );
    Ok(())
}

// -----------------------------------------------------------------------------------------
// Spawning
// -----------------------------------------------------------------------------------------

/// Bind `main`'s WAVE-encoded named arguments against its signature (the usermode
/// `parse_args` rule: every declared parameter exactly once, no unknown arguments).
fn bind_args(
    signature: &wasmtime::component::types::ComponentFunc,
    args: &[WitNamedArg],
) -> Result<Vec<Val>, String> {
    let params: Vec<(String, Type)> = signature
        .params()
        .map(|(name, ty)| (name.to_string(), ty))
        .collect();
    for arg in args {
        if !params.iter().any(|(name, _)| *name == arg.name) {
            return Err(format!("unknown argument `{}`", arg.name));
        }
    }
    let mut vals = Vec::with_capacity(params.len());
    for (name, ty) in &params {
        let matching: Vec<&WitNamedArg> = args.iter().filter(|arg| arg.name == *name).collect();
        let arg = match matching.as_slice() {
            [] => return Err(format!("missing argument `{name}`")),
            [arg] => *arg,
            _ => return Err(format!("argument `{name}` supplied more than once")),
        };
        let val = wave::parse(ty, &arg.value).map_err(|err| {
            format!(
                "argument `{name}` is not a valid `{}`: {err}",
                wave::type_text(ty)
            )
        })?;
        vals.push(val);
    }
    Ok(vals)
}

/// Instantiate a child from its precompiled component, bind `main`'s arguments, and park
/// it in the registry. No guest code beyond instantiation runs here; the child executes on
/// the shell's drive loop.
///
/// Children inherit the full session environment — the kernel root providers
/// (text/time/entropy), the read-only store filesystem (`/bin`, `/session`), the io
/// buffers, and the whole `eo9:exec` surface — every generation, exactly like usermode
/// children since the layered-session change (plan/11 D14–15). The loader rule keeps this
/// honest: a child only links the interfaces its (possibly `only`-restricted) component
/// imports, so granting the full set is inert for programs that never asked for it, and a
/// nested `eosh` is a full peer that can resolve `/bin`, spawn, and compose.
fn spawn_child(
    engine: &Engine,
    entries: &'static [super::store::StoreEntry],
    component: &Component,
    args: &[WitNamedArg],
    max_memory: Option<u64>,
) -> Result<u32, WitSpawnError> {
    let internal = |err: wasmtime::Error| {
        let text = format!("{err:?}");
        WitSpawnError::Internal(match missing_capability(&text) {
            Some(friendly) => friendly,
            None => text,
        })
    };

    let mut linker: Linker<KernelState> = Linker::new(engine);
    providers::add_providers(&mut linker).map_err(internal)?;
    super::shellfs::add_buffers(&mut linker).map_err(internal)?;
    super::shellfs::add_fs(&mut linker).map_err(internal)?;
    add_exec(&mut linker).map_err(internal)?;

    let mut state = KernelState::new();
    state.shell = Some(Box::new(super::shell::ShellState {
        fs: super::shellfs::ShellFs::new(entries, super::shell::session_manifest(entries)),
        buffers: super::shellfs::BufferTable::default(),
        exec: ShellExec::default(),
        engine: engine.clone(),
    }));
    let mut store = Store::new(engine, state);
    if let Some(max_memory) = max_memory {
        store.data_mut().set_max_memory(max_memory);
        store.limiter(|state| state.limiter());
    }

    // Instantiation runs on a small bounded fuel budget paid by the spawner (usermode
    // `SPAWN_FUEL` parity): any start-time code either finishes within it or the spawn
    // fails — never an unbounded burn. It must also not depend on external completions;
    // drive it with a bounded poll loop, as usermode `spawn` does.
    store.set_fuel(SPAWN_FUEL).map_err(internal)?;
    let instance = {
        let instantiate = linker.instantiate_async(&mut store, component);
        let mut instantiate = core::pin::pin!(instantiate);
        let waker = Waker::from(Arc::new(ChildWaker));
        let mut cx = Context::from_waker(&waker);
        let mut result = None;
        for _ in 0..4096 {
            match instantiate.as_mut().poll(&mut cx) {
                Poll::Ready(r) => {
                    result = Some(r);
                    break;
                }
                Poll::Pending => continue,
            }
        }
        result
            .ok_or_else(|| {
                WitSpawnError::Internal("instantiation unexpectedly suspended".to_string())
            })?
            .map_err(|err| {
                if matches!(
                    err.downcast_ref::<wasmtime::Trap>(),
                    Some(wasmtime::Trap::OutOfFuel)
                ) {
                    WitSpawnError::Internal(format!(
                        "component start-time code exceeded the spawn fuel budget \
                         ({SPAWN_FUEL} fuel): instantiation must not run unbounded guest code"
                    ))
                } else {
                    internal(err)
                }
            })?
    };

    // Normal fuel regime for the child's life (usermode parity): an effectively-infinite
    // pool sliced by the fixed yield quantum, so every poll of the child runs at most
    // FUEL_QUANTUM units and then yields back to the drive loop — that slicing is what
    // keeps a compute-bound child from monopolizing the machine.
    store.set_fuel(u64::MAX).map_err(internal)?;
    store
        .fuel_async_yield_interval(Some(FUEL_QUANTUM))
        .map_err(internal)?;

    let main = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| WitSpawnError::Internal("component does not export `main`".to_string()))?;
    let signature = main.ty(&store);
    let params = bind_args(&signature, args).map_err(WitSpawnError::BadArguments)?;
    let result_ty = signature.results().next();

    // The drive future owns the child's store and performs the one call to `main`.
    let drive = Box::pin(async move {
        let mut store = store;
        let mut results = vec![Val::Bool(false)];
        match main.call_async(&mut store, &params, &mut results).await {
            Ok(()) => render_outcome(result_ty.as_ref(), results.first()),
            // An exhausted fuel pool is the budget being enforced, not a guest bug: report
            // it as the task being killed (usermode `--max-fuel` parity). Unreachable with
            // the u64::MAX pool above, but correct the moment a per-child cap is plumbed.
            Err(err)
                if matches!(
                    err.downcast_ref::<wasmtime::Trap>(),
                    Some(wasmtime::Trap::OutOfFuel)
                ) =>
            {
                KOutcome::Killed
            }
            Err(err) => KOutcome::Trapped(format!("{err:?}")),
        }
    });

    let rep = CHILDREN.with(|children| {
        let index = children.iter().position(Option::is_none);
        let index = match index {
            Some(index) => {
                children[index] = Some(ChildSlot::Running(drive));
                index
            }
            None => {
                children.push(Some(ChildSlot::Running(drive)));
                children.len() - 1
            }
        };
        index as u32
    });
    Ok(rep)
}

// -----------------------------------------------------------------------------------------
// Shell exec state (component / image tables) and metadata
// -----------------------------------------------------------------------------------------

/// One open component value: its bytes, plus the originating store entry when it is a
/// pristine baked-in component (which enables the host-AOT fast path in `compile` and the
/// baked metadata in `describe`). Algebra results (`compose`/`extend`/…) carry `entry =
/// None` and are compiled on-target.
struct KComponent {
    bytes: Vec<u8>,
    entry: Option<usize>,
}

/// One compiled image: the deserialized baked-in artifact.
struct KImage {
    component: Component,
}

/// The shell session's exec state.
#[derive(Default)]
pub struct ShellExec {
    components: Vec<Option<KComponent>>,
    images: Vec<Option<KImage>>,
}

impl ShellExec {
    fn insert_component(&mut self, value: KComponent) -> u32 {
        let index = self.components.iter().position(Option::is_none);
        let index = match index {
            Some(index) => {
                self.components[index] = Some(value);
                index
            }
            None => {
                self.components.push(Some(value));
                self.components.len() - 1
            }
        };
        index as u32
    }

    fn insert_image(&mut self, value: KImage) -> u32 {
        let index = self.images.iter().position(Option::is_none);
        let index = match index {
            Some(index) => {
                self.images[index] = Some(value);
                index
            }
            None => {
                self.images.push(Some(value));
                self.images.len() - 1
            }
        };
        index as u32
    }

    fn component(&self, rep: u32) -> Result<&KComponent> {
        self.components
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown component handle {rep}")))
    }

    fn take_component(&mut self, rep: u32) -> Result<KComponent> {
        self.components
            .get_mut(rep as usize)
            .and_then(Option::take)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown component handle {rep}")))
    }

    fn free_component(&mut self, rep: u32) {
        if let Some(slot) = self.components.get_mut(rep as usize) {
            *slot = None;
        }
    }

    fn image(&self, rep: u32) -> Result<&KImage> {
        self.images
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown image handle {rep}")))
    }

    fn free_image(&mut self, rep: u32) {
        if let Some(slot) = self.images.get_mut(rep as usize) {
            *slot = None;
        }
    }
}

/// Parse a store entry's metadata block (written by xtask; see store.rs for the format)
/// into the WIT-shaped `component-info`. Empty fields are spelled `-` in the image.
fn parse_metadata(metadata: &str) -> WitComponentInfo {
    fn field(text: &str) -> String {
        if text == "-" {
            String::new()
        } else {
            text.to_string()
        }
    }
    let mut info = WitComponentInfo {
        kind: WitComponentKind::Binary,
        imports: Vec::new(),
        exports: Vec::new(),
        args: Vec::new(),
    };
    for line in metadata.lines() {
        let line = line.trim();
        let Some((kind, rest)) = line.split_once(' ') else {
            continue;
        };
        match kind {
            "kind" => {
                if rest.trim() == "provider" {
                    info.kind = WitComponentKind::Provider;
                }
            }
            "import" => {
                let mut parts = rest.splitn(4, ' ');
                let (Some(required), Some(slot), Some(interface), Some(version)) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                info.imports.push(WitImportNeed {
                    slot: field(slot),
                    interface: field(interface),
                    version: field(version),
                    required: required == "required",
                });
            }
            "export" => {
                let mut parts = rest.splitn(3, ' ');
                let (Some(name), Some(interface), Some(version)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                info.exports.push(WitExportSlot {
                    name: field(name),
                    interface: field(interface),
                    version: field(version),
                });
            }
            "arg" => {
                let Some((name, ty)) = rest.split_once(' ') else {
                    continue;
                };
                info.args.push(WitArgSpec {
                    name: field(name),
                    ty: ty.trim().to_string(),
                });
            }
            _ => {}
        }
    }
    info
}

/// The clear refusal used by every algebra combinator when on-target codegen is off.
#[cfg(not(feature = "wasm-codegen"))]
fn unsupported(operation: &str) -> String {
    format!(
        "the bare-metal kernel does not implement `{operation}` yet: the component algebra \
         needs on-target codegen (the `wasm-codegen` feature); only programs baked into the \
         read-only store can be run as-is"
    )
}

/// Runs a two-operand algebra op (`compose`/`extend`) over component bytes and stores the
/// fused result as a new component handle (compiled on-target by `compile`).
#[cfg(feature = "wasm-codegen")]
fn alg_binary_op(
    store: &mut StoreContextMut<'_, KernelState>,
    a: Vec<u8>,
    b: Vec<u8>,
    op: impl Fn(
        &eo9_component::Component,
        &eo9_component::Component,
    ) -> core::result::Result<eo9_component::Component, eo9_component::ComposeError>,
) -> core::result::Result<Resource<AlgComponentRes>, WitComposeError> {
    let load = |bytes| {
        eo9_component::Component::load(bytes)
            .map_err(|err| WitComposeError::Internal(format!("operand is not a component: {err}")))
    };
    let a = load(a)?;
    let b = load(b)?;
    let fused = op(&a, &b).map_err(|err| WitComposeError::Internal(format!("{err}")))?;
    let rep = store
        .data_mut()
        .shell_exec()
        .map_err(|err| WitComposeError::Internal(format!("{err}")))?
        .insert_component(KComponent {
            bytes: fused.into_bytes(),
            entry: None,
        });
    Ok(Resource::new_own(rep))
}

/// Describes a (non-store) fused component by loading it with the eo9-component crate and
/// converting its `ComponentInfo` into the WIT record.
#[cfg(feature = "wasm-codegen")]
fn wit_info_from_eo9(bytes: &[u8]) -> Result<WitComponentInfo> {
    let component = eo9_component::Component::load(bytes.to_vec())
        .map_err(|err| wasmtime::Error::msg(format!("failed to describe component: {err}")))?;
    let info = component.describe();
    Ok(WitComponentInfo {
        kind: match info.kind {
            eo9_component::ComponentKind::Binary => WitComponentKind::Binary,
            eo9_component::ComponentKind::Provider => WitComponentKind::Provider,
        },
        imports: info
            .imports
            .into_iter()
            .map(|need| WitImportNeed {
                slot: need.slot,
                interface: need.interface,
                version: need.version,
                required: need.required,
            })
            .collect(),
        exports: info
            .exports
            .into_iter()
            .map(|slot| WitExportSlot {
                name: slot.name,
                interface: slot.interface,
                version: slot.version,
            })
            .collect(),
        args: info
            .args
            .into_iter()
            .map(|arg| WitArgSpec {
                name: arg.name,
                ty: arg.ty,
            })
            .collect(),
    })
}

/// Whether a fused component is a binary (vs a provider) — for the `compile` binary check.
#[cfg(feature = "wasm-codegen")]
fn fused_is_provider(bytes: &[u8]) -> bool {
    eo9_component::Component::load(bytes.to_vec())
        .map(|component| matches!(component.kind(), eo9_component::ComponentKind::Provider))
        .unwrap_or(false)
}

// -----------------------------------------------------------------------------------------
// State plumbing
// -----------------------------------------------------------------------------------------

impl KernelState {
    fn shell_exec(&mut self) -> Result<&mut ShellExec> {
        self.shell
            .as_mut()
            .map(|shell| &mut shell.exec)
            .ok_or_else(|| wasmtime::Error::msg("the exec capability was not granted to this task"))
    }

    fn shell_engine(&mut self) -> Result<Engine> {
        self.shell
            .as_mut()
            .map(|shell| shell.engine.clone())
            .ok_or_else(|| wasmtime::Error::msg("the exec capability was not granted to this task"))
    }

    fn shell_entries(&mut self) -> Result<&'static [super::store::StoreEntry]> {
        self.shell
            .as_mut()
            .map(|shell| shell.fs.entries())
            .ok_or_else(|| wasmtime::Error::msg("no store entries available to this task"))
    }
}

// -----------------------------------------------------------------------------------------
// Linker registration
// -----------------------------------------------------------------------------------------

/// Register the `eo9:exec` interfaces for the shell session.
pub fn add_exec(linker: &mut Linker<KernelState>) -> Result<()> {
    // ----- component-algebra --------------------------------------------------------------
    let mut algebra = linker.instance("eo9:exec/component-algebra@0.1.0")?;
    algebra.resource(
        "component",
        ResourceType::host::<AlgComponentRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            if let Ok(exec) = store.data_mut().shell_exec() {
                exec.free_component(rep);
            }
            Ok(())
        },
    )?;

    algebra.func_wrap(
        "load",
        |mut store: StoreContextMut<'_, KernelState>,
         (bytes,): (Vec<u8>,)|
         -> Result<(Result<Resource<AlgComponentRes>, WitLoadError>,)> {
            let entries = store.data_mut().shell_entries()?;
            let entry = entries
                .iter()
                .position(|entry| entry.component == bytes.as_slice());
            Ok((match entry {
                Some(entry) => {
                    let rep = store.data_mut().shell_exec()?.insert_component(KComponent {
                        bytes: entries[entry].component.to_vec(),
                        entry: Some(entry),
                    });
                    Ok(Resource::new_own(rep))
                }
                // With on-target codegen the kernel can also load components that are not in
                // the baked-in store (e.g. algebra results round-tripped through `save`),
                // validating them with the same `eo9-component` loader usermode uses.
                #[cfg(feature = "wasm-codegen")]
                None => match eo9_component::Component::load(bytes) {
                    Ok(component) => {
                        let rep = store.data_mut().shell_exec()?.insert_component(KComponent {
                            bytes: component.into_bytes(),
                            entry: None,
                        });
                        Ok(Resource::new_own(rep))
                    }
                    Err(err) => Err(WitLoadError::NotAnEo9Module(format!(
                        "not a loadable Eo9 component: {err}"
                    ))),
                },
                #[cfg(not(feature = "wasm-codegen"))]
                None => Err(WitLoadError::NotAnEo9Module(
                    "this component is not in the kernel's baked-in store; the bare-metal \
                     kernel cannot load arbitrary components without on-target codegen"
                        .to_string(),
                )),
            },))
        },
    )?;

    algebra.func_wrap(
        "save",
        |mut store: StoreContextMut<'_, KernelState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(Vec<u8>,)> {
            let bytes = store
                .data_mut()
                .shell_exec()?
                .component(component.rep())?
                .bytes
                .clone();
            Ok((bytes,))
        },
    )?;

    algebra.func_wrap(
        "describe",
        |mut store: StoreContextMut<'_, KernelState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(WitComponentInfo,)> {
            let entries = store.data_mut().shell_entries()?;
            let kc = store.data_mut().shell_exec()?.component(component.rep())?;
            match kc.entry {
                // Pristine store entry: replay the metadata xtask precomputed.
                Some(entry) => Ok((parse_metadata(entries[entry].metadata),)),
                // Algebra result: describe the fused bytes with the eo9-component loader.
                #[cfg(feature = "wasm-codegen")]
                None => {
                    let info = wit_info_from_eo9(&kc.bytes)?;
                    Ok((info,))
                }
                #[cfg(not(feature = "wasm-codegen"))]
                None => Err(wasmtime::Error::msg(
                    "cannot describe a composed component without on-target codegen",
                )),
            }
        },
    )?;

    algebra.func_wrap(
        "compose",
        |mut store: StoreContextMut<'_, KernelState>,
         (provider, consumer): (Resource<AlgComponentRes>, Resource<AlgComponentRes>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitComposeError>,)> {
            let (pb, cb) = {
                let exec = store.data_mut().shell_exec()?;
                (
                    exec.take_component(provider.rep())?.bytes,
                    exec.take_component(consumer.rep())?.bytes,
                )
            };
            #[cfg(feature = "wasm-codegen")]
            {
                Ok((alg_binary_op(&mut store, pb, cb, eo9_component::compose),))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (pb, cb);
                Ok((Err(WitComposeError::Internal(unsupported("$ (compose)"))),))
            }
        },
    )?;

    algebra.func_wrap(
        "extend",
        |mut store: StoreContextMut<'_, KernelState>,
         (base, layer): (Resource<AlgComponentRes>, Resource<AlgComponentRes>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitComposeError>,)> {
            let (bb, lb) = {
                let exec = store.data_mut().shell_exec()?;
                (
                    exec.take_component(base.rep())?.bytes,
                    exec.take_component(layer.rep())?.bytes,
                )
            };
            #[cfg(feature = "wasm-codegen")]
            {
                Ok((alg_binary_op(&mut store, bb, lb, eo9_component::extend),))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (bb, lb);
                Ok((Err(WitComposeError::Internal(unsupported("& (extend)"))),))
            }
        },
    )?;

    algebra.func_wrap(
        "restrict",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, allow): (Resource<AlgComponentRes>, Vec<WitInterfaceRef>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitRestrictError>,)> {
            let bytes = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?
                .bytes;
            #[cfg(feature = "wasm-codegen")]
            {
                let allow: Vec<eo9_component::InterfaceRef> = allow
                    .into_iter()
                    .map(|r| eo9_component::InterfaceRef {
                        interface: r.interface,
                        version: r.version,
                    })
                    .collect();
                let result =
                    (|| -> core::result::Result<Resource<AlgComponentRes>, WitRestrictError> {
                        let c = eo9_component::Component::load(bytes)
                            .map_err(|e| WitRestrictError::Internal(format!("{e}")))?;
                        let restricted = eo9_component::restrict(&c, &allow)
                            .map_err(|e| WitRestrictError::Internal(format!("{e}")))?;
                        let rep = store
                            .data_mut()
                            .shell_exec()
                            .map_err(|e| WitRestrictError::Internal(format!("{e}")))?
                            .insert_component(KComponent {
                                bytes: restricted.into_bytes(),
                                entry: None,
                            });
                        Ok(Resource::new_own(rep))
                    })();
                Ok((result,))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (bytes, allow);
                Ok((Err(WitRestrictError::Internal(unsupported(
                    "only (restrict)",
                ))),))
            }
        },
    )?;

    algebra.func_wrap(
        "rename",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, old, new): (Resource<AlgComponentRes>, String, String)|
         -> Result<(Result<Resource<AlgComponentRes>, WitRenameError>,)> {
            let bytes = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?
                .bytes;
            #[cfg(feature = "wasm-codegen")]
            {
                let result =
                    (|| -> core::result::Result<Resource<AlgComponentRes>, WitRenameError> {
                        let c = eo9_component::Component::load(bytes)
                            .map_err(|e| WitRenameError::Internal(format!("{e}")))?;
                        let renamed = eo9_component::rename(&c, &old, &new)
                            .map_err(|e| WitRenameError::Internal(format!("{e}")))?;
                        let rep = store
                            .data_mut()
                            .shell_exec()
                            .map_err(|e| WitRenameError::Internal(format!("{e}")))?
                            .insert_component(KComponent {
                                bytes: renamed.into_bytes(),
                                entry: None,
                            });
                        Ok(Resource::new_own(rep))
                    })();
                Ok((result,))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (bytes, old, new);
                Ok((Err(WitRenameError::Internal(unsupported("rename"))),))
            }
        },
    )?;

    algebra.func_wrap(
        "configure",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, args): (Resource<AlgComponentRes>, Vec<WitNamedArg>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitConfigureError>,)> {
            let bytes = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?
                .bytes;
            #[cfg(feature = "wasm-codegen")]
            {
                let pairs: Vec<(String, String)> =
                    args.into_iter().map(|a| (a.name, a.value)).collect();
                let result =
                    (|| -> core::result::Result<Resource<AlgComponentRes>, WitConfigureError> {
                        let c = eo9_component::Component::load(bytes)
                            .map_err(|e| WitConfigureError::Internal(format!("{e}")))?;
                        let configured = eo9_component::configure(&c, &pairs)
                            .map_err(|e| WitConfigureError::Internal(format!("{e}")))?;
                        let rep = store
                            .data_mut()
                            .shell_exec()
                            .map_err(|e| WitConfigureError::Internal(format!("{e}")))?
                            .insert_component(KComponent {
                                bytes: configured.into_bytes(),
                                entry: None,
                            });
                        Ok(Resource::new_own(rep))
                    })();
                Ok((result,))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (bytes, args);
                Ok((Err(WitConfigureError::Internal(unsupported("configure"))),))
            }
        },
    )?;

    // The record-only args interface carries no functions or resources, but guests import
    // it as an instance; make sure the linker has a definition for it.
    let _ = linker.instance("eo9:exec/args@0.1.0")?;

    // ----- images + compile ---------------------------------------------------------------
    let mut images = linker.instance("eo9:exec/images@0.1.0")?;
    images.resource(
        "image",
        ResourceType::host::<ExecImageRes>(),
        |mut store: StoreContextMut<'_, KernelState>, rep| {
            if let Ok(exec) = store.data_mut().shell_exec() {
                exec.free_image(rep);
            }
            Ok(())
        },
    )?;

    let mut compile = linker.instance("eo9:exec/compile@0.1.0")?;
    compile.func_wrap(
        "compile",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, _opts): (Resource<AlgComponentRes>, WitCompileOpts)|
         -> Result<(Result<Resource<ExecImageRes>, WitCompileError>,)> {
            let entries = store.data_mut().shell_entries()?;
            let engine = store.data_mut().shell_engine()?;
            let component = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?;

            let image = match component.entry {
                // Pristine store entry: deserialize the baked-in host-AOT artifact (the
                // fast path / cache; no codegen needed).
                Some(entry) => {
                    let entry = &entries[entry];
                    if parse_metadata(entry.metadata).kind == WitComponentKind::Provider {
                        return Ok((Err(WitCompileError::NotABinary),));
                    }
                    // SAFETY: the artifact comes from the store image produced by `cargo
                    // xtask build-kernel` with the same wasmtime version and engine config.
                    unsafe { Component::deserialize(&engine, entry.artifact) }.map_err(|err| {
                        WitCompileError::Codegen(format!(
                            "the baked-in artifact for this component failed to load: {err:?}"
                        ))
                    })
                }
                // Algebra result (fused, not in the store): compile it on-target with
                // Cranelift, exactly like the codegen demo (plan/12 Decision 29).
                #[cfg(feature = "wasm-codegen")]
                None => {
                    if fused_is_provider(&component.bytes) {
                        return Ok((Err(WitCompileError::NotABinary),));
                    }
                    Component::new(&engine, &component.bytes).map_err(|err| {
                        WitCompileError::Codegen(format!("on-target compilation failed: {err:?}"))
                    })
                }
                #[cfg(not(feature = "wasm-codegen"))]
                None => Err(WitCompileError::Codegen(
                    "composed components require on-target codegen".to_string(),
                )),
            };

            Ok((match image {
                Ok(image) => {
                    let rep = store
                        .data_mut()
                        .shell_exec()?
                        .insert_image(KImage { component: image });
                    Ok(Resource::new_own(rep))
                }
                Err(err) => Err(err),
            },))
        },
    )?;

    // ----- task -----------------------------------------------------------------------------
    let mut task = linker.instance("eo9:exec/task@0.1.0")?;
    task.resource(
        "task",
        ResourceType::host::<ChildTaskRes>(),
        |_store: StoreContextMut<'_, KernelState>, rep| {
            // Dropping the handle kills the child: its drive future (and with it the
            // child's store and any in-flight work) is dropped.
            CHILDREN.with(|children| {
                if let Some(slot) = children.get_mut(rep as usize) {
                    *slot = None;
                }
            });
            Ok(())
        },
    )?;

    task.func_wrap(
        "spawn",
        |mut store: StoreContextMut<'_, KernelState>,
         (image, args, limits): (Resource<ExecImageRes>, Vec<WitNamedArg>, WitSpawnLimits)|
         -> Result<(Result<Resource<ChildTaskRes>, WitSpawnError>,)> {
            let engine = store.data_mut().shell_engine()?;
            let entries = store.data_mut().shell_entries()?;
            let component = {
                let exec = store.data_mut().shell_exec()?;
                exec.image(image.rep())?.component.clone()
            };
            Ok((
                match spawn_child(&engine, entries, &component, &args, limits.max_memory) {
                    Ok(rep) => Ok(Resource::new_own(rep)),
                    Err(err) => Err(err),
                },
            ))
        },
    )?;

    task.func_wrap(
        "resume",
        |_store: StoreContextMut<'_, KernelState>,
         (child, _fuel): (Resource<ChildTaskRes>, u64)|
         -> Result<(WitResumeOutcome,)> {
            // Same limitation as usermode (E5): children execute on the shell's own drive
            // loop; report a finished child, otherwise refuse loudly.
            let outcome = CHILDREN.with(|children| match children.get(child.rep() as usize) {
                Some(Some(ChildSlot::Done(outcome))) => Some(outcome.clone()),
                _ => None,
            });
            match outcome {
                Some(outcome) => Ok((WitResumeOutcome::Done(wit_outcome(&outcome)),)),
                None => Err(wasmtime::Error::msg(
                    "eo9:exec/task.resume is not supported by this kernel yet: child tasks \
                     run on the shell's drive loop; use wait",
                )),
            }
        },
    )?;

    task.func_wrap_concurrent(
        "wait",
        |_accessor: &Accessor<KernelState>,
         (child,): (Resource<ChildTaskRes>,)|
         -> ConcurrentFuture<'_, (WitProgramOutcome,)> {
            Box::pin(async move {
                let rep = child.rep() as usize;
                let outcome = core::future::poll_fn(move |cx| {
                    let observed = CHILDREN.with(|children| match children.get(rep) {
                        Some(Some(ChildSlot::Done(outcome))) => Some(Ok(outcome.clone())),
                        Some(Some(ChildSlot::Running(_) | ChildSlot::Polling)) => None,
                        _ => Some(Err(wasmtime::Error::msg(format!(
                            "unknown task handle {rep}"
                        )))),
                    });
                    match observed {
                        Some(result) => Poll::Ready(result),
                        None => {
                            // The child makes progress on the shell's drive loop between
                            // polls of the shell; stay runnable so that loop keeps turning.
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                    }
                })
                .await?;
                Ok((wit_outcome(&outcome),))
            })
        },
    )?;

    task.func_wrap_concurrent(
        "runnable",
        |_accessor: &Accessor<KernelState>,
         (child,): (Resource<ChildTaskRes>,)|
         -> ConcurrentFuture<'_, ()> {
            Box::pin(async move {
                let rep = child.rep() as usize;
                core::future::poll_fn(move |cx| {
                    let done = CHILDREN.with(|children| {
                        matches!(children.get(rep), Some(Some(ChildSlot::Done(_))))
                    });
                    if done {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                Ok(())
            })
        },
    )?;

    task.func_wrap_concurrent(
        "kill",
        |_accessor: &Accessor<KernelState>,
         (child,): (Resource<ChildTaskRes>,)|
         -> ConcurrentFuture<'_, (WitProgramOutcome,)> {
            Box::pin(async move {
                let rep = child.rep() as usize;
                let outcome = CHILDREN.with(|children| match children.get_mut(rep) {
                    Some(slot) => match slot {
                        Some(ChildSlot::Done(outcome)) => Ok(outcome.clone()),
                        Some(ChildSlot::Running(_) | ChildSlot::Polling) => {
                            // Dropping the drive future drops the child's store, guest
                            // state, and in-flight work (SPEC "Kill and linearity") — for a
                            // child currently checked out by `drive_children`, that drop
                            // happens when the poll returns and sees the slot is no longer
                            // `Polling`.
                            *slot = Some(ChildSlot::Done(KOutcome::Killed));
                            Ok(KOutcome::Killed)
                        }
                        None => Err(wasmtime::Error::msg(format!("unknown task handle {rep}"))),
                    },
                    None => Err(wasmtime::Error::msg(format!("unknown task handle {rep}"))),
                })?;
                Ok((wit_outcome(&outcome),))
            })
        },
    )?;

    Ok(())
}

/// Translate a linker "missing import" instantiation error into the capability story
/// instead of leaking the raw error text (user-study finding). Children now inherit the
/// session's fs/io/exec surface, so the remaining genuinely-unavailable capabilities on
/// bare metal are the ones the kernel has no provider for at all.
fn missing_capability(text: &str) -> Option<String> {
    let capability = if text.contains("eo9:net/") {
        "the network, which the bare-metal session does not provide"
    } else if text.contains("eo9:disk/") {
        "raw disk access, which the bare-metal session does not provide"
    } else if text.contains("eo9:pci/") {
        "PCI device access, which the bare-metal session does not provide"
    } else {
        return None;
    };
    Some(alloc::format!(
        "the program requires {capability} (refused at instantiation)"
    ))
}
