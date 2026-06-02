//! The kernel service registry: the `eo9:svc` host surface on bare metal (executor v2).
//!
//! Mirrors the usermode registry (`crates/eo9-runtime/src/svc.rs`) on the kernel's own
//! machinery: detached services are reparented children of the *machine* — they live in
//! this registry (not the shell's child table), are pumped by the same root drive loop
//! that pumps foreground children, and survive the console exiting and restarting. The
//! registry's lifetime is the boot (owner ruling E: the kernel binds it to the machine);
//! `poweroff`/init-exit stops everything that is still running.
//!
//! # Capability soundness (the load-bearing rule, identical to usermode)
//!
//! A detached child runs with **exactly what its detacher composed into it**, plus the
//! registry's log capture and the `eo9:rt/*` riders — never the session's authority.
//! Detach refuses (typed `not-closed`) any composition whose residual *required* imports
//! fall outside text/rt/io, and the service linker registers nothing else: no fs, no
//! exec, no time, no entropy, no pci, no svc. The kernel session's inherit-everything
//! default applies to *foreground children only*; a service gets the short list.
//!
//! # Restart policies are programs
//!
//! The policy component is validated at detach (provider, exports
//! `eo9:svc/restart-policy`, pure) and **compiled once** at detach time — the kernel
//! deviation from usermode's compile-per-decision: on-target Cranelift is ~100 ms, so
//! the artifact is kept and only *instantiation* happens per decision. A policy that
//! traps, exhausts its fuel budget, or answers garbage reads as `give-up`: a broken
//! policy can never wedge the registry or restart-loop a service forever. A killed
//! service never consults its policy at all: stop means stop.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::AtomicBool;
use core::task::{Context, Poll, Waker};

use wasmtime::component::{
    Accessor, Component, ComponentType, Lift, Linker, Lower, Resource, ResourceType, Val,
};
use wasmtime::{Result, Store, StoreContextMut};

use super::providers::KernelState;
use super::shellexec::{self, DriveStatus, KOutcome};
use super::store::StoreEntry;

/// Boxed future shape for `func_wrap_concurrent` closures.
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

/// How many services the machine registry may hold (running or finished-but-not-cleared).
/// Same value as the usermode registry.
pub const MAX_SERVICES: usize = 16;
/// Ceiling on one service's captured log (bytes); older output is dropped first.
pub const MAX_LOG_BYTES: usize = 256 * 1024;
/// Ceiling on the failure history kept per service (records).
pub const MAX_HISTORY: usize = 64;
/// Fuel budget for one restart-policy `decide` (instantiation + the call).
pub const POLICY_FUEL: u64 = 50_000_000;
/// Ceiling on rendered outcome / error strings kept per service.
const MAX_DETAIL_BYTES: usize = 1024;

