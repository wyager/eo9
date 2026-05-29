//! The Task API host side: spawn / resume / runnable / kill over a compiled [`Image`].
//!
//! This is the host implementation behind `eo9:exec/task`. A [`Task`] owns one Wasmtime
//! `Store` (one linear memory, one capability set, one fuel ledger) whose single guest
//! entrypoint is the component's `main`. Execution only ever happens inside
//! [`Task::resume`]: fuel is donated by the caller and the guest runs *on the caller's
//! thread* until the donation is spent, the guest blocks on host I/O, or `main` finishes.
//!
//! ## How a resume drives the guest (and why it looks like this)
//!
//! Wasmtime 45 keeps all component-model-async execution state (guest tasks, parked fibers,
//! pending host operations) inside the `Store`, and the embedder drives it by polling the
//! future returned by `Store::run_concurrent`. Two limitations of the pinned version shape
//! the implementation (full findings in plan/04-runtime.md § Decisions):
//!
//! * a **fuel yield suspends the executing fiber in place**, held by the in-flight
//!   `run_concurrent` poll — it is not parked in the store, so that future cannot be
//!   dropped and re-created between donations without destroying the guest;
//! * while that future exists it **mutably borrows the store**, so fuel cannot be added
//!   or inspected between donations.
//!
//! The runtime therefore uses one **long-lived drive future per task** that owns the store
//! for the task's whole life, a **fixed fuel-yield quantum** configured before the drive
//! starts, and an embedder-side **quantum ledger**:
//!
//! 1. at spawn the store gets an effectively-infinite fuel pool and a yield interval of
//!    [`FUEL_QUANTUM`]; the drive future (instantiated component + the one call to `main`)
//!    is created but not polled;
//! 2. `resume(fuel)` converts the donation into quanta and polls the drive: every poll that
//!    returns `Pending` after waking its own waker synchronously is one quantum of guest
//!    execution consumed (the fuel yield); polling stops when the donated quanta are spent
//!    — **out-of-fuel**, genuinely suspended, resumable later;
//! 3. a poll that returns `Pending` *without* a synchronous wake means nothing can progress
//!    until an external completion arrives — **blocked**;
//! 4. when `main` completes (value or trap) the outcome is rendered — **done**.
//!
//! Fuel accounting is therefore quantum-granular: a donation buys `fuel / FUEL_QUANTUM`
//! quanta, unspent remainder is carried to the next resume, and a resume that ends blocked
//! or done under-charges by at most one quantum. This is the documented shim over what
//! wasmtime 45 supports today; the WIT surface (`eo9:exec/task`) is unchanged.
//!
//! ## Doorbell
//!
//! The per-task doorbell is the edge-triggered wake channel from SPEC "How readiness is
//! implemented": every poll of the task's drive uses the doorbell as its waker, so host
//! completions (provider operations finishing on other threads) ring it, and `runnable`
//! reports / waits on exactly that edge. The per-completion queue itself lives inside
//! Wasmtime's per-store concurrent state (which already delivers completions to the guest's
//! waitable-sets); this crate adds only the doorbell on top.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use wasmtime::component::{Linker, Type, Val};
use wasmtime::{Store, Trap};

use crate::image::Image;
use crate::link;
use crate::outcome::Outcome;
use crate::providers::Providers;
use crate::wave;

pub use crate::wave::NamedArg;

/// The fuel-yield quantum: the granularity of CPU donation. The store suspends guest
/// execution every `FUEL_QUANTUM` units of fuel, which is the point at which `resume`
/// decides whether the donation is spent. See the module docs for why the quantum is fixed
/// at spawn rather than derived from each donation.
pub const FUEL_QUANTUM: u64 = 10_000;

/// Fuel budget for the instantiate phase of `spawn`. Instantiation is paid for by the
/// spawner, not by later `resume` donations, so it must be bounded: a component whose core
/// modules carry start-time code gets at most this much fuel before spawn fails (rather
/// than burning unbounded CPU). Eo9 components have no start functions and consume none of
/// it.
const SPAWN_FUEL: u64 = 4 * FUEL_QUANTUM;

/// Static resource limits, fixed at spawn (`eo9:exec/task.spawn-limits`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SpawnLimits {
    /// Ceiling on linear-memory growth in bytes, enforced at `memory.grow`.
    pub max_memory: Option<u64>,
    /// Ceiling on core-wasm table growth, in elements, enforced at `table.grow`. When
    /// absent but `max_memory` is set, a bound is derived from the memory ceiling (one
    /// element per 8 bytes of allowed linear memory) so that a memory-limited task can
    /// never grow tables without bound either.
    pub max_table_elements: Option<u64>,
}

