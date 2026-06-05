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
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
/// Hard cap on concurrently-live (running or checked-out) children across the machine, so a
/// fork-bomb-style shell can't exhaust memory/drive-loop time by spawning without bound. A
/// spawn past the cap is refused with a clear error; finished children free a slot. Generous
/// enough for normal nesting (plan/12 D38 item 4).
pub const MAX_LIVE_CHILDREN: usize = 64;

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
pub(super) struct WitNamedArg {
    pub(super) name: String,
    pub(super) value: String,
}

/// `eo9:exec/task.component-arg`: one component-typed `main` argument. The handle is the
/// spawner's; the host takes the underlying component value out of the spawner's table
/// and re-mints it in the child's (ownership transfer — the detach precedent).
#[derive(ComponentType, Lift, Lower)]
#[component(record)]
pub(super) struct WitComponentArg {
    pub(super) name: String,
    pub(super) value: Resource<AlgComponentRes>,
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
pub(super) fn render_outcome(result_ty: Option<&Type>, val: Option<&Val>) -> KOutcome {
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
/// registry (and the service registry in `svc.rs`) `Sync`; with one core and the lock
/// never held across a yield it cannot actually contend.
pub(super) struct KLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for KLock<T> {}

impl<T> KLock<T> {
    pub(super) const fn new(value: T) -> Self {
        KLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub(super) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
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

/// Parent rep per child, parallel to [`CHILDREN`] by index (`None` = spawned by the
/// top-level shell, which is not itself in the registry). Lets `kill` cascade to descendants
/// so killing a foreground nested eosh takes its children/grandchildren down with it rather
/// than orphaning them on the drive loop (plan/12 D38 item 3). Kept as a parallel vector so
/// the `ChildSlot` enum and the many `CHILDREN.with` sites are untouched.
static PARENTS: KLock<Vec<Option<u32>>> = KLock::new(Vec::new());

/// The rep currently being polled by [`drive_children`], so a nested spawn during that poll
/// records its parent. `u32::MAX` means "no current child" (top-level shell spawns). Single
/// core, set/cleared around each unlocked child poll, never nested (children do not call
/// `drive_children`).
static CURRENT_PARENT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Whether the *root* program's own `task.wait` consumes Ctrl-C. True when the root is
/// the console itself (`boot_to_eosh`, headless runs — today's behavior); false when the
/// root is the boot supervisor (`boot_to_init`), whose wait on the console must NOT eat
/// the interrupt key — Ctrl-C belongs to the console's foreground job, exactly as it
/// does today, and a Ctrl-C at the bare prompt stays a no-op instead of killing the
/// console. Child waiters (the console waiting on its foreground job, a nested eosh)
/// always consume, which `CURRENT_PARENT != MAX` identifies during their polls.
static ROOT_CONSUMES_CTRL_C: AtomicBool = AtomicBool::new(true);

/// Set whether the root program's waits consume Ctrl-C (see [`ROOT_CONSUMES_CTRL_C`]).
pub fn set_root_consumes_ctrl_c(consumes: bool) {
    ROOT_CONSUMES_CTRL_C.store(consumes, Ordering::Release);
}

/// Kill task `rep` and all its descendants (transitively, via [`PARENTS`]). Running/checked-out
/// slots become `Done(Killed)` (dropping a checked-out drive future happens when its poll
/// returns and sees the slot is no longer `Polling`); already-finished slots keep their
/// outcome. Returns the target's resulting outcome (`Killed` if it was running, else its
/// existing outcome, or `Killed` if the handle is unknown). Used by `task.kill` and the
/// Ctrl-C path.
fn kill_task_tree(rep: usize) -> KOutcome {
    // Kill descendants first (depth doesn't matter — the registry is flat; we just need to
    // catch every transitive child). Iterate to a fixed point so grandchildren are caught.
    loop {
        let killed_any = CHILDREN.with(|children| {
            PARENTS.with(|parents| {
                let mut killed = false;
                for i in 0..children.len() {
                    let is_descendant = is_descendant_of(parents, i, rep);
                    if !is_descendant {
                        continue;
                    }
                    if let Some(slot @ (ChildSlot::Running(_) | ChildSlot::Polling)) =
                        children.get_mut(i).and_then(Option::as_mut)
                    {
                        *slot = ChildSlot::Done(KOutcome::Killed);
                        killed = true;
                    }
                }
                killed
            })
        });
        if !killed_any {
            break;
        }
    }
    // Then the target itself.
    CHILDREN.with(|children| match children.get_mut(rep) {
        Some(slot) => match slot {
            Some(ChildSlot::Done(outcome)) => outcome.clone(),
            Some(ChildSlot::Running(_) | ChildSlot::Polling) => {
                *slot = Some(ChildSlot::Done(KOutcome::Killed));
                KOutcome::Killed
            }
            None => KOutcome::Killed,
        },
        None => KOutcome::Killed,
    })
}

/// Whether child index `i` is a (transitive) descendant of `ancestor`, walking [`PARENTS`].
/// Bounded by the registry length to be safe against any accidental cycle.
fn is_descendant_of(parents: &[Option<u32>], i: usize, ancestor: usize) -> bool {
    let mut cur = i;
    for _ in 0..parents.len() {
        match parents.get(cur).copied().flatten() {
            Some(p) if p as usize == ancestor => return true,
            Some(p) => cur = p as usize,
            None => return false,
        }
    }
    false
}

/// Waker used when polling one child drive. wasmtime re-polls the sub-futures whose waker
/// was rung; in addition, this records *whether* the waker was rung at all during the poll,
/// which is how [`drive_children`] tells a child that yielded on fuel (wants an immediate
/// re-poll — runnable) from one parked on a host future like `read-line`/`time.sleep` (which
/// instead registers the executor's idle waker and is re-driven after a `wfi`).
struct ChildWaker {
    rung: AtomicBool,
}

impl alloc::task::Wake for ChildWaker {
    fn wake(self: Arc<Self>) {
        self.rung.store(true, Ordering::Release);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.rung.store(true, Ordering::Release);
    }
}

/// What [`drive_children`] observed across one pass, so the drive loop can decide whether to
/// keep polling at full speed or idle the core in `wfi`.
#[derive(Clone, Copy, Default)]
pub struct DriveStatus {
    /// At least one child yielded on fuel and wants to run again immediately — the loop
    /// should re-poll without a `wfi` so a compute-bound child runs at full speed.
    pub any_runnable: bool,
    /// At least one child is still running (runnable or parked on a host future) — the loop
    /// keeps a short idle backstop so a child keeps getting turns.
    pub any_running: bool,
}

/// Reset the registry (called once when a shell session starts, so reps are dense and a
/// stale handle from a previous session cannot alias a new child).
pub fn reset_children() {
    CHILDREN.with(|children| children.clear());
    PARENTS.with(|parents| parents.clear());
    CURRENT_PARENT.store(u32::MAX, Ordering::Release);
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
pub fn drive_children() -> DriveStatus {
    let mut status = DriveStatus::default();
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
        // most one quantum of guest code before yielding back here. The flag-waker records
        // whether the poll asked to be re-run (a fuel yield rings it; a host-future park does
        // not), which tells the drive loop whether this child is runnable or merely waiting.
        let child_waker = Arc::new(ChildWaker {
            rung: AtomicBool::new(false),
        });
        let waker = Waker::from(child_waker.clone());
        let mut cx = Context::from_waker(&waker);
        // Record which child is running so a nested spawn during this poll records its parent
        // (for kill-cascade). Restored after the poll; never nested (children don't drive).
        CURRENT_PARENT.store(index as u32, Ordering::Release);
        let polled = drive.as_mut().poll(&mut cx);
        CURRENT_PARENT.store(u32::MAX, Ordering::Release);

        let still_running = CHILDREN.with(|children| {
            let slot = &mut children[index];
            match slot {
                // Normal case: still checked out to us — check it back in.
                Some(ChildSlot::Polling) => match polled {
                    Poll::Ready(outcome) => {
                        *slot = Some(ChildSlot::Done(outcome));
                        false
                    }
                    Poll::Pending => {
                        *slot = Some(ChildSlot::Running(drive));
                        true
                    }
                },
                // The child was killed (slot now Done) or its handle was dropped (slot now
                // None) while we were polling it: keep that state; dropping the checked-out
                // drive future here releases the child's store and any in-flight work.
                _ => false,
            }
        });
        if still_running {
            status.any_running = true;
            if child_waker.rung.load(Ordering::Acquire) {
                status.any_runnable = true;
            }
        }
        index += 1;
    }
    status
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
        spawn_child(&engine, entries, &component, &args, Vec::new(), None, 0).map_err(|err| {
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
/// `parse_args` rule: every declared parameter exactly once, no unknown arguments — except
/// that a *final* `list<…>` parameter left unsupplied defaults to the empty list, the
/// variadic-tail convention shared with the usermode binder).
pub(super) fn bind_args(
    signature: &wasmtime::component::types::ComponentFunc,
    args: &[WitNamedArg],
    component_vals: &mut alloc::collections::BTreeMap<String, Val>,
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
    for name in component_vals.keys() {
        if !params.iter().any(|(param, _)| param == name) {
            return Err(format!("unknown component argument `{name}`"));
        }
    }
    let mut vals = Vec::with_capacity(params.len());
    for (index, (name, ty)) in params.iter().enumerate() {
        // A component-typed parameter binds from the spawn's component arguments — an
        // owned handle minted in the child, never WAVE text.
        if matches!(ty, Type::Own(_)) {
            if args.iter().any(|arg| arg.name == *name) {
                return Err(format!(
                    "parameter `{name}` is component-typed; it takes a program value, not text"
                ));
            }
            match component_vals.remove(name) {
                Some(val) => {
                    vals.push(val);
                    continue;
                }
                None => {
                    return Err(format!(
                        "missing component argument `{name}` (pass a program expression)"
                    ));
                }
            }
        }
        let matching: Vec<&WitNamedArg> = args.iter().filter(|arg| arg.name == *name).collect();
        let arg = match matching.as_slice() {
            // Variadic tail: a missing final `list<…>` parameter is the empty list, so
            // bare `ls` and friends run without an explicit `paths` argument.
            [] if index + 1 == params.len() && matches!(ty, Type::List(_)) => {
                vals.push(Val::List(Vec::new()));
                continue;
            }
            // An unsupplied `option<…>` parameter binds to `none` — usermode-runtime
            // parity (its binder and the headless runner both do this), so a spawner
            // that passes no arguments (init starting its console) works against
            // option-typed signatures exactly as it does in usermode.
            [] if matches!(ty, Type::Option(_)) => {
                vals.push(Val::Option(None));
                continue;
            }
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
/// The one spawn `Linker`, built lazily on first use and reused for every spawn (the
/// host-function set is boot-constant: the only conditional registration is `eo9:pci`,
/// gated by the boot command line's `pci` token, which never changes after boot). The
/// stored bool is that grant bit — re-checked on every reuse as the grant-shape guard
/// (security review of the spawn-cache design): a linker built under one grant shape can
/// never serve a spawn under another. Per-spawn capability state (svc generations, the
/// session fs, buffer tables) lives in the per-spawn `Store`, not the linker, so reuse
/// shares no authority between spawns.
static SPAWN_LINKER: KLock<Option<(bool, Arc<Linker<KernelState>>)>> = KLock::new(None);

fn spawn_linker(engine: &Engine) -> Result<Arc<Linker<KernelState>>, wasmtime::Error> {
    let granted = super::pci_provider::granted();
    if let Some(linker) = SPAWN_LINKER.with(|slot| match slot {
        Some((shape, linker)) if *shape == granted => Some(linker.clone()),
        _ => None,
    }) {
        return Ok(linker);
    }
    let mut linker: Linker<KernelState> = Linker::new(engine);
    providers::add_providers(&mut linker)?;
    super::shellfs::add_buffers(&mut linker)?;
    super::shellfs::add_fs(&mut linker)?;
    add_exec(&mut linker)?;
    // PCI is never granted by default (bus mastering means DMA): only when the boot's
    // command line carried the `pci` token — and even then the loader rule applies, so
    // only a child that imports `eo9:pci/pci` actually links it.
    if granted {
        super::pci_provider::add_pci(&mut linker)?;
    }
    let linker = Arc::new(linker);
    SPAWN_LINKER.with(|slot| *slot = Some((granted, linker.clone())));
    Ok(linker)
}

fn spawn_child(
    engine: &Engine,
    entries: &'static [super::store::StoreEntry],
    component: &Component,
    args: &[WitNamedArg],
    components: Vec<(String, KComponent)>,
    max_memory: Option<u64>,
    svc_generations: u32,
) -> Result<u32, WitSpawnError> {
    let internal = |err: wasmtime::Error| {
        let text = format!("{err:?}");
        WitSpawnError::Internal(match missing_capability(&text) {
            Some(friendly) => friendly,
            None => text,
        })
    };

    // Refuse before doing any work if the machine is already at the live-children cap, so a
    // runaway shell can't exhaust resources by spawning without bound (plan/12 D38 item 4).
    let live = CHILDREN.with(|children| {
        children
            .iter()
            .filter(|slot| matches!(slot, Some(ChildSlot::Running(_) | ChildSlot::Polling)))
            .count()
    });
    if live >= MAX_LIVE_CHILDREN {
        return Err(WitSpawnError::Internal(format!(
            "spawn refused: the live-task cap of {MAX_LIVE_CHILDREN} is reached \
             (a task must finish or be killed before another can start)"
        )));
    }

    #[cfg(feature = "spawn-trace")]
    let __trace_linker = crate::timer::uptime_us();
    let linker = spawn_linker(engine).map_err(internal)?;
    #[cfg(feature = "spawn-trace")]
    spawn_trace::add_since(spawn_trace::LINKER, __trace_linker);

    #[cfg(feature = "spawn-trace")]
    let __trace_state = crate::timer::uptime_us();
    let mut state = KernelState::new();
    // The svc capability reaches exactly the configured number of generations down
    // (owner ruling B, mirroring the usermode generation count): init holds 2, so its
    // console holds 1, and the console's children hold 0 — never a default grant.
    state.svc_generations = svc_generations;
    state.shell = Some(Box::new(super::shell::ShellState {
        fs: super::shellfs::ShellFs::new(entries, super::shell::session_manifest(entries)),
        buffers: super::shellfs::BufferTable::default(),
        exec: ShellExec::default(),
        engine: engine.clone(),
    }));
    #[cfg(feature = "spawn-trace")]
    spawn_trace::add_since(spawn_trace::STATE, __trace_state);
    #[cfg(feature = "spawn-trace")]
    let __trace_store = crate::timer::uptime_us();
    let mut store = Store::new(engine, state);
    if let Some(max_memory) = max_memory {
        store.data_mut().set_max_memory(max_memory);
        store.limiter(|state| state.limiter());
    }
    #[cfg(feature = "spawn-trace")]
    spawn_trace::add_since(spawn_trace::STORE, __trace_store);

    // Instantiation runs on a small bounded fuel budget paid by the spawner (usermode
    // `SPAWN_FUEL` parity): any start-time code either finishes within it or the spawn
    // fails — never an unbounded burn. It must also not depend on external completions;
    // drive it with a bounded poll loop, as usermode `spawn` does.
    store.set_fuel(SPAWN_FUEL).map_err(internal)?;
    #[cfg(feature = "spawn-trace")]
    let __trace_inst = crate::timer::uptime_us();
    let instance = {
        let instantiate = linker.instantiate_async(&mut store, component);
        let mut instantiate = core::pin::pin!(instantiate);
        let waker = Waker::from(Arc::new(ChildWaker {
            rung: AtomicBool::new(false),
        }));
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
    #[cfg(feature = "spawn-trace")]
    spawn_trace::add_since(spawn_trace::INSTANTIATE, __trace_inst);

    // Apply any compose-time configuration baked into the artifact (plan/03 D23): a
    // configured composition exports `eo9:rt/configured`, whose parameterless `bind`
    // runs every nested provider's `configure` with its baked constants. Executor
    // contract: once, after instantiation, before the first entry. It shares the
    // bounded spawn budget — configuration binds constants and must not block. A
    // configuration the provider rejects comes back as `bind`'s typed error (never a
    // trap) and refuses the spawn.
    #[cfg(feature = "spawn-trace")]
    let __trace_bind = crate::timer::uptime_us();
    if let Some(bind) = super::bind_entrypoint(&instance, &mut store) {
        let mut bind_results =
            alloc::vec![Val::Bool(false); super::bind_result_slots(&bind, &store)];
        {
            let call = bind.call_async(&mut store, &[], &mut bind_results);
            let mut call = core::pin::pin!(call);
            let waker = Waker::from(Arc::new(ChildWaker {
                rung: AtomicBool::new(false),
            }));
            let mut cx = Context::from_waker(&waker);
            let mut result = None;
            for _ in 0..4096 {
                match call.as_mut().poll(&mut cx) {
                    Poll::Ready(r) => {
                        result = Some(r);
                        break;
                    }
                    Poll::Pending => continue,
                }
            }
            result
                .ok_or_else(|| {
                    WitSpawnError::Internal(
                        "compose-time configuration (`bind`) unexpectedly suspended".to_string(),
                    )
                })?
                .map_err(|err| {
                    if matches!(
                        err.downcast_ref::<wasmtime::Trap>(),
                        Some(wasmtime::Trap::OutOfFuel)
                    ) {
                        WitSpawnError::Internal(format!(
                            "compose-time configuration exceeded the spawn fuel budget \
                             ({SPAWN_FUEL} fuel): `configure` must bind constants, not run \
                             unbounded code"
                        ))
                    } else {
                        WitSpawnError::Internal(format!(
                            "compose-time configuration (`bind`) failed: {err:#}"
                        ))
                    }
                })?;
        }
        if let Some(refused) = super::configuration_refused(&bind_results) {
            return Err(WitSpawnError::Internal(format!(
                "compose-time configuration refused: {refused}"
            )));
        }
    }
    #[cfg(feature = "spawn-trace")]
    spawn_trace::add_since(spawn_trace::BIND, __trace_bind);

    // Normal fuel regime for the child's life (usermode parity): an effectively-infinite
    // pool sliced by the fixed yield quantum, so every poll of the child runs at most
    // FUEL_QUANTUM units and then yields back to the drive loop — that slicing is what
    // keeps a compute-bound child from monopolizing the machine.
    store.set_fuel(u64::MAX).map_err(internal)?;
    store
        .fuel_async_yield_interval(Some(FUEL_QUANTUM))
        .map_err(internal)?;

    #[cfg(feature = "spawn-trace")]
    let __trace_args = crate::timer::uptime_us();
    let main = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| WitSpawnError::Internal("component does not export `main`".to_string()))?;
    let signature = main.ty(&store);
    // Component-typed arguments: each value transfers into the *child's* exec table and
    // binds as an owned handle of the child's imported component-algebra resource — the
    // child receives a live component value, provenance intact, never bytes.
    let mut component_vals = alloc::collections::BTreeMap::new();
    for (name, value) in components {
        let rep = store
            .data_mut()
            .shell_exec()
            .map_err(internal)?
            .insert_component(value);
        let resource = Resource::<AlgComponentRes>::new_own(rep);
        let any = resource
            .try_into_resource_any(&mut store)
            .map_err(internal)?;
        component_vals.insert(name, Val::Resource(any));
    }
    let params =
        bind_args(&signature, args, &mut component_vals).map_err(WitSpawnError::BadArguments)?;
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
            Err(err) => KOutcome::Trapped(match store.data().panic_message.as_deref() {
                // The guest reported its panic message through eo9:rt/diagnostics just
                // before trapping — put it in front of the raw trap text.
                Some(message) => format!("guest panicked: {message} — {err:?}"),
                None => format!("{err:?}"),
            }),
        }
    });

    let parent = match CURRENT_PARENT.load(Ordering::Acquire) {
        u32::MAX => None,
        rep => Some(rep),
    };
    let rep = CHILDREN.with(|children| {
        let index = children.iter().position(Option::is_none);
        match index {
            Some(index) => {
                children[index] = Some(ChildSlot::Running(drive));
                index
            }
            None => {
                children.push(Some(ChildSlot::Running(drive)));
                children.len() - 1
            }
        }
    });
    // Keep PARENTS index-aligned with CHILDREN (grow to cover `rep`).
    PARENTS.with(|parents| {
        if parents.len() <= rep {
            parents.resize(rep + 1, None);
        }
        parents[rep] = parent;
    });
    #[cfg(feature = "spawn-trace")]
    {
        spawn_trace::add_since(spawn_trace::ARGS, __trace_args);
        spawn_trace::dump_and_reset();
    }
    Ok(rep as u32)
}

// -----------------------------------------------------------------------------------------
// Shell exec state (component / image tables) and metadata
// -----------------------------------------------------------------------------------------

/// One open component value: its bytes, plus the originating store entry when it is a
/// pristine baked-in component (which enables the host-AOT fast path in `compile` and the
/// baked metadata in `describe`). Algebra results (`compose`/`extend`/…) carry `entry =
/// None` and are compiled on-target.
pub(super) struct KComponent {
    pub(super) bytes: Vec<u8>,
    pub(super) entry: Option<usize>,
    /// The component's *semantic identity*: blake3 over its fusion graph (owner design,
    /// plan/12 entry 73). A leaf (loaded component) hashes its bytes; an interior node
    /// hashes (op tag ‖ ordered child hashes ‖ canonicalized args). Two prompt lines that
    /// fuse the same graph — regardless of spelling, whitespace, or binding names — get
    /// the same hash, and the fused-bytes and compiled-artifact session caches key on it,
    /// so a repeat skips re-fusion, re-extraction, and recompilation entirely.
    pub(super) graph_hash: [u8; 32],
}

/// One compiled image: the deserialized baked-in artifact.
struct KImage {
    component: Component,
}

/// Most fused compositions a session keeps compiled at once. Each cached artifact holds
/// its published JIT code pages, so the cache is small and FIFO-evicted; eight covers an
/// interactive session's working set (the same handful of pipelines re-run repeatedly)
/// at a bounded memory cost.
#[cfg(feature = "wasm-codegen")]
const COMPILE_CACHE_ENTRIES: usize = 8;

/// Spawn-path phase tracing (measurement-only; `spawn-trace` feature). Accumulates
/// per-phase microseconds across the host calls a single prompt-line spawn makes, and
/// prints one summary line when the spawn completes. Phases overlap nothing: each is a
/// disjoint slice of kernel-side work; the residual vs. the externally measured
/// echo-to-first-line total is guest-side (eosh parse/resolve) plus fs reads.
#[cfg(feature = "spawn-trace")]
pub(super) mod spawn_trace {
    use super::KLock;

    pub const FS_READ: usize = 0;
    pub const ALG_LOAD: usize = 1;
    pub const ALG_OP: usize = 2;
    pub const EXEC_BYTES: usize = 3;
    pub const HASH_LOOKUP: usize = 4;
    pub const LINKER: usize = 5;
    pub const STATE: usize = 6;
    pub const STORE: usize = 7;
    pub const INSTANTIATE: usize = 8;
    pub const BIND: usize = 9;
    pub const ARGS: usize = 10;
    pub const N: usize = 11;
    const NAMES: [&str; N] = [
        "fs-read",
        "alg-load",
        "alg-op",
        "exec-bytes",
        "hash-lookup",
        "linker",
        "state",
        "store",
        "instantiate",
        "bind",
        "args",
    ];

    static PHASES: KLock<[u64; N]> = KLock::new([0; N]);
    static COUNTS: KLock<[u64; N]> = KLock::new([0; N]);

    pub fn add_since(idx: usize, started_us: u64) {
        let elapsed = crate::timer::uptime_us().saturating_sub(started_us);
        PHASES.with(|p| p[idx] += elapsed);
        COUNTS.with(|c| c[idx] += 1);
    }

    pub fn dump_and_reset() {
        let phases = PHASES.with(|p| core::mem::take(p));
        let counts = COUNTS.with(|c| core::mem::take(c));
        let mut line = alloc::string::String::from("spawn-trace:");
        let mut total = 0u64;
        for i in 0..N {
            if counts[i] > 0 {
                line.push_str(&alloc::format!(
                    " {}={}us/{}",
                    NAMES[i],
                    phases[i],
                    counts[i]
                ));
                total += phases[i];
            }
        }
        line.push_str(&alloc::format!(" kernel-total={}us", total));
        crate::kprintln!("{}", line);
    }
}

/// Domain-separated blake3 over one fusion-graph node (owner design: the graph hash).
/// `children` are the operands' graph hashes in operand order; `args` are the operation's
/// canonicalized arguments (length-prefixed, in the order the operation received them —
/// argument order is part of the operation's meaning, so a reordering is a different node
/// and merely misses the cache).
#[cfg(feature = "wasm-codegen")]
fn graph_node_hash(tag: &str, children: &[[u8; 32]], args: &[&str]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"eo9-fusion-graph-v1\0");
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag.as_bytes());
    hasher.update(&(children.len() as u64).to_le_bytes());
    for child in children {
        hasher.update(child);
    }
    hasher.update(&(args.len() as u64).to_le_bytes());
    for arg in args {
        hasher.update(&(arg.len() as u64).to_le_bytes());
        hasher.update(arg.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// A leaf's graph hash: blake3 of the component bytes themselves.
#[cfg(feature = "wasm-codegen")]
fn graph_leaf_hash(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"eo9-fusion-leaf-v1\0");
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// Per-store-entry leaf hashes, computed once on first use (the store is immutable for
/// the boot's lifetime, so the hash of entry *i* never changes).
#[cfg(feature = "wasm-codegen")]
static ENTRY_LEAF_HASHES: KLock<Vec<Option<[u8; 32]>>> = KLock::new(Vec::new());

#[cfg(feature = "wasm-codegen")]
fn entry_leaf_hash(entries: &'static [super::store::StoreEntry], index: usize) -> [u8; 32] {
    if let Some(hash) = ENTRY_LEAF_HASHES.with(|hashes| hashes.get(index).copied().flatten()) {
        return hash;
    }
    let hash = graph_leaf_hash(entries[index].component);
    ENTRY_LEAF_HASHES.with(|hashes| {
        if hashes.len() <= index {
            hashes.resize(index + 1, None);
        }
        hashes[index] = Some(hash);
    });
    hash
}

/// The compiler fingerprint: a build-time blake3 over the vendored compiler sources
/// (kernel/vendor/**) plus the engine-config sources (wasm/mod.rs, wasm/codegen.rs),
/// emitted by build.rs. It joins every *persistent* compiled-artifact cache key (the
/// storedisk compile cache), so a vendored cranelift/wasmtime change — including a
/// miscompile fix — makes every old entry an unreachable clean MISS (owner ruling: the
/// fingerprint lives in the lookup key; no verification-failure path exists for
/// staleness; the keyed MAC stays reserved for genuine integrity failures, and the
/// in-RAM session cache needs no fingerprint because one boot has one engine).
#[cfg(feature = "wasm-codegen")]
pub(super) const COMPILER_FINGERPRINT: &str = env!("EO9_COMPILER_FINGERPRINT");

/// The shell session's exec state.
#[derive(Default)]
pub struct ShellExec {
    components: Vec<Option<KComponent>>,
    images: Vec<Option<KImage>>,
    /// In-RAM compiled-artifact cache, keyed by the **graph hash** (semantic identity —
    /// see `KComponent::graph_hash`): covers fused algebra results *and* pristine store
    /// entries (whose per-spawn deserialization is also nontrivial under TCG). LRU over
    /// `COMPILE_CACHE_ENTRIES`. The blake3 key is collision-resistant, so no byte-equality
    /// confirmation is needed (and on a hit there are no fused bytes to compare — the
    /// whole point is that re-fusion was skipped). The persistent `storedisk` cache is
    /// unchanged in role: it serves across boots; this serves within a session.
    #[cfg(feature = "wasm-codegen")]
    compiled: Vec<([u8; 32], Component)>,
    /// In-RAM fused-bytes cache, keyed by the graph hash: a repeat of an identical
    /// fusion skips `eo9_component::compose`/`extend`/… entirely (the dominant warm
    /// spawn cost — see docs/spikes/spawn-latency.md). Same LRU bound; ~250 KiB per
    /// entry worst case.
    #[cfg(feature = "wasm-codegen")]
    fused: Vec<([u8; 32], Vec<u8>)>,
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

    pub(super) fn take_component(&mut self, rep: u32) -> Result<KComponent> {
        self.components
            .get_mut(rep as usize)
            .and_then(Option::take)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown component handle {rep}")))
    }

    /// Look up a compiled artifact by graph hash (semantic identity); a hit refreshes the
    /// entry to the back of the list (LRU).
    #[cfg(feature = "wasm-codegen")]
    fn cached_compile(&mut self, graph_hash: &[u8; 32]) -> Option<Component> {
        let index = self.compiled.iter().position(|(h, _)| h == graph_hash)?;
        let entry = self.compiled.remove(index);
        let component = entry.1.clone();
        self.compiled.push(entry);
        Some(component)
    }

    /// Insert a freshly compiled (or deserialized) artifact, evicting the
    /// least-recently-used entry beyond the cache bound.
    #[cfg(feature = "wasm-codegen")]
    fn cache_compile(&mut self, graph_hash: [u8; 32], component: Component) {
        self.compiled.retain(|(h, _)| h != &graph_hash);
        self.compiled.push((graph_hash, component));
        while self.compiled.len() > COMPILE_CACHE_ENTRIES {
            self.compiled.remove(0);
        }
    }

    /// Look up fused bytes by graph hash; a hit refreshes to the back (LRU) and clones
    /// the bytes (a bounded memcpy, far cheaper than re-running the fusion).
    #[cfg(feature = "wasm-codegen")]
    fn cached_fused(&mut self, graph_hash: &[u8; 32]) -> Option<Vec<u8>> {
        let index = self.fused.iter().position(|(h, _)| h == graph_hash)?;
        let entry = self.fused.remove(index);
        let bytes = entry.1.clone();
        self.fused.push(entry);
        Some(bytes)
    }

    /// Remember a fusion result, evicting the least-recently-used entry beyond the bound.
    #[cfg(feature = "wasm-codegen")]
    fn cache_fused(&mut self, graph_hash: [u8; 32], bytes: Vec<u8>) {
        self.fused.retain(|(h, _)| h != &graph_hash);
        self.fused.push((graph_hash, bytes));
        while self.fused.len() > COMPILE_CACHE_ENTRIES {
            self.fused.remove(0);
        }
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
    tag: &str,
    a: KComponent,
    b: KComponent,
    op: impl Fn(
        &eo9_component::Component,
        &eo9_component::Component,
    ) -> core::result::Result<eo9_component::Component, eo9_component::ComposeError>,
) -> core::result::Result<Resource<AlgComponentRes>, WitComposeError> {
    #[cfg(feature = "spawn-trace")]
    let __trace_op = crate::timer::uptime_us();
    let node_hash = graph_node_hash(tag, &[a.graph_hash, b.graph_hash], &[]);
    // The fused-bytes session cache: an identical fusion graph re-fuses to identical
    // bytes (the encoder is deterministic — BTreeMap-ordered, no randomness; the
    // `graph-verify` feature re-runs the op on every hit and asserts equality, which the
    // verification battery exercises), so a hit skips the fusion entirely.
    let cached = store
        .data_mut()
        .shell_exec()
        .ok()
        .and_then(|exec| exec.cached_fused(&node_hash));
    let fused_bytes = match cached {
        Some(bytes) => {
            #[cfg(feature = "graph-verify")]
            {
                let av = eo9_component::Component::load(a.bytes.clone()).map_err(|err| {
                    WitComposeError::Internal(format!("operand is not a component: {err}"))
                })?;
                let bv = eo9_component::Component::load(b.bytes.clone()).map_err(|err| {
                    WitComposeError::Internal(format!("operand is not a component: {err}"))
                })?;
                let refused = op(&av, &bv)
                    .map_err(|err| WitComposeError::Internal(format!("{err}")))?
                    .into_bytes();
                assert!(
                    refused == bytes,
                    "graph-verify: fusion cache hit disagrees with re-encoding                      (the encoder is not deterministic, or the node hash is wrong)"
                );
                crate::kprintln!("graph-verify: fusion hit re-encoded identically");
            }
            bytes
        }
        None => {
            let load = |bytes| {
                eo9_component::Component::load(bytes).map_err(|err| {
                    WitComposeError::Internal(format!("operand is not a component: {err}"))
                })
            };
            let av = load(a.bytes)?;
            let bv = load(b.bytes)?;
            let fused = op(&av, &bv).map_err(|err| WitComposeError::Internal(format!("{err}")))?;
            let bytes = fused.into_bytes();
            if let Ok(exec) = store.data_mut().shell_exec() {
                exec.cache_fused(node_hash, bytes.clone());
            }
            bytes
        }
    };
    #[cfg(feature = "spawn-trace")]
    spawn_trace::add_since(spawn_trace::ALG_OP, __trace_op);
    let rep = store
        .data_mut()
        .shell_exec()
        .map_err(|err| WitComposeError::Internal(format!("{err}")))?
        .insert_component(KComponent {
            bytes: fused_bytes,
            graph_hash: node_hash,
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
// Shared component helpers (used by the exec surface and the service registry)
// -----------------------------------------------------------------------------------------

/// Describe an open component value: baked store entries replay their precomputed
/// metadata; algebra results are described with the eo9-component loader (on-target
/// codegen builds only).
pub(super) fn component_info(
    entries: &'static [super::store::StoreEntry],
    component: &KComponent,
) -> Result<WitComponentInfo, String> {
    match component.entry {
        Some(entry) => Ok(parse_metadata(entries[entry].metadata)),
        #[cfg(feature = "wasm-codegen")]
        None => wit_info_from_eo9(&component.bytes).map_err(|err| format!("{err}")),
        #[cfg(not(feature = "wasm-codegen"))]
        None => Err("cannot describe a composed component without on-target codegen".to_string()),
    }
}

/// The wiring view of an open component value (leaf-only on the kernel, as in the
/// exec surface's `wiring` operation).
pub(super) fn component_wiring(component: &KComponent) -> String {
    #[cfg(feature = "wasm-codegen")]
    {
        match eo9_component::Component::load(component.bytes.clone()) {
            Ok(loaded) => loaded.wiring_tree(),
            Err(_) => String::from("(wiring unavailable)"),
        }
    }
    #[cfg(not(feature = "wasm-codegen"))]
    {
        let _ = component;
        String::from("(wiring unavailable: this kernel build has no component loader)")
    }
}

/// Compile an open component value to a runnable artifact: the baked host-AOT artifact
/// for pristine store entries (the fast path), on-target Cranelift for algebra results.
/// The same rule as the exec surface's `compile`, minus the persistent disk cache —
/// services compile exactly once at detach and restart from the in-memory artifact, so
/// the disk cache would only help across reboots (recorded follow-up). The *session*
/// compile cache is consulted, though: a detach of a composition the session already
/// compiled (spawned at the prompt, or detached under another name) reuses the artifact
/// and skips Cranelift, exactly like a re-spawn would.
pub(super) fn compile_component(
    engine: &Engine,
    entries: &'static [super::store::StoreEntry],
    component: &KComponent,
    exec: &mut ShellExec,
) -> Result<Component, String> {
    #[cfg(not(feature = "wasm-codegen"))]
    let _ = exec;
    // The session cache first, keyed by the graph hash — the same discipline as the
    // exec surface's `compile` (semantic identity; no byte equality needed).
    #[cfg(feature = "wasm-codegen")]
    if let Some(component) = exec.cached_compile(&component.graph_hash) {
        return Ok(component);
    }
    match component.entry {
        // SAFETY: the artifact comes from the store image produced by `cargo xtask
        // build-kernel` with the same wasmtime version and engine configuration.
        Some(entry) => {
            let deserialized = unsafe { Component::deserialize(engine, entries[entry].artifact) }
                .map_err(|err| format!("the baked-in artifact failed to load: {err:?}"));
            #[cfg(feature = "wasm-codegen")]
            if let Ok(image) = &deserialized {
                exec.cache_compile(component.graph_hash, image.clone());
            }
            deserialized
        }
        #[cfg(feature = "wasm-codegen")]
        None => {
            let exec_bytes = eo9_component::Component::load(component.bytes.clone())
                .map(|c| c.executable_bytes())
                .unwrap_or_else(|_| component.bytes.clone());
            // Codegen blocks the console; announce it so a long compile (a detach of a
            // large composition) never reads as a frozen shell.
            crate::kprintln!(
                "codegen: compiling the composed component on-target ({} KiB) …",
                exec_bytes.len() / 1024
            );
            let started = crate::timer::uptime_us();
            let compiled = Component::new(engine, &exec_bytes)
                .map_err(|err| format!("on-target compilation failed: {err:?}"));
            if let Ok(image) = &compiled {
                crate::kprintln!(
                    "codegen: compiled in {} ms",
                    (crate::timer::uptime_us() - started) / 1000
                );
                exec.cache_compile(component.graph_hash, image.clone());
            }
            compiled
        }
        #[cfg(not(feature = "wasm-codegen"))]
        None => Err("composed components require on-target codegen".to_string()),
    }
}

// -----------------------------------------------------------------------------------------
// State plumbing
// -----------------------------------------------------------------------------------------

impl KernelState {
    pub(super) fn shell_exec(&mut self) -> Result<&mut ShellExec> {
        self.shell
            .as_mut()
            .map(|shell| &mut shell.exec)
            .ok_or_else(|| wasmtime::Error::msg("the exec capability was not granted to this task"))
    }

    pub(super) fn shell_engine(&mut self) -> Result<Engine> {
        self.shell
            .as_mut()
            .map(|shell| shell.engine.clone())
            .ok_or_else(|| wasmtime::Error::msg("the exec capability was not granted to this task"))
    }

    pub(super) fn shell_entries(&mut self) -> Result<&'static [super::store::StoreEntry]> {
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
            #[cfg(feature = "spawn-trace")]
            let __trace_load = crate::timer::uptime_us();
            let entries = store.data_mut().shell_entries()?;
            let entry = entries
                .iter()
                .position(|entry| entry.component == bytes.as_slice());
            #[cfg(feature = "spawn-trace")]
            spawn_trace::add_since(spawn_trace::ALG_LOAD, __trace_load);
            Ok((match entry {
                Some(entry) => {
                    #[cfg(feature = "wasm-codegen")]
                    let graph_hash = entry_leaf_hash(entries, entry);
                    #[cfg(not(feature = "wasm-codegen"))]
                    let graph_hash = [0u8; 32];
                    let rep = store.data_mut().shell_exec()?.insert_component(KComponent {
                        bytes: entries[entry].component.to_vec(),
                        entry: Some(entry),
                        graph_hash,
                    });
                    Ok(Resource::new_own(rep))
                }
                // With on-target codegen the kernel can also load components that are not in
                // the baked-in store (e.g. algebra results round-tripped through `save`),
                // validating them with the same `eo9-component` loader usermode uses.
                #[cfg(feature = "wasm-codegen")]
                None => match eo9_component::Component::load(bytes) {
                    Ok(component) => {
                        let bytes = component.into_bytes();
                        let graph_hash = graph_leaf_hash(&bytes);
                        let rep = store.data_mut().shell_exec()?.insert_component(KComponent {
                            bytes,
                            entry: None,
                            graph_hash,
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
        "wiring",
        |mut store: StoreContextMut<'_, KernelState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(String,)> {
            // The kernel's algebra stores results as fused bytes (no in-memory
            // provenance survives `alg_binary_op`'s save), so the wiring view here is
            // always the single leaf the loader reconstructs from the bytes. Keeping the
            // in-memory `eo9_component::Component` values across operations (and with
            // them the full tree) is the recorded follow-up in plan/02.
            let kc = store.data_mut().shell_exec()?.component(component.rep())?;
            #[cfg(feature = "wasm-codegen")]
            {
                let loaded = eo9_component::Component::load(kc.bytes.clone()).map_err(|err| {
                    wasmtime::Error::msg(format!("failed to load component: {err}"))
                })?;
                Ok((loaded.wiring_tree(),))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = kc;
                Ok((String::from(
                    "(wiring unavailable: this kernel build has no component loader)",
                ),))
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
                    exec.take_component(provider.rep())?,
                    exec.take_component(consumer.rep())?,
                )
            };
            #[cfg(feature = "wasm-codegen")]
            {
                Ok((alg_binary_op(
                    &mut store,
                    "compose",
                    pb,
                    cb,
                    eo9_component::compose,
                ),))
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
                    exec.take_component(base.rep())?,
                    exec.take_component(layer.rep())?,
                )
            };
            #[cfg(feature = "wasm-codegen")]
            {
                Ok((alg_binary_op(
                    &mut store,
                    "extend",
                    bb,
                    lb,
                    eo9_component::extend,
                ),))
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
            let kc = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?;
            #[cfg(feature = "wasm-codegen")]
            {
                let allow: Vec<eo9_component::InterfaceRef> = allow
                    .into_iter()
                    .map(|r| eo9_component::InterfaceRef {
                        interface: r.interface,
                        version: r.version,
                    })
                    .collect();
                let arg_strings: Vec<String> = allow
                    .iter()
                    .map(|r| match &r.version {
                        Some(version) => format!("{}@{}", r.interface, version),
                        None => r.interface.clone(),
                    })
                    .collect();
                let args_ref: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
                let node_hash = graph_node_hash("restrict", &[kc.graph_hash], &args_ref);
                let result =
                    (|| -> core::result::Result<Resource<AlgComponentRes>, WitRestrictError> {
                        let cached = store
                            .data_mut()
                            .shell_exec()
                            .ok()
                            .and_then(|exec| exec.cached_fused(&node_hash));
                        let bytes = match cached {
                            Some(bytes) => bytes,
                            None => {
                                let c = eo9_component::Component::load(kc.bytes)
                                    .map_err(|e| WitRestrictError::Internal(format!("{e}")))?;
                                let restricted = eo9_component::restrict(&c, &allow)
                                    .map_err(|e| WitRestrictError::Internal(format!("{e}")))?;
                                let bytes = restricted.into_bytes();
                                if let Ok(exec) = store.data_mut().shell_exec() {
                                    exec.cache_fused(node_hash, bytes.clone());
                                }
                                bytes
                            }
                        };
                        let rep = store
                            .data_mut()
                            .shell_exec()
                            .map_err(|e| WitRestrictError::Internal(format!("{e}")))?
                            .insert_component(KComponent {
                                bytes,
                                graph_hash: node_hash,
                                entry: None,
                            });
                        Ok(Resource::new_own(rep))
                    })();
                Ok((result,))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (kc, allow);
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
            let kc = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?;
            #[cfg(feature = "wasm-codegen")]
            {
                let node_hash = graph_node_hash("rename", &[kc.graph_hash], &[&old, &new]);
                let result =
                    (|| -> core::result::Result<Resource<AlgComponentRes>, WitRenameError> {
                        let cached = store
                            .data_mut()
                            .shell_exec()
                            .ok()
                            .and_then(|exec| exec.cached_fused(&node_hash));
                        let bytes = match cached {
                            Some(bytes) => bytes,
                            None => {
                                let c = eo9_component::Component::load(kc.bytes)
                                    .map_err(|e| WitRenameError::Internal(format!("{e}")))?;
                                let renamed = eo9_component::rename(&c, &old, &new)
                                    .map_err(|e| WitRenameError::Internal(format!("{e}")))?;
                                let bytes = renamed.into_bytes();
                                if let Ok(exec) = store.data_mut().shell_exec() {
                                    exec.cache_fused(node_hash, bytes.clone());
                                }
                                bytes
                            }
                        };
                        let rep = store
                            .data_mut()
                            .shell_exec()
                            .map_err(|e| WitRenameError::Internal(format!("{e}")))?
                            .insert_component(KComponent {
                                bytes,
                                graph_hash: node_hash,
                                entry: None,
                            });
                        Ok(Resource::new_own(rep))
                    })();
                Ok((result,))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (kc, old, new);
                Ok((Err(WitRenameError::Internal(unsupported("rename"))),))
            }
        },
    )?;

    algebra.func_wrap(
        "configure",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, args): (Resource<AlgComponentRes>, Vec<WitNamedArg>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitConfigureError>,)> {
            let kc = store
                .data_mut()
                .shell_exec()?
                .take_component(component.rep())?;
            #[cfg(feature = "wasm-codegen")]
            {
                let pairs: Vec<(String, String)> =
                    args.into_iter().map(|a| (a.name, a.value)).collect();
                let arg_strings: Vec<String> = pairs
                    .iter()
                    .map(|(name, value)| format!("{name}\u{0}{value}"))
                    .collect();
                let args_ref: Vec<&str> = arg_strings.iter().map(String::as_str).collect();
                let node_hash = graph_node_hash("configure", &[kc.graph_hash], &args_ref);
                let result =
                    (|| -> core::result::Result<Resource<AlgComponentRes>, WitConfigureError> {
                        let cached = store
                            .data_mut()
                            .shell_exec()
                            .ok()
                            .and_then(|exec| exec.cached_fused(&node_hash));
                        let bytes = match cached {
                            Some(bytes) => bytes,
                            None => {
                                let c = eo9_component::Component::load(kc.bytes)
                                    .map_err(|e| WitConfigureError::Internal(format!("{e}")))?;
                                let configured = eo9_component::configure(&c, &pairs)
                                    .map_err(|e| WitConfigureError::Internal(format!("{e}")))?;
                                let bytes = configured.into_bytes();
                                if let Ok(exec) = store.data_mut().shell_exec() {
                                    exec.cache_fused(node_hash, bytes.clone());
                                }
                                bytes
                            }
                        };
                        let rep = store
                            .data_mut()
                            .shell_exec()
                            .map_err(|e| WitConfigureError::Internal(format!("{e}")))?
                            .insert_component(KComponent {
                                bytes,
                                graph_hash: node_hash,
                                entry: None,
                            });
                        Ok(Resource::new_own(rep))
                    })();
                Ok((result,))
            }
            #[cfg(not(feature = "wasm-codegen"))]
            {
                let _ = (kc, args);
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

            // The session cache first, keyed by the graph hash (semantic identity): a hit
            // covers fused results *and* pristine entries, and skips exec-bytes
            // extraction, deserialization, and codegen alike. Providers can never be
            // cached (their first compile attempt is refused below), so cache-first
            // cannot bypass the NotABinary refusals.
            #[cfg(feature = "wasm-codegen")]
            {
                #[cfg(feature = "spawn-trace")]
                let __trace_hl = crate::timer::uptime_us();
                let hit = store
                    .data_mut()
                    .shell_exec()?
                    .cached_compile(&component.graph_hash);
                #[cfg(feature = "spawn-trace")]
                spawn_trace::add_since(spawn_trace::HASH_LOOKUP, __trace_hl);
                if let Some(component_hit) = hit {
                    let rep = store.data_mut().shell_exec()?.insert_image(KImage {
                        component: component_hit,
                    });
                    return Ok((Ok(Resource::new_own(rep)),));
                }
            }

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
                    // Strip `implements` annotations before codegen: a renamed residual
                    // import or a multi-instance consumer carries one the vendored runtime's
                    // parser predates, so compiling the stored (annotated) bytes would fail
                    // with an opaque parse error. Identical to the stored bytes when there is
                    // no annotation; `describe`/the algebra keep the full form (`kc.bytes`).
                    #[cfg(feature = "spawn-trace")]
                    let __trace_eb = crate::timer::uptime_us();
                    let exec_bytes = eo9_component::Component::load(component.bytes.clone())
                        .map(|c| c.executable_bytes())
                        .unwrap_or_else(|_| component.bytes.clone());
                    #[cfg(feature = "spawn-trace")]
                    spawn_trace::add_since(spawn_trace::EXEC_BYTES, __trace_eb);

                    // The persistent store disk, when this boot has one, caches compile
                    // results by the blake3 of exactly these bytes: a hit deserializes the
                    // artifact compiled on an earlier boot instead of re-running Cranelift;
                    // a miss compiles and then writes the artifact back. The fused bytes are
                    // deterministic for a given composition, so the key is stable across
                    // boots. A cached artifact that fails to deserialize (different wasmtime
                    // build, corruption eofs could not catch) falls through to a fresh
                    // compile that overwrites it.
                    #[cfg(feature = "wasm-storedisk")]
                    let cached: Option<wasmtime::component::Component> =
                        if super::diskcache::enabled() {
                            let key = super::diskcache::key(&exec_bytes);
                            super::diskcache::lookup(&key).and_then(|artifact| {
                                let started = crate::timer::uptime_us();
                                // SAFETY: the artifact was produced by this kernel's own
                                // on-target compiler on a previous boot and stored on the
                                // operator-attached store disk (same trust class as the
                                // baked-in image); deserialize validates compatibility.
                                match unsafe { Component::deserialize(&engine, &artifact) } {
                                    Ok(component) => {
                                        crate::kprintln!(
                                            "storedisk: compile cache hit ({} KiB loaded in {} us)",
                                            artifact.len() / 1024,
                                            crate::timer::uptime_us() - started
                                        );
                                        Some(component)
                                    }
                                    Err(error) => {
                                        crate::kprintln!(
                                            "storedisk: cached artifact rejected ({error:?}); \
                                             recompiling"
                                        );
                                        None
                                    }
                                }
                            })
                        } else {
                            None
                        };
                    #[cfg(not(feature = "wasm-storedisk"))]
                    let cached: Option<wasmtime::component::Component> = None;

                    let image = match cached {
                        Some(component) => Ok(component),
                        None => {
                            // The console is single-threaded and codegen blocks it for
                            // seconds (much longer on a loaded host): say so *before* the
                            // silence, so a long compile never reads as a frozen shell
                            // (plan/12, the gfx freeze investigation).
                            crate::kprintln!(
                                "codegen: compiling the composed component on-target \
                                 ({} KiB) …",
                                exec_bytes.len() / 1024
                            );
                            let compile_started = crate::timer::uptime_us();
                            let compiled = Component::new(&engine, &exec_bytes).map_err(|err| {
                                WitCompileError::Codegen(format!(
                                    "on-target compilation failed: {err:?}"
                                ))
                            });
                            if compiled.is_ok() {
                                let elapsed_ms =
                                    (crate::timer::uptime_us() - compile_started) / 1000;
                                crate::kprintln!("codegen: compiled in {elapsed_ms} ms");
                            }
                            #[cfg(feature = "wasm-storedisk")]
                            if let Ok(component) = &compiled {
                                if super::diskcache::enabled() {
                                    match component.serialize() {
                                        Ok(artifact) => {
                                            let key = super::diskcache::key(&exec_bytes);
                                            super::diskcache::store(&key, &artifact);
                                        }
                                        Err(error) => crate::kprintln!(
                                            "storedisk: serializing the compiled artifact \
                                             failed ({error:?}); not cached"
                                        ),
                                    }
                                }
                            }
                            compiled
                        }
                    };
                    image
                }
                #[cfg(not(feature = "wasm-codegen"))]
                None => Err(WitCompileError::Codegen(
                    "composed components require on-target codegen".to_string(),
                )),
            };

            // Either way the artifact came to exist (deserialized, loaded from the store
            // disk, or compiled), remember it in the session cache under the graph hash so
            // the next spawn of the same semantic composition skips this whole function.
            #[cfg(feature = "wasm-codegen")]
            if let Ok(image) = &image {
                store
                    .data_mut()
                    .shell_exec()?
                    .cache_compile(component.graph_hash, image.clone());
            }
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
         (image, args, components, limits): (
            Resource<ExecImageRes>,
            Vec<WitNamedArg>,
            Vec<WitComponentArg>,
            WitSpawnLimits,
        )|
         -> Result<(Result<Resource<ChildTaskRes>, WitSpawnError>,)> {
            let engine = store.data_mut().shell_engine()?;
            let entries = store.data_mut().shell_entries()?;
            let child_generations = store.data().svc_generations.saturating_sub(1);
            let component = {
                let exec = store.data_mut().shell_exec()?;
                exec.image(image.rep())?.component.clone()
            };
            // Take each component argument out of the *spawner's* table now (ownership
            // transfer at the API boundary); they are re-minted in the child by
            // `spawn_child`.
            let mut component_args = Vec::with_capacity(components.len());
            {
                let exec = store.data_mut().shell_exec()?;
                for arg in components {
                    let value = exec.take_component(arg.value.rep())?;
                    component_args.push((arg.name, value));
                }
            }
            Ok((
                match spawn_child(
                    &engine,
                    entries,
                    &component,
                    &args,
                    component_args,
                    limits.max_memory,
                    child_generations,
                ) {
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
                            // Ctrl-C at the console interrupts the foreground job: the shell
                            // is parked here (not in `read-line`) while waiting on the child,
                            // so a Ctrl-C in the RX ring means "kill what I'm waiting on" and
                            // return to the prompt. Kills the awaited task and its descendants
                            // (a foreground nested eosh takes its own children down with it),
                            // mirroring `task.kill`. The boot supervisor's wait on the console
                            // is exempt (ROOT_CONSUMES_CTRL_C false): the interrupt key belongs
                            // to the console's foreground job, never to the console itself.
                            let waiter_consumes = CURRENT_PARENT.load(Ordering::Acquire)
                                != u32::MAX
                                || ROOT_CONSUMES_CTRL_C.load(Ordering::Acquire);
                            if waiter_consumes && crate::uart::take_ctrl_c() {
                                let outcome = kill_task_tree(rep);
                                return Poll::Ready(Ok(outcome));
                            }
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
                // Reject an unknown handle; otherwise kill the task and its descendants.
                // Dropping a drive future drops the child's store, guest state, and in-flight
                // work (SPEC "Kill and linearity"); for a child currently checked out by
                // `drive_children`, that drop happens when its poll returns and sees the slot
                // is no longer `Polling`.
                let known = CHILDREN.with(|children| matches!(children.get(rep), Some(Some(_))));
                if !known {
                    return Err(wasmtime::Error::msg(format!("unknown task handle {rep}")));
                }
                let outcome = kill_task_tree(rep);
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
        "PCI device access, which this boot did not grant (add the `pci` token to the \
         kernel command line — `cargo xtask qemu aarch64 pci` — to provide it)"
    } else {
        return None;
    };
    Some(alloc::format!(
        "the program requires {capability} (refused at instantiation)"
    ))
}