// -----------------------------------------------------------------------------------------
// WIT-shaped host types (eo9:svc), mirroring crates/eo9-runtime/src/link.rs
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:svc/detach.detach-impl` (stateless token).
pub struct SvcDetachCap;
/// Host representation of `eo9:svc/services.services-impl`.
pub struct SvcServicesCap;
/// Host representation of the text root handle a service writes its log through.
struct ServiceTextCap;

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)] // constructed by the generated Lift impl (values come from the guest)
enum WitLogPolicy {
    #[component(name = "capture")]
    Capture,
    #[component(name = "discard")]
    Discard,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
#[allow(dead_code)]
enum WitDetachError {
    #[component(name = "not-closed")]
    NotClosed(Vec<String>),
    #[component(name = "not-a-binary")]
    NotABinary,
    #[component(name = "name-taken")]
    NameTaken(String),
    #[component(name = "invalid-name")]
    InvalidName(String),
    #[component(name = "invalid-policy")]
    InvalidPolicy(String),
    #[component(name = "exhausted")]
    Exhausted,
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
#[allow(dead_code)]
enum WitServiceState {
    #[component(name = "running")]
    Running,
    #[component(name = "blocked")]
    Blocked,
    #[component(name = "waiting-restart")]
    WaitingRestart,
    #[component(name = "finished")]
    Finished,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitServiceInfo {
    name: String,
    state: WitServiceState,
    wiring: String,
    outcome: Option<String>,
    #[component(name = "fuel-used")]
    fuel_used: u64,
    restarts: u32,
}

// -----------------------------------------------------------------------------------------
// The log ring (the registry's one capability contribution to a service)
// -----------------------------------------------------------------------------------------

/// Bounded text capture shared between the service's text provider (inside its store)
/// and the registry (which serves `services.log` reads from it).
#[derive(Default)]
struct LogRing {
    bytes: Vec<u8>,
    /// Total bytes ever written, so `log` offsets stay stable as old bytes drop out.
    written: u64,
}

impl LogRing {
    fn push(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
        self.written += text.len() as u64;
        if self.bytes.len() > MAX_LOG_BYTES {
            let excess = self.bytes.len() - MAX_LOG_BYTES;
            self.bytes.drain(..excess);
        }
    }

    /// A window of the captured log; bytes already dropped from the ring read as absent.
    fn window(&self, offset: u64, max_len: u32) -> Vec<u8> {
        let start_of_ring = self.written - self.bytes.len() as u64;
        let begin = offset.max(start_of_ring) - start_of_ring;
        let begin = usize::try_from(begin)
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
        let end = begin.saturating_add(max_len as usize).min(self.bytes.len());
        self.bytes[begin..end].to_vec()
    }
}

/// The single-core spinlock from shellexec, reused for the service registry and rings.
use super::shellexec::KLock;

type SharedRing = Arc<KLock<LogRing>>;

// -----------------------------------------------------------------------------------------
// One service
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutcomeClass {
    Success,
    Failure,
    Trapped,
    Killed,
}

impl OutcomeClass {
    fn of(outcome: &KOutcome) -> Self {
        match outcome {
            KOutcome::Success { .. } => OutcomeClass::Success,
            KOutcome::Failure { .. } => OutcomeClass::Failure,
            KOutcome::Trapped(_) => OutcomeClass::Trapped,
            KOutcome::Killed => OutcomeClass::Killed,
        }
    }

    fn wit_name(self) -> &'static str {
        match self {
            OutcomeClass::Success => "success",
            OutcomeClass::Failure => "failure",
            OutcomeClass::Trapped => "trapped",
            OutcomeClass::Killed => "killed",
        }
    }
}

/// Render an outcome the way the shell renders them, so service records read the same
/// as foreground output (and the same as usermode service records).
fn render_service_outcome(outcome: &KOutcome) -> String {
    match outcome {
        KOutcome::Success { value, .. } => {
            if value.is_empty() {
                String::from("success")
            } else {
                format!("success({value})")
            }
        }
        KOutcome::Failure { value, .. } => {
            if value.is_empty() {
                String::from("failure")
            } else {
                format!("failure({value})")
            }
        }
        KOutcome::Trapped(reason) => format!("abnormal(trapped({reason}))"),
        KOutcome::Killed => String::from("abnormal(killed)"),
    }
}

struct FailureRecord {
    at_ms: u64,
    class: OutcomeClass,
    detail: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RestartAction {
    Restart,
    RestartAfterMs(u64),
    GiveUp,
}

enum SRun {
    /// Still executing: the drive future owns the service's store and the call to `main`.
    Running(Pin<Box<dyn Future<Output = KOutcome> + Send>>),
    /// Checked out by [`drive_services`] for one unlocked poll.
    Polling,
    /// A policy ordered a delayed restart; respawn when uptime reaches `until_ns`.
    WaitingRestart { until_ns: u64 },
    /// Finished (gave up, stopped, or failed to respawn). Record stays until `clear`.
    Finished,
}

struct KService {
    name: String,
    run: SRun,
    /// The engine the service's artifacts were compiled on (a wasmtime component can
    /// only instantiate on its own engine, so restarts and policy decisions reuse it).
    engine: wasmtime::Engine,
    /// The compiled program; restarts re-instantiate from it.
    component: Component,
    args: Vec<(String, String)>,
    /// The compiled restart policy (instantiated once per decision).
    policy: Component,
    wiring: String,
    log: SharedRing,
    capture: bool,
    entries: &'static [StoreEntry],
    history: Vec<FailureRecord>,
    restarts: u32,
    fuel_used: u64,
    last_outcome: Option<String>,
}

impl KService {
    fn state(&self) -> WitServiceState {
        match &self.run {
            // The kernel polls every running service each drive pass; "running" vs
            // "blocked" is reported from the last poll's runnable flag, tracked in
            // `fuel_used` heuristics — keep it simple and honest: a service that is
            // not finished and not waiting is "running" (the kernel has no per-service
            // park introspection; usermode's blocked/running split is approximated).
            SRun::Running(_) | SRun::Polling => WitServiceState::Running,
            SRun::WaitingRestart { .. } => WitServiceState::WaitingRestart,
            SRun::Finished => WitServiceState::Finished,
        }
    }

    fn info(&self) -> WitServiceInfo {
        WitServiceInfo {
            name: self.name.clone(),
            state: self.state(),
            wiring: self.wiring.clone(),
            outcome: self.last_outcome.clone(),
            fuel_used: self.fuel_used,
            restarts: self.restarts,
        }
    }
}

/// The machine's service registry. Static: services outlive the console session that
/// detached them; the lifetime is the boot (reset at boot, stopped at poweroff).
static SERVICES: KLock<Vec<Option<KService>>> = KLock::new(Vec::new());

/// Reset the registry (called once when the boot supervisor starts).
pub fn reset_services() {
    SERVICES.with(|services| services.clear());
}

fn truncated(mut text: String) -> String {
    if text.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// Same name rules as the shell's `save` and the usermode registry.
fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
}

/// The interfaces a detached composition may still (require-)import: the registry's log
/// capture satisfies text, and the rt riders are runtime contract everywhere. Identical
/// to the usermode rule.
fn import_allowed_for_service(interface: &str) -> bool {
    interface.starts_with("eo9:text/")
        || interface.starts_with("eo9:rt/")
        || interface.starts_with("eo9:io/")
}

/// The interfaces a *pure policy* may import: types and runtime-contract riders only.
fn import_allowed_for_policy(interface: &str) -> bool {
    interface.starts_with("eo9:rt/") || interface.starts_with("eo9:svc/types")
}

// -----------------------------------------------------------------------------------------
// The service execution environment (the short list, never the session's)
// -----------------------------------------------------------------------------------------

/// Build the linker a service instantiates against: text (the log ring), the rt
/// diagnostics rider, io buffers, and nothing else. No fs, no exec, no time, no
/// entropy, no pci, no svc — capability soundness is structural, exactly as in
/// usermode (`Service::providers` there builds from `Providers::none()`).
fn service_linker(
    engine: &wasmtime::Engine,
    ring: Option<SharedRing>,
) -> Result<Linker<KernelState>> {
    let mut linker: Linker<KernelState> = Linker::new(engine);

    // text/types: the root-handle resource.
    linker.instance("eo9:text/types@0.1.0")?.resource(
        "text-impl",
        ResourceType::host::<ServiceTextCap>(),
        |_, _| Ok(()),
    )?;

    // text/text: writes go to the (bounded) ring or nowhere; there is no input.
    let mut text = linker.instance("eo9:text/text@0.1.0")?;
    text.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<ServiceTextCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;
    text.func_wrap(
        "write",
        move |_store: StoreContextMut<'_, KernelState>,
              (_cap, _to, content): (
            Resource<ServiceTextCap>,
            super::providers::WitOutputStream,
            String,
        )|
              -> Result<(core::result::Result<(), super::providers::WitTextError>,)> {
            if let Some(ring) = &ring {
                ring.with(|ring| ring.push(&content));
            }
            Ok((Ok(()),))
        },
    )?;
    text.func_wrap_concurrent(
        "read-line",
        |_accessor: &Accessor<KernelState>,
         (_cap,): (Resource<ServiceTextCap>,)|
         -> ConcurrentFuture<
            '_,
            (core::result::Result<Option<String>, super::providers::WitTextError>,),
        > {
            // End of input, immediately: a service has no terminal to read from.
            Box::pin(async move { Ok((Ok(None),)) })
        },
    )?;

    // The rt riders: the panic-message sink (carries no authority).
    super::providers::add_diagnostics(&mut linker)?;

    // io buffers: authority-free data plumbing (the usermode allow-list includes io).
    super::shellfs::add_buffers(&mut linker)?;

    Ok(linker)
}

/// Instantiate one run of a service and return its drive future (the same shape as a
/// foreground child's, so the root drive loop pumps both identically).
fn spawn_service_run(
    engine: &wasmtime::Engine,
    entries: &'static [StoreEntry],
    component: &Component,
    args: &[(String, String)],
    ring: &SharedRing,
    capture: bool,
) -> core::result::Result<Pin<Box<dyn Future<Output = KOutcome> + Send>>, String> {
    let linker = service_linker(engine, capture.then(|| ring.clone()))
        .map_err(|err| format!("building the service environment failed: {err:?}"))?;

    // The service's store: a ShellState is present so the io buffer table works, but no
    // fs/exec/svc functions are registered on the linker, so nothing beyond buffers is
    // reachable through it.
    let mut state = KernelState::new();
    state.shell = Some(Box::new(super::shell::ShellState {
        fs: super::shellfs::ShellFs::new(entries, String::new()),
        buffers: super::shellfs::BufferTable::default(),
        exec: super::shellexec::ShellExec::default(),
        engine: engine.clone(),
    }));
    let mut store = Store::new(engine, state);

    // Instantiation on the bounded spawn budget (same regime as foreground children).
    store
        .set_fuel(shellexec::SPAWN_FUEL)
        .map_err(|err| format!("{err:?}"))?;
    let instance = bounded_block_on(linker.instantiate_async(&mut store, component))
        .ok_or_else(|| "service instantiation unexpectedly suspended".to_string())?
        .map_err(|err| format!("instantiating the service failed: {err:?}"))?;

    // Apply compose-time configuration (the bind entrypoint), if the artifact carries
    // it — a detached saved composition with baked configuration must behave exactly as
    // it would in the foreground. A refused configuration refuses the spawn.
    if let Some(bind) = super::bind_entrypoint(&instance, &mut store) {
        let mut bind_results = vec![Val::Bool(false); super::bind_result_slots(&bind, &store)];
        bounded_block_on(bind.call_async(&mut store, &[], &mut bind_results))
            .ok_or_else(|| "service configuration (`bind`) unexpectedly suspended".to_string())?
            .map_err(|err| format!("service configuration (`bind`) failed: {err:#}"))?;
        if let Some(refused) = super::configuration_refused(&bind_results) {
            return Err(format!("compose-time configuration refused: {refused}"));
        }
    }

    // The normal fuel regime: an effectively-infinite pool sliced by the yield quantum,
    // so a compute-bound service is preempted and the console stays responsive.
    store.set_fuel(u64::MAX).map_err(|err| format!("{err:?}"))?;
    store
        .fuel_async_yield_interval(Some(shellexec::FUEL_QUANTUM))
        .map_err(|err| format!("{err:?}"))?;

    let main = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| "the service does not export `main`".to_string())?;
    let signature = main.ty(&store);
    let wit_args: Vec<shellexec::WitNamedArg> = args
        .iter()
        .map(|(name, value)| shellexec::WitNamedArg {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    let params = shellexec::bind_args(&signature, &wit_args)?;
    let result_ty = signature.results().next();

    Ok(Box::pin(async move {
        let mut store = store;
        let mut results = vec![Val::Bool(false)];
        match main.call_async(&mut store, &params, &mut results).await {
            Ok(()) => shellexec::render_outcome(result_ty.as_ref(), results.first()),
            Err(err)
                if matches!(
                    err.downcast_ref::<wasmtime::Trap>(),
                    Some(wasmtime::Trap::OutOfFuel)
                ) =>
            {
                KOutcome::Killed
            }
            Err(err) => KOutcome::Trapped(match store.data().panic_message.as_deref() {
                Some(message) => format!("guest panicked: {message} — {err:?}"),
                None => format!("{err:?}"),
            }),
        }
    }))
}

/// Drive a short, non-suspending wasmtime future with a bounded poll loop (the spawn /
/// bind / policy paths — pure compute that must not park).
fn bounded_block_on<F: Future>(future: F) -> Option<F::Output> {
    let mut future = core::pin::pin!(future);
    let waker = Waker::from(Arc::new(NopWaker));
    let mut cx = Context::from_waker(&waker);
    for _ in 0..4096 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return Some(value);
        }
    }
    None
}

struct NopWaker;

impl alloc::task::Wake for NopWaker {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

/// Waker that records whether it was rung (a fuel yield rings it; a parked host future
/// does not), mirroring the child drive loop's runnable detection.
struct RungWaker {
    rung: AtomicBool,
}

impl alloc::task::Wake for RungWaker {
    fn wake(self: Arc<Self>) {
        self.rung.store(true, core::sync::atomic::Ordering::Release);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.rung.store(true, core::sync::atomic::Ordering::Release);
    }
}

// -----------------------------------------------------------------------------------------
// Restart-policy invocation (compiled at detach, instantiated per decision)
// -----------------------------------------------------------------------------------------

/// Instantiate the compiled policy and ask it what to do. Any problem — instantiate, a
/// trap, fuel exhaustion, an unparseable answer — is `give-up`, never an error.
fn consult_policy(
    engine: &wasmtime::Engine,
    policy: &Component,
    history: &[FailureRecord],
    total_restarts: u32,
) -> RestartAction {
    match try_consult_policy(engine, policy, history, total_restarts) {
        Ok(action) => action,
        Err(_) => RestartAction::GiveUp,
    }
}

fn try_consult_policy(
    engine: &wasmtime::Engine,
    policy: &Component,
    history: &[FailureRecord],
    total_restarts: u32,
) -> Result<RestartAction> {
    // The policy's world: the types-only svc vocabulary plus the rt riders. Nothing else
    // is registered — purity was enforced at detach, and the linker enforces it again.
    let mut linker: Linker<KernelState> = Linker::new(engine);
    let _ = linker.instance("eo9:svc/types@0.1.0")?;
    super::providers::add_diagnostics(&mut linker)?;

    let mut store = Store::new(engine, KernelState::new());
    store.set_fuel(POLICY_FUEL)?;

    let instance = bounded_block_on(linker.instantiate_async(&mut store, policy))
        .ok_or_else(|| wasmtime::Error::msg("policy instantiation did not complete"))??;

    // Configured policies (restart.backoff) carry the bind entrypoint: apply it.
    if let Some(bind) = super::bind_entrypoint(&instance, &mut store) {
        let mut bind_results = vec![Val::Bool(false); super::bind_result_slots(&bind, &store)];
        bounded_block_on(bind.call_async(&mut store, &[], &mut bind_results))
            .ok_or_else(|| wasmtime::Error::msg("policy configuration did not complete"))??;
        if let Some(refused) = super::configuration_refused(&bind_results) {
            return Err(wasmtime::Error::msg(format!(
                "the policy refused its configuration: {refused}"
            )));
        }
    }

    let policy_export = instance
        .get_export_index(&mut store, None, "eo9:svc/restart-policy@0.1.0")
        .ok_or_else(|| wasmtime::Error::msg("policy does not export eo9:svc/restart-policy"))?;
    let decide_index = instance
        .get_export_index(&mut store, Some(&policy_export), "decide")
        .ok_or_else(|| wasmtime::Error::msg("policy does not export decide"))?;
    let decide = instance
        .get_func(&mut store, decide_index)
        .ok_or_else(|| wasmtime::Error::msg("decide is not a function"))?;

    // Build the failure-history value.
    let records: Vec<Val> = history
        .iter()
        .map(|record| {
            Val::Record(vec![
                ("at-ms".to_string(), Val::U64(record.at_ms)),
                (
                    "class".to_string(),
                    Val::Enum(record.class.wit_name().to_string()),
                ),
                ("detail".to_string(), Val::String(record.detail.clone())),
            ])
        })
        .collect();
    let history_val = Val::Record(vec![
        ("failures".to_string(), Val::List(records)),
        ("total-restarts".to_string(), Val::U32(total_restarts)),
    ]);

    let params = [history_val];
    let mut results = vec![Val::Bool(false)];
    bounded_block_on(decide.call_async(&mut store, &params, &mut results))
        .ok_or_else(|| wasmtime::Error::msg("the policy call did not complete"))??;

    Ok(match &results[0] {
        Val::Variant(case, payload) => match case.as_str() {
            "restart" => RestartAction::Restart,
            "restart-after-ms" => match payload.as_deref() {
                Some(Val::U64(ms)) => RestartAction::RestartAfterMs(*ms),
                _ => RestartAction::GiveUp,
            },
            _ => RestartAction::GiveUp,
        },
        _ => RestartAction::GiveUp,
    })
}

// -----------------------------------------------------------------------------------------
// The pump (called from the root drive loop, beside drive_children)
// -----------------------------------------------------------------------------------------

/// What one registry index held when [`drive_services`] looked at it.
enum STaken {
    End,
    Skip,
    Run(Pin<Box<dyn Future<Output = KOutcome> + Send>>),
    Respawn,
}

/// Poll every running service once and start due restarts. The same checkout pattern as
/// `drive_children`: the slot is marked `Polling`, the future is polled with the lock
/// released, and the result is checked back in (a service never reaches the registry
/// from inside its own poll — it holds no svc — but the symmetry keeps the locking
/// story identical to children).
pub fn drive_services() -> DriveStatus {
    let mut status = DriveStatus::default();
    let now_ns = crate::timer::uptime_ns();
    let mut index = 0usize;
    loop {
        let taken = SERVICES.with(|services| {
            if index >= services.len() {
                return STaken::End;
            }
            match &mut services[index] {
                Some(service) => match &mut service.run {
                    SRun::Running(_) => match core::mem::replace(&mut service.run, SRun::Polling) {
                        SRun::Running(drive) => STaken::Run(drive),
                        _ => unreachable!("checked-out service was not running"),
                    },
                    SRun::WaitingRestart { until_ns } => {
                        if now_ns >= *until_ns {
                            STaken::Respawn
                        } else {
                            // Ask the idle loop to wake when the restart is due, so a
                            // delayed restart does not wait for the 1 s backstop.
                            super::request_timer_wake(*until_ns);
                            STaken::Skip
                        }
                    }
                    _ => STaken::Skip,
                },
                None => STaken::Skip,
            }
        });

        match taken {
            STaken::End => break,
            STaken::Skip => {
                index += 1;
                continue;
            }
            STaken::Respawn => {
                respawn(index);
                status.any_runnable = true;
                status.any_running = true;
                index += 1;
                continue;
            }
            STaken::Run(mut drive) => {
                let waker_state = Arc::new(RungWaker {
                    rung: AtomicBool::new(false),
                });
                let waker = Waker::from(waker_state.clone());
                let mut cx = Context::from_waker(&waker);
                let polled = drive.as_mut().poll(&mut cx);
                let rung = waker_state.rung.load(core::sync::atomic::Ordering::Acquire);

                let completed: Option<KOutcome> = SERVICES.with(|services| {
                    let Some(service) = services.get_mut(index).and_then(Option::as_mut) else {
                        return None; // cleared while polling (cannot happen today)
                    };
                    match &service.run {
                        SRun::Polling => match polled {
                            Poll::Ready(outcome) => {
                                // Recorded below, with the lock re-taken per phase.
                                Some(outcome)
                            }
                            Poll::Pending => {
                                if rung {
                                    service.fuel_used =
                                        service.fuel_used.saturating_add(shellexec::FUEL_QUANTUM);
                                }
                                service.run = SRun::Running(drive);
                                None
                            }
                        },
                        // Stopped while we were polling: keep that state; dropping the
                        // checked-out future here releases the service's store.
                        _ => None,
                    }
                });

                if let Some(outcome) = completed {
                    complete_run(index, outcome);
                    status.any_runnable = true;
                    status.any_running = true;
                } else {
                    let still_running = SERVICES.with(|services| {
                        matches!(
                            services.get(index).and_then(Option::as_ref).map(|s| &s.run),
                            Some(SRun::Running(_))
                        )
                    });
                    if still_running {
                        status.any_running = true;
                        if rung {
                            status.any_runnable = true;
                        }
                    }
                }
            }
        }
        index += 1;
    }
    status
}

/// A run completed: record it, consult the policy (unless killed), act.
fn complete_run(index: usize, outcome: KOutcome) {
    let class = OutcomeClass::of(&outcome);
    let rendered = truncated(render_service_outcome(&outcome));
    let at_ms = crate::timer::uptime_us() / 1000;

    // Phase 1: record the run.
    let policy_input = SERVICES.with(|services| {
        let service = services.get_mut(index).and_then(Option::as_mut)?;
        if !matches!(service.run, SRun::Polling) {
            return None; // stopped while completing
        }
        service.last_outcome = Some(rendered.clone());
        service.history.push(FailureRecord {
            at_ms,
            class,
            detail: rendered.clone(),
        });
        if service.history.len() > MAX_HISTORY {
            let excess = service.history.len() - MAX_HISTORY;
            service.history.drain(..excess);
        }
        service.run = SRun::Finished; // provisional; a restart replaces it below
        Some((
            service.engine.clone(),
            service.policy.clone(),
            service.restarts,
        ))
    });
    let Some((engine, policy, restarts)) = policy_input else {
        return;
    };

    // A killed run never consults the policy: stop means stop.
    let action = if class == OutcomeClass::Killed {
        RestartAction::GiveUp
    } else {
        // Consulting the policy runs guest code; the registry lock is *not* held (the
        // history is cloned out for the call).
        let history: Vec<FailureRecord> = SERVICES.with(|services| {
            services
                .get(index)
                .and_then(Option::as_ref)
                .map(|service| {
                    service
                        .history
                        .iter()
                        .map(|record| FailureRecord {
                            at_ms: record.at_ms,
                            class: record.class,
                            detail: record.detail.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        });
        consult_policy(&engine, &policy, &history, restarts)
    };

    match action {
        RestartAction::GiveUp => {}
        RestartAction::Restart => {
            SERVICES.with(|services| {
                if let Some(service) = services.get_mut(index).and_then(Option::as_mut) {
                    service.restarts += 1;
                }
            });
            respawn(index);
        }
        RestartAction::RestartAfterMs(delay_ms) => {
            let until_ns =
                crate::timer::uptime_ns().saturating_add(delay_ms.saturating_mul(1_000_000));
            SERVICES.with(|services| {
                if let Some(service) = services.get_mut(index).and_then(Option::as_mut) {
                    service.restarts += 1;
                    service.run = SRun::WaitingRestart { until_ns };
                }
            });
            super::request_timer_wake(until_ns);
        }
    }
}

/// Spawn a fresh run of service `index` from its stored image and arguments.
fn respawn(index: usize) {
    let setup = SERVICES.with(|services| {
        services.get(index).and_then(Option::as_ref).map(|service| {
            (
                service.engine.clone(),
                service.component.clone(),
                service.args.clone(),
                service.log.clone(),
                service.capture,
                service.entries,
            )
        })
    });
    let Some((engine, component, args, ring, capture, entries)) = setup else {
        return;
    };
    let spawned = spawn_service_run(&engine, entries, &component, &args, &ring, capture);
    SERVICES.with(|services| {
        if let Some(service) = services.get_mut(index).and_then(Option::as_mut) {
            match spawned {
                Ok(drive) => service.run = SRun::Running(drive),
                Err(reason) => {
                    service.last_outcome = Some(truncated(format!("restart failed: {reason}")));
                    service.run = SRun::Finished;
                }
            }
        }
    });
}

/// Stop everything still running and report it (the boot supervisor exited; the
/// registry's lifetime — the machine — is ending).
pub fn stop_all_and_report() {
    let alive: Vec<String> = SERVICES.with(|services| {
        services
            .iter_mut()
            .flatten()
            .filter(|service| !matches!(service.run, SRun::Finished))
            .map(|service| {
                service.run = SRun::Finished;
                service.last_outcome = Some(String::from("abnormal(killed)"));
                service.name.clone()
            })
            .collect()
    });
    if !alive.is_empty() {
        crate::kprintln!(
            "svc: stopping the service(s) still running (the registry lives until poweroff): {}",
            alive.join(", ")
        );
    }
}

// -----------------------------------------------------------------------------------------
// Linker registration (the eo9:svc host surface)
// -----------------------------------------------------------------------------------------

/// The not-granted refusal, mirroring the usermode trap text.
fn not_granted<T>() -> Result<T> {
    Err(wasmtime::Error::msg(
        "svc capability was not granted to this task",
    ))
}

/// Register the `eo9:svc` interfaces.
///
/// The same convention as usermode (plan/02 D27): the operations are registered
/// unconditionally — a client must import both the optional flavor (the honest signal)
/// and the full interface, and must instantiate everywhere — and the *grant* is the
/// store's `svc_generations` count: the optionals answer `some` only when it is
/// positive, and an operation called without the grant traps with the usermode message.
pub fn add_svc(linker: &mut Linker<KernelState>) -> Result<()> {
    // ----- detach ---------------------------------------------------------------------
    let mut detach = linker.instance("eo9:svc/detach@0.1.0")?;
    detach.resource(
        "detach-impl",
        ResourceType::host::<SvcDetachCap>(),
        |_, _| Ok(()),
    )?;
    detach.func_wrap(
        "default",
        |store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<SvcDetachCap>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((Resource::new_own(0),))
        },
    )?;
    detach.func_wrap(
        "detach",
        |mut store: StoreContextMut<'_, KernelState>,
         (_d, child, restart, name, args, logs): (
            Resource<SvcDetachCap>,
            Resource<shellexec::AlgComponentRes>,
            Resource<shellexec::AlgComponentRes>,
            String,
            Vec<shellexec::WitNamedArg>,
            WitLogPolicy,
        )|
         -> Result<(core::result::Result<String, WitDetachError>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((host_detach(&mut store, child, restart, name, args, logs),))
        },
    )?;

    let mut detach_optional = linker.instance("eo9:svc/detach-optional@0.1.0")?;
    detach_optional.func_wrap(
        "default",
        |store: StoreContextMut<'_, KernelState>,
         (): ()|
         -> Result<(Option<Resource<SvcDetachCap>>,)> {
            Ok(((store.data().svc_generations > 0).then(|| Resource::new_own(0)),))
        },
    )?;

    // ----- services -------------------------------------------------------------------
    let mut services = linker.instance("eo9:svc/services@0.1.0")?;
    services.resource(
        "services-impl",
        ResourceType::host::<SvcServicesCap>(),
        |_, _| Ok(()),
    )?;
    services.func_wrap(
        "default",
        |store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<SvcServicesCap>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((Resource::new_own(0),))
        },
    )?;
    services.func_wrap(
        "list",
        |store: StoreContextMut<'_, KernelState>,
         (_s,): (Resource<SvcServicesCap>,)|
         -> Result<(Vec<WitServiceInfo>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((SERVICES.with(|services| {
                services
                    .iter()
                    .flatten()
                    .map(KService::info)
                    .collect::<Vec<_>>()
            }),))
        },
    )?;
    services.func_wrap(
        "status",
        |store: StoreContextMut<'_, KernelState>,
         (_s, name): (Resource<SvcServicesCap>, String)|
         -> Result<(Option<WitServiceInfo>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((SERVICES.with(|services| {
                services
                    .iter()
                    .flatten()
                    .find(|service| service.name == name)
                    .map(KService::info)
            }),))
        },
    )?;
    services.func_wrap(
        "log",
        |store: StoreContextMut<'_, KernelState>,
         (_s, name, offset, max_len): (Resource<SvcServicesCap>, String, u64, u32)|
         -> Result<(Option<Vec<u8>>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            let ring = SERVICES.with(|services| {
                services
                    .iter()
                    .flatten()
                    .find(|service| service.name == name)
                    .filter(|service| service.capture)
                    .map(|service| service.log.clone())
            });
            Ok((ring.map(|ring| ring.with(|ring| ring.window(offset, max_len))),))
        },
    )?;
    services.func_wrap(
        "stop",
        |store: StoreContextMut<'_, KernelState>,
         (_s, name): (Resource<SvcServicesCap>, String)|
         -> Result<(Option<String>,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((host_stop(&name),))
        },
    )?;
    services.func_wrap(
        "clear",
        |store: StoreContextMut<'_, KernelState>,
         (_s, name): (Resource<SvcServicesCap>, String)|
         -> Result<(bool,)> {
            if store.data().svc_generations == 0 {
                return not_granted();
            }
            Ok((SERVICES.with(|services| {
                let index = services
                    .iter()
                    .position(|slot| matches!(slot, Some(s) if s.name == name));
                match index {
                    Some(index)
                        if matches!(
                            services[index].as_ref().map(|s| &s.run),
                            Some(SRun::Finished)
                        ) =>
                    {
                        services[index] = None;
                        true
                    }
                    _ => false,
                }
            }),))
        },
    )?;

    let mut services_optional = linker.instance("eo9:svc/services-optional@0.1.0")?;
    services_optional.func_wrap(
        "default",
        |store: StoreContextMut<'_, KernelState>,
         (): ()|
         -> Result<(Option<Resource<SvcServicesCap>>,)> {
            Ok(((store.data().svc_generations > 0).then(|| Resource::new_own(0)),))
        },
    )?;

    Ok(())
}

/// Kill a running service (or cancel its pending restart); the rendered final outcome.
fn host_stop(name: &str) -> Option<String> {
    SERVICES.with(|services| {
        let service = services
            .iter_mut()
            .flatten()
            .find(|service| service.name == name)?;
        let rendered = match &service.run {
            // Setting the slot away from Running drops the drive future (and with it the
            // service's store and in-flight work) — for a service currently checked out,
            // that drop happens when its poll returns and sees the state changed.
            SRun::Running(_) | SRun::Polling => String::from("abnormal(killed)"),
            SRun::WaitingRestart { .. } => {
                String::from("abnormal: killed (pending restart cancelled)")
            }
            SRun::Finished => service
                .last_outcome
                .clone()
                .unwrap_or_else(|| String::from("finished")),
        };
        service.run = SRun::Finished;
        service.last_outcome = Some(rendered.clone());
        Some(rendered)
    })
}

/// The detach operation: validate, compile, spawn the first run, register.
fn host_detach(
    store: &mut StoreContextMut<'_, KernelState>,
    child: Resource<shellexec::AlgComponentRes>,
    restart: Resource<shellexec::AlgComponentRes>,
    name: String,
    args: Vec<shellexec::WitNamedArg>,
    logs: WitLogPolicy,
) -> core::result::Result<String, WitDetachError> {
    let internal = |msg: String| WitDetachError::Internal(msg);

    // --- name / capacity ------------------------------------------------------------
    if !valid_service_name(&name) {
        return Err(WitDetachError::InvalidName(name));
    }
    let (taken, count) = SERVICES.with(|services| {
        (
            services
                .iter()
                .flatten()
                .any(|service| service.name == name),
            services.iter().flatten().count(),
        )
    });
    if taken {
        return Err(WitDetachError::NameTaken(name));
    }
    if count >= MAX_SERVICES {
        return Err(WitDetachError::Exhausted);
    }

    // --- the operands (consumed from the caller's exec table, as in usermode) --------
    let entries = store
        .data_mut()
        .shell_entries()
        .map_err(|err| internal(format!("{err}")))?;
    let engine = store
        .data_mut()
        .shell_engine()
        .map_err(|err| internal(format!("{err}")))?;
    let (child_kc, policy_kc) = {
        let exec = store
            .data_mut()
            .shell_exec()
            .map_err(|err| internal(format!("{err}")))?;
        let child = exec
            .take_component(child.rep())
            .map_err(|err| internal(format!("{err}")))?;
        let policy = exec
            .take_component(restart.rep())
            .map_err(|err| internal(format!("{err}")))?;
        (child, policy)
    };

    // --- the child: a binary, closed except for what the registry supplies ----------
    let child_info = shellexec::component_info(entries, &child_kc)
        .map_err(|err| internal(format!("describing the service failed: {err}")))?;
    if child_info.kind != shellexec::WitComponentKind::Binary {
        return Err(WitDetachError::NotABinary);
    }
    let unsupplied: Vec<String> = child_info
        .imports
        .iter()
        .filter(|need| need.required && !import_allowed_for_service(&need.interface))
        .map(|need| need.interface.clone())
        .collect();
    if !unsupplied.is_empty() {
        return Err(WitDetachError::NotClosed(unsupplied));
    }

    // --- the restart policy: pure, and actually a restart policy ---------------------
    let policy_info = shellexec::component_info(entries, &policy_kc)
        .map_err(|err| internal(format!("describing the policy failed: {err}")))?;
    if policy_info.kind != shellexec::WitComponentKind::Provider {
        return Err(WitDetachError::InvalidPolicy(
            "the restart policy must be a provider exporting eo9:svc/restart-policy \
             (a binary cannot be a policy)"
                .to_string(),
        ));
    }
    if !policy_info
        .exports
        .iter()
        .any(|export| export.interface.starts_with("eo9:svc/restart-policy"))
    {
        return Err(WitDetachError::InvalidPolicy(format!(
            "the policy component does not export eo9:svc/restart-policy (it exports: {})",
            policy_info
                .exports
                .iter()
                .map(|e| e.interface.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let impure: Vec<String> = policy_info
        .imports
        .iter()
        .filter(|need| need.required && !import_allowed_for_policy(&need.interface))
        .map(|need| need.interface.clone())
        .collect();
    if !impure.is_empty() {
        return Err(WitDetachError::InvalidPolicy(format!(
            "restart policies must be pure (import nothing), but this one requires: {} — \
             purity is what lets the registry trust a policy's answer",
            impure.join(", ")
        )));
    }

    // --- compile both, once (restarts and decisions reuse the artifacts) -------------
    let wiring = shellexec::component_wiring(&child_kc);
    let component = shellexec::compile_component(&engine, entries, &child_kc)
        .map_err(|err| internal(format!("compiling the service failed: {err}")))?;
    let policy = shellexec::compile_component(&engine, entries, &policy_kc)
        .map_err(|err| internal(format!("compiling the policy failed: {err}")))?;

    // --- spawn the first run ----------------------------------------------------------
    let capture = matches!(logs, WitLogPolicy::Capture);
    let ring: SharedRing = Arc::new(KLock::new(LogRing::default()));
    let args: Vec<(String, String)> = args.into_iter().map(|arg| (arg.name, arg.value)).collect();
    let drive = spawn_service_run(&engine, entries, &component, &args, &ring, capture)
        .map_err(|err| internal(format!("spawning the service failed: {err}")))?;

    let service = KService {
        name: name.clone(),
        run: SRun::Running(drive),
        engine,
        component,
        args,
        policy,
        wiring,
        log: ring,
        capture,
        entries,
        history: Vec::new(),
        restarts: 0,
        fuel_used: 0,
        last_outcome: None,
    };
    SERVICES.with(|services| match services.iter().position(Option::is_none) {
        Some(index) => services[index] = Some(service),
        None => services.push(Some(service)),
    });
    Ok(name)
}