/// Why a task could not be spawned (`eo9:exec/task.spawn-error`).
#[derive(Debug)]
pub enum SpawnError {
    /// Argument WAVE parse / type-check failure against `main`'s signature.
    BadArguments(String),
    /// An import could not be satisfied by the fused component plus the root providers
    /// (the loader rule from SPEC "WASM runtime"), or instantiation failed.
    Internal(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::BadArguments(msg) => write!(f, "bad arguments: {msg}"),
            SpawnError::Internal(msg) => write!(f, "spawn failed: {msg}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// The result of one `resume` donation (`eo9:exec/task.resume-outcome`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// The donation was consumed; the task has more work to do.
    OutOfFuel,
    /// Every in-flight operation is waiting on an external completion; donating more fuel
    /// will not help until the doorbell rings (see [`Task::runnable`]).
    Blocked,
    /// `main` finished (or the task trapped); the task will not run again.
    Done(Outcome),
}

// ---------------------------------------------------------------------------------------
// Doorbell
// ---------------------------------------------------------------------------------------

/// Edge-triggered per-task doorbell. Rung by any wake of the task's store (host completions
/// from provider threads, guest fuel yields); drained by the run loop and by `runnable`.
#[derive(Default)]
pub(crate) struct Doorbell {
    rung: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
}

impl Doorbell {
    fn ring(&self) {
        self.rung.store(true, Ordering::SeqCst);
        let waiters = std::mem::take(&mut *self.waiters.lock().unwrap());
        for waker in waiters {
            waker.wake();
        }
    }

    /// Clear the doorbell, returning whether it had been rung (the edge).
    fn take(&self) -> bool {
        self.rung.swap(false, Ordering::SeqCst)
    }

    fn is_rung(&self) -> bool {
        self.rung.load(Ordering::SeqCst)
    }

    fn register(&self, waker: &Waker) {
        self.waiters.lock().unwrap().push(waker.clone());
    }
}

impl std::task::Wake for Doorbell {
    fn wake(self: Arc<Self>) {
        self.ring();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.ring();
    }
}

// ---------------------------------------------------------------------------------------
// Store data
// ---------------------------------------------------------------------------------------

/// Resource limits enforced where WASM asks the host for resources (`memory.grow`,
/// `table.grow`).
struct StoreLimits {
    max_memory: Option<u64>,
    max_table_elements: Option<u64>,
}

impl StoreLimits {
    fn new(limits: &SpawnLimits) -> Self {
        Self {
            max_memory: limits.max_memory,
            // A memory-limited task must not be able to grow tables without bound either:
            // derive a table ceiling from the memory ceiling when none was given
            // explicitly (one element per 8 bytes — the host-side size of a reference).
            max_table_elements: limits
                .max_table_elements
                .or_else(|| limits.max_memory.map(|max| (max / 8).max(1))),
        }
    }
}

impl wasmtime::ResourceLimiter for StoreLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(match self.max_memory {
            Some(max) => desired as u64 <= max,
            None => true,
        })
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(match self.max_table_elements {
            Some(max) => desired as u64 <= max,
            None => true,
        })
    }
}

/// Per-task store data: the task's root providers, its resource limits, and the host-side
/// table backing `eo9:io/buffers` handles.
pub(crate) struct TaskState {
    pub(crate) providers: Providers,
    pub(crate) buffers: BufferTable,
    limits: StoreLimits,
    /// The task's `eo9:rt/diagnostics` slot: the panic message the guest reported just
    /// before trapping, if any. Write-once, bounded, shared with the [`Task`] so the trap
    /// rendering can read it after the store has been consumed by the drive future.
    pub(crate) panic_message: Arc<Mutex<Option<String>>>,
}

/// Ceiling on a reported panic message (bytes); anything longer is truncated on a char
/// boundary. Diagnostics must never become a way to make the host hold unbounded data.
const MAX_PANIC_MESSAGE_BYTES: usize = 1024;

impl TaskState {
    /// Record the guest's reported panic message (write-once: the first report wins).
    pub(crate) fn report_panic(&self, message: String) {
        let mut slot = self.panic_message.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let mut message = message;
        if message.len() > MAX_PANIC_MESSAGE_BYTES {
            let mut end = MAX_PANIC_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push('…');
        }
        *slot = Some(message);
    }
}

// ---------------------------------------------------------------------------------------
// Host-side buffer table (eo9:io/buffers)
// ---------------------------------------------------------------------------------------

/// Per-buffer allocation ceiling for `eo9:io/buffers.buffer` (bytes). I/O buffers are
/// host-side memory, outside the guest's linear-memory ceiling, so their size must be
/// bounded before allocation (same rule as the entropy request cap).
pub const MAX_BUFFER_BYTES: u64 = 16 * 1024 * 1024;

/// Ceiling on the total bytes held by all live buffers of one task.
pub const MAX_TOTAL_BUFFER_BYTES: u64 = 64 * 1024 * 1024;

/// The backing store for the guest's `eo9:io/buffers.buffer` handles: resource rep ->
/// bytes. A slot whose bytes have been handed to a provider for an in-flight owned-buffer
/// round-trip stays reserved (`InFlight`) until the operation completes and the bytes come
/// back.
#[derive(Default)]
pub(crate) struct BufferTable {
    slots: Vec<BufferSlot>,
    total_bytes: u64,
}

enum BufferSlot {
    Free,
    /// Bytes currently owned by the guest-visible handle.
    Held(Vec<u8>),
    /// Bytes handed to a provider for an in-flight operation; the slot (and its byte
    /// budget) stays reserved until they come back.
    InFlight(u64),
}

impl BufferTable {
    /// Allocate a zero-filled buffer of `len` bytes, returning its rep.
    pub(crate) fn alloc(&mut self, len: u64) -> wasmtime::Result<u32> {
        if len > MAX_BUFFER_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "buffer of {len} bytes exceeds the per-buffer cap of {MAX_BUFFER_BYTES} bytes"
            )));
        }
        if self.total_bytes + len > MAX_TOTAL_BUFFER_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "task buffer budget exceeded: {len} more bytes would pass the \
                 {MAX_TOTAL_BUFFER_BYTES}-byte ceiling"
            )));
        }
        let bytes = vec![0; len as usize];
        self.total_bytes += len;
        let slot = self
            .slots
            .iter()
            .position(|slot| matches!(slot, BufferSlot::Free));
        let index = match slot {
            Some(index) => {
                self.slots[index] = BufferSlot::Held(bytes);
                index
            }
            None => {
                self.slots.push(BufferSlot::Held(bytes));
                self.slots.len() - 1
            }
        };
        u32::try_from(index).map_err(|_| wasmtime::Error::msg("buffer table full"))
    }

    fn slot(&mut self, rep: u32) -> wasmtime::Result<&mut BufferSlot> {
        self.slots
            .get_mut(rep as usize)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown buffer handle {rep}")))
    }

    /// Borrow the bytes held by a buffer (for the guest-facing accessors).
    pub(crate) fn bytes(&mut self, rep: u32) -> wasmtime::Result<&mut Vec<u8>> {
        match self.slot(rep)? {
            BufferSlot::Held(bytes) => Ok(bytes),
            BufferSlot::InFlight(_) => Err(wasmtime::Error::msg(
                "buffer is owned by an in-flight operation",
            )),
            BufferSlot::Free => Err(wasmtime::Error::msg("buffer handle is not live")),
        }
    }

    /// Take the bytes out for an owned-buffer round-trip, leaving the slot reserved.
    pub(crate) fn take(&mut self, rep: u32) -> wasmtime::Result<Vec<u8>> {
        let slot = self.slot(rep)?;
        match std::mem::replace(slot, BufferSlot::Free) {
            BufferSlot::Held(bytes) => {
                *slot = BufferSlot::InFlight(bytes.len() as u64);
                Ok(bytes)
            }
            other => {
                *slot = other;
                Err(wasmtime::Error::msg(
                    "buffer is not available for a new operation",
                ))
            }
        }
    }

    /// Give the bytes back to a slot reserved by [`BufferTable::take`].
    pub(crate) fn restore(&mut self, rep: u32, bytes: Vec<u8>) {
        if let Some(slot) = self.slots.get_mut(rep as usize)
            && let BufferSlot::InFlight(reserved) = *slot
        {
            self.total_bytes = self.total_bytes - reserved + bytes.len() as u64;
            *slot = BufferSlot::Held(bytes);
        }
    }

    /// Drop a buffer (guest dropped the handle).
    pub(crate) fn free(&mut self, rep: u32) {
        if let Some(slot) = self.slots.get_mut(rep as usize) {
            let released = match slot {
                BufferSlot::Held(bytes) => bytes.len() as u64,
                BufferSlot::InFlight(reserved) => *reserved,
                BufferSlot::Free => 0,
            };
            self.total_bytes = self.total_bytes.saturating_sub(released);
            *slot = BufferSlot::Free;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------------------

/// `main`'s eventual result: the returned value, or the trap/error that ended the call.
type MainResultCell = Arc<Mutex<Option<Result<Val, String>>>>;

enum LifeState {
    Running,
    Done(Outcome),
}

/// One spawned task: the long-lived drive future (which owns the store), the shared result
/// cell, and the doorbell.
pub struct Task {
    /// Owns the `Store` and runs the task's event loop plus the single call to `main`.
    drive: Pin<Box<dyn Future<Output = wasmtime::Result<()>> + Send>>,
    main_result: MainResultCell,
    result_ty: Option<Type>,
    doorbell: Arc<Doorbell>,
    state: LifeState,
    /// Donated fuel not yet charged (smaller than one quantum, or left over from a resume
    /// that ended blocked).
    carried_fuel: u64,
    /// Children spawned through this task's exec capability (shared with the exec
    /// provider inside the store). Driven by [`Task::resume`]; dropped with the task.
    children: Option<crate::exec::ChildSet>,
    /// True when the last resume returned [`ResumeOutcome::Blocked`] and the doorbell has
    /// not rung since.
    parked: bool,
    /// The guest-reported panic message (see [`TaskState::report_panic`]); read when a
    /// trap is rendered into the outcome.
    panic_message: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field(
                "state",
                match &self.state {
                    LifeState::Running => &"running",
                    LifeState::Done(_) => &"done",
                },
            )
            .field("parked", &self.parked)
            .field("carried_fuel", &self.carried_fuel)
            .finish_non_exhaustive()
    }
}

impl Task {
    /// Spawn a new task from a compiled image: instantiate it against the given root
    /// providers, type-check the WAVE `args` against `main`'s signature, and set up the
    /// call to `main`. No guest code runs until the first [`Task::resume`].
    pub fn spawn(
        image: &Image,
        args: &[NamedArg],
        limits: SpawnLimits,
        providers: Providers,
    ) -> Result<Self, SpawnError> {
        let internal = |err: wasmtime::Error| SpawnError::Internal(format!("{err:#}"));

        let engine = image.engine().clone();
        let children = providers.exec.as_ref().map(|exec| exec.child_set());
        let mut linker: Linker<TaskState> = Linker::new(&engine);
        link::add_providers(&mut linker, &providers)
            .map_err(|err| SpawnError::Internal(format!("provider wiring failed: {err:#}")))?;

        let panic_message: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let state = TaskState {
            providers,
            buffers: BufferTable::default(),
            limits: StoreLimits::new(&limits),
            panic_message: panic_message.clone(),
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|state| &mut state.limits);

        let doorbell = Arc::new(Doorbell::default());

        // Instantiation runs on a small, bounded fuel budget paid by the spawner: any
        // start-time code in the component either finishes within SPAWN_FUEL or the spawn
        // fails (no yield interval is configured yet, so exhaustion is a trap here, never
        // an unbounded burn). It must also not depend on external completions; drive it
        // with a bounded manual poll loop.
        store.set_fuel(SPAWN_FUEL).map_err(internal)?;
        let instance = {
            let instantiate = linker.instantiate_async(&mut store, image.component());
            let mut instantiate = std::pin::pin!(instantiate);
            let waker = Waker::from(doorbell.clone());
            let mut cx = Context::from_waker(&waker);
            let mut result = None;
            for _ in 0..1024 {
                match instantiate.as_mut().poll(&mut cx) {
                    Poll::Ready(r) => {
                        result = Some(r);
                        break;
                    }
                    Poll::Pending if doorbell.take() => continue,
                    Poll::Pending => break,
                }
            }
            result
                .ok_or_else(|| {
                    SpawnError::Internal("instantiation unexpectedly suspended".to_string())
                })?
                .map_err(|err| {
                    if matches!(err.downcast_ref::<Trap>(), Some(Trap::OutOfFuel)) {
                        SpawnError::Internal(format!(
                            "component start-time code exceeded the spawn fuel budget \
                             ({SPAWN_FUEL} fuel): instantiation must not run unbounded \
                             guest code"
                        ))
                    } else {
                        internal(err)
                    }
                })?
        };

        // Normal fuel regime for the task's life: an effectively-infinite pool sliced by
        // the fixed yield quantum. Both must be configured before the drive future takes
        // ownership of the store.
        store.set_fuel(u64::MAX).map_err(internal)?;
        store
            .fuel_async_yield_interval(Some(FUEL_QUANTUM))
            .map_err(internal)?;

        let main = instance
            .get_func(&mut store, "main")
            .ok_or_else(|| SpawnError::Internal("component does not export `main`".into()))?;

        let signature = main.ty(&store);
        let params = wave::parse_args(&signature, args).map_err(SpawnError::BadArguments)?;
        let result_ty = signature.results().next();

        // The long-lived drive: owns the store, runs the event loop, performs the one call
        // to `main`, and parks the result (value or trap) in the shared cell.
        let main_result: MainResultCell = Arc::new(Mutex::new(None));
        let cell = main_result.clone();
        let panic_slot = panic_message.clone();
        let drive = Box::pin(async move {
            let mut store = store;
            store
                .run_concurrent(async move |accessor| {
                    let mut results = vec![Val::Bool(false)];
                    let call = main.call_concurrent(accessor, &params, &mut results).await;
                    let stored = match call {
                        Ok(()) => Ok(results.into_iter().next().unwrap_or(Val::Bool(false))),
                        Err(err) => Err(crate::trap::trap_reason(
                            &err,
                            panic_slot.lock().unwrap().as_deref(),
                        )),
                    };
                    *cell.lock().unwrap() = Some(stored);
                })
                .await
        });

        Ok(Self {
            drive,
            main_result,
            result_ty,
            doorbell,
            state: LifeState::Running,
            carried_fuel: 0,
            children,
            parked: false,
            panic_message,
        })
    }

    /// Donate `fuel` to the task and run it now, on the caller's thread, until the donation
    /// is spent, the task blocks on I/O, or it finishes.
    pub fn resume(&mut self, fuel: u64) -> ResumeOutcome {
        if let LifeState::Done(outcome) = &self.state {
            return ResumeOutcome::Done(outcome.clone());
        }
        self.parked = false;

        // Whole quanta available from the carried remainder plus this donation.
        let mut budget = self.carried_fuel.saturating_add(fuel);
        let waker = Waker::from(self.doorbell.clone());
        let mut cx = Context::from_waker(&waker);

        loop {
            if budget < FUEL_QUANTUM {
                // Not enough left to run another quantum; carry the remainder.
                self.carried_fuel = budget;
                return ResumeOutcome::OutOfFuel;
            }

            // Children spawned through this task's exec capability run on their parent's
            // donated fuel, one slice per runnable child per iteration (they cannot be run
            // from inside the parent's own event loop — wasmtime forbids recursive
            // `run_concurrent` — so the embedder-facing resume is where they execute).
            if let Some(children) = self.children.clone() {
                let mut children = children.lock().unwrap();
                for child in children.iter_mut() {
                    if budget < FUEL_QUANTUM {
                        break;
                    }
                    if child.outcome().is_none() && child.is_runnable() {
                        child.resume(FUEL_QUANTUM);
                        budget -= FUEL_QUANTUM;
                    }
                }
            }
            if budget < FUEL_QUANTUM {
                self.carried_fuel = budget;
                return ResumeOutcome::OutOfFuel;
            }

            self.doorbell.take();
            match self.drive.as_mut().poll(&mut cx) {
                Poll::Ready(result) => {
                    let outcome = self.finish(result);
                    self.carried_fuel = budget;
                    return ResumeOutcome::Done(outcome);
                }
                Poll::Pending => {
                    if self.doorbell.take() {
                        // The guest yielded after consuming one fuel quantum; charge it
                        // and keep going while the donation lasts.
                        budget -= FUEL_QUANTUM;
                        continue;
                    }
                    // No synchronous wake: every pending operation waits on an external
                    // completion. The task is parked until the doorbell rings.
                    self.carried_fuel = budget;
                    self.parked = true;
                    return ResumeOutcome::Blocked;
                }
            }
        }
    }

    /// Convert the completed drive (and the shared result cell) into the final outcome.
    fn finish(&mut self, drive_result: wasmtime::Result<()>) -> Outcome {
        let main_result = self.main_result.lock().unwrap().take();
        let outcome = match (main_result, drive_result) {
            (Some(Ok(val)), _) => match &self.result_ty {
                Some(ty) => wave::render_outcome(ty, &val),
                None => Outcome::Success(crate::outcome::WaveValue {
                    ty: String::new(),
                    value: String::new(),
                }),
            },
            (Some(Err(trap)), _) => Outcome::Trapped(trap),
            (None, Err(err)) => Outcome::Trapped(crate::trap::trap_reason(
                &err,
                self.panic_message.lock().unwrap().as_deref(),
            )),
            (None, Ok(())) => {
                Outcome::Trapped("task event loop finished without a `main` result".to_string())
            }
        };
        self.state = LifeState::Done(outcome.clone());
        // Wake anyone parked in `wait()` (the doorbell's waiter list doubles as the
        // completion notification channel).
        self.doorbell.ring();
        outcome
    }

    /// True when the task could make progress if resumed: it has never been parked, or an
    /// external completion has rung the doorbell since it was parked. A finished task is
    /// never runnable.
    pub fn is_runnable(&self) -> bool {
        match self.state {
            LifeState::Done(_) => false,
            LifeState::Running => !self.parked || self.doorbell.is_rung(),
        }
    }

    /// Wait (as a plain future) until the task is runnable again. Resolves immediately if
    /// [`Task::is_runnable`] is already true; never resolves for a finished task (callers
    /// should check [`Task::outcome`] first).
    pub fn runnable(&self) -> impl Future<Output = ()> + '_ {
        std::future::poll_fn(move |cx| {
            if self.is_runnable() {
                Poll::Ready(())
            } else {
                self.doorbell.register(cx.waker());
                // Re-check to close the race between the check above and registration.
                if self.is_runnable() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        })
    }

    /// The task's final outcome, if it has finished.
    pub fn outcome(&self) -> Option<&Outcome> {
        match &self.state {
            LifeState::Done(outcome) => Some(outcome),
            LifeState::Running => None,
        }
    }

    /// Wait (as a plain future) until the task has finished, yielding its outcome — the
    /// host-side counterpart of `eo9:exec/task.wait`, which is now an `async func`.
    ///
    /// Like the WIT operation, this only observes completion: the task still only makes
    /// progress when someone donates fuel through [`Task::resume`].
    pub fn wait(&self) -> impl Future<Output = Outcome> + '_ {
        std::future::poll_fn(move |cx| {
            if let Some(outcome) = self.outcome() {
                return Poll::Ready(outcome.clone());
            }
            self.doorbell.register(cx.waker());
            // Re-check to close the race between the check above and registration.
            match self.outcome() {
                Some(outcome) => Poll::Ready(outcome.clone()),
                None => Poll::Pending,
            }
        })
    }

    /// Kill the task and return its final outcome.
    ///
    /// A killed task never observes anything again: its drive future — and with it the
    /// store, linear memory, guest state and queued completions — is dropped here.
    /// Outstanding provider operations are dropped with it; each provider's `Drop`
    /// completes or aborts the underlying work on its own schedule, and results destined
    /// for the dead task go nowhere (SPEC "Kill and linearity").
    pub fn kill(mut self) -> Outcome {
        self.kill_in_place()
    }

    /// Kill the task without consuming the handle: the drive future (and with it the
    /// store, guest state, and in-flight provider operations) is dropped immediately and
    /// the task becomes finished with [`Outcome::Killed`] (unless it had already
    /// finished, in which case that outcome is kept). Later observations — `outcome()`,
    /// `wait()`, or a guest-level `wait` through the exec surface — see the final
    /// outcome instead of an error.
    pub fn kill_in_place(&mut self) -> Outcome {
        if let LifeState::Done(outcome) = &self.state {
            return outcome.clone();
        }
        // Replace the drive with an inert future, dropping the store and everything in it.
        self.drive = Box::pin(std::future::ready(Ok(())));
        let outcome = Outcome::Killed;
        self.state = LifeState::Done(outcome.clone());
        self.doorbell.ring();
        outcome
    }

    /// Fuel donated to this task that has not yet been charged against guest execution
    /// (sub-quantum remainder plus anything donated while the task was blocked).
    pub fn unspent_fuel(&self) -> u64 {
        self.carried_fuel
    }
}
