//! The service registry: host-side state behind the `eo9:svc/*` interfaces.
//!
//! This is stage A of the executor model (docs/design/executor-model.md): the registry —
//! the name table, the per-service log rings, restart-policy application, and the pump
//! that gives detached services CPU — is host code, exactly the way `eo9:exec/task` is.
//! `init`, `eosh`, and every other client are ordinary, unprivileged guests of the API;
//! a later supervisor program (stage B, post-Message-API) takes the policy half over
//! without changing the WIT.
//!
//! Lifetime is the embedder's choice (owner ruling E): the registry lives as long as the
//! [`SharedRegistry`] handle the embedder holds. The usermode CLI binds it to the `eo9`
//! process; the kernel (v2) will bind it to the machine.
//!
//! # Capability soundness
//!
//! A detached child runs with **exactly what its detacher composed into it**, plus the
//! registry's log-capture text provider and the `eo9:rt/*` runtime-contract riders —
//! never the registry's own authority. [`ServiceRegistry::detach`] enforces this by
//! refusing (typed `not-closed`) any composition whose residual *required* imports fall
//! outside that short list, so handing a child off can never escalate it beyond what the
//! detacher could have run in the foreground.
//!
//! # Restart policies are programs
//!
//! The restart policy is itself a component (SPEC "Policies are programs", owner ruling
//! C): a pure provider exporting `eo9:svc/restart-policy`. The registry validates it at
//! detach time (provider, exports the interface, imports nothing but types and rt
//! riders) and instantiates it once per decision — the cold-path binding from
//! docs/design/policy-components.md. A policy that traps, exceeds its fuel budget, or
//! returns garbage is treated as `give-up`: a broken policy can never wedge the
//! registry or restart-loop a service forever.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use wasmtime::component::{Linker, Val};
use wasmtime::{Engine, Store};

use crate::image::Image;
use crate::link;
use crate::outcome::Outcome;
use crate::providers::{BoxOp, OutputStream, Providers, TextError, TextProvider};
use crate::task::{NamedArg, ResumeOutcome, SpawnLimits, Task};

/// How many services one registry may hold (running or finished-but-not-cleared).
pub const MAX_SERVICES: usize = 16;
/// Ceiling on one service's captured log (bytes); older output is dropped first.
pub const MAX_LOG_BYTES: usize = 256 * 1024;
/// Ceiling on the failure history kept per service (records); older runs are dropped
/// first, but `total-restarts` keeps counting.
pub const MAX_HISTORY: usize = 64;
/// Fuel budget for one restart-policy `decide` call (instantiation + the call). Policies
/// are bounded by construction: exhausting this is treated as `give-up`.
pub const POLICY_FUEL: u64 = 50_000_000;
/// Ceiling on the rendered outcome / policy error strings kept per service.
const MAX_DETAIL_BYTES: usize = 1024;

// ---------------------------------------------------------------------------------------
// Public vocabulary (mirrors eo9:svc/types)
// ---------------------------------------------------------------------------------------

/// How a service's terminal output is kept (mirrors `eo9:svc/detach.log-policy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogPolicy {
    /// Keep a bounded ring readable through `services.log`.
    Capture,
    /// Discard output.
    Discard,
}

/// How a completed run ended (mirrors `eo9:svc/types.outcome-class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeClass {
    Success,
    Failure,
    Trapped,
    Killed,
}

impl OutcomeClass {
    fn of(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Success(_) => OutcomeClass::Success,
            Outcome::Failure(_) => OutcomeClass::Failure,
            Outcome::Trapped(_) => OutcomeClass::Trapped,
            Outcome::Killed => OutcomeClass::Killed,
        }
    }

    /// The WIT enum case name.
    fn wit_name(self) -> &'static str {
        match self {
            OutcomeClass::Success => "success",
            OutcomeClass::Failure => "failure",
            OutcomeClass::Trapped => "trapped",
            OutcomeClass::Killed => "killed",
        }
    }
}

/// Render an outcome the way the shell renders them (`success(…)` / `failure(…)` /
/// `abnormal(…)`), so service records read the same as foreground output.
fn render_outcome(outcome: &Outcome) -> String {
    let arm = |name: &str, value: &crate::outcome::WaveValue| {
        if value.value.is_empty() {
            name.to_string()
        } else {
            format!("{name}({})", value.value)
        }
    };
    match outcome {
        Outcome::Success(value) => arm("success", value),
        Outcome::Failure(value) => arm("failure", value),
        Outcome::Trapped(reason) => format!("abnormal(trapped({reason}))"),
        Outcome::Killed => "abnormal(killed)".to_string(),
    }
}

/// One completed run of a service (mirrors `eo9:svc/types.failure-record`).
#[derive(Clone, Debug)]
pub struct FailureRecord {
    pub at_ms: u64,
    pub class: OutcomeClass,
    pub detail: String,
}

/// What a restart policy ordered (mirrors `eo9:svc/types.restart-action`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartAction {
    Restart,
    RestartAfterMs(u64),
    GiveUp,
}

/// Why a detach was refused (mirrors `eo9:svc/detach.detach-error`).
#[derive(Debug)]
pub enum DetachError {
    /// Residual required imports the registry will not supply, named.
    NotClosed(Vec<String>),
    NotABinary,
    NameTaken(String),
    InvalidName(String),
    InvalidPolicy(String),
    Exhausted,
    Internal(String),
}

impl std::fmt::Display for DetachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetachError::NotClosed(needs) => write!(
                f,
                "the composition still requires {} — a detached service runs with exactly \
                 what its detacher composed (plus log capture); compose those capabilities \
                 in before detaching",
                needs.join(", ")
            ),
            DetachError::NotABinary => {
                write!(
                    f,
                    "the detached child is a provider, not a runnable program"
                )
            }
            DetachError::NameTaken(name) => write!(f, "a service named `{name}` already exists"),
            DetachError::InvalidName(name) => write!(
                f,
                "`{name}` is not a usable service name (letters, digits, `-`, `_`, and \
                 interior `.` only)"
            ),
            DetachError::InvalidPolicy(reason) => write!(f, "invalid restart policy: {reason}"),
            DetachError::Exhausted => write!(
                f,
                "the service registry is full ({MAX_SERVICES} services); `clear` finished \
                 ones or `stop` running ones first"
            ),
            DetachError::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

/// One service's externally visible state (mirrors `eo9:svc/services.service-state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Running,
    Blocked,
    WaitingRestart,
    Finished,
}

impl ServiceState {
    pub fn wit_name(self) -> &'static str {
        match self {
            ServiceState::Running => "running",
            ServiceState::Blocked => "blocked",
            ServiceState::WaitingRestart => "waiting-restart",
            ServiceState::Finished => "finished",
        }
    }
}

/// One row of `services.list` (mirrors `eo9:svc/services.service-info`).
#[derive(Clone, Debug)]
pub struct ServiceInfo {
    pub name: String,
    pub state: ServiceState,
    pub wiring: String,
    pub outcome: Option<String>,
    pub fuel_used: u64,
    pub restarts: u32,
}

// ---------------------------------------------------------------------------------------
// The log ring: the registry's one capability contribution to a service
// ---------------------------------------------------------------------------------------

/// Bounded text capture shared between the service's text provider (inside its store)
/// and the registry (which serves `services.log` reads from it).
#[derive(Default)]
struct LogRing {
    bytes: Vec<u8>,
    /// Total bytes ever written (so `log` offsets stay stable as old bytes are dropped).
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

    /// A window of the captured log. `offset` is an absolute write offset; bytes that
    /// have already been dropped from the ring read as absent (the window starts at the
    /// oldest retained byte).
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

/// The text provider a detached service runs against: writes go to the (bounded) ring,
/// or nowhere; there is no input (services have no terminal).
struct ServiceText {
    ring: Option<Arc<Mutex<LogRing>>>,
}

impl TextProvider for ServiceText {
    fn write(&mut self, _to: OutputStream, text: &str) -> Result<(), TextError> {
        if let Some(ring) = &self.ring {
            ring.lock().unwrap().push(text);
        }
        Ok(())
    }

    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>> {
        // End of input, immediately: a service has no terminal to read from.
        Box::pin(std::future::ready(Ok(None)))
    }
}

// ---------------------------------------------------------------------------------------
// One service
// ---------------------------------------------------------------------------------------

enum RunState {
    Running(Task),
    WaitingRestart { until: Instant },
    Finished,
}

struct Service {
    name: String,
    run: RunState,
    /// The compiled image; restarts re-spawn from it (an image spawns any number of times).
    image: Image,
    args: Vec<NamedArg>,
    /// The validated restart-policy component (instantiated once per decision).
    policy: eo9_component::Component,
    wiring: String,
    log: Arc<Mutex<LogRing>>,
    log_policy: LogPolicy,
    history: Vec<FailureRecord>,
    restarts: u32,
    fuel_used: u64,
    /// The rendered outcome of the last completed run.
    last_outcome: Option<String>,
}

impl Service {
    fn state(&self) -> ServiceState {
        match &self.run {
            RunState::Running(task) => {
                if task.is_runnable() {
                    ServiceState::Running
                } else {
                    ServiceState::Blocked
                }
            }
            RunState::WaitingRestart { .. } => ServiceState::WaitingRestart,
            RunState::Finished => ServiceState::Finished,
        }
    }

    fn info(&self) -> ServiceInfo {
        ServiceInfo {
            name: self.name.clone(),
            state: self.state(),
            wiring: self.wiring.clone(),
            outcome: self.last_outcome.clone(),
            fuel_used: self.fuel_used,
            restarts: self.restarts,
        }
    }

    fn providers(&self) -> Providers {
        let ring = match self.log_policy {
            LogPolicy::Capture => Some(self.log.clone()),
            LogPolicy::Discard => None,
        };
        Providers {
            text: Some(Box::new(ServiceText { ring })),
            ..Providers::none()
        }
    }
}

// ---------------------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------------------

/// The service registry. Embedders hold it behind [`SharedRegistry`] and pump it from
/// their root drive loop; guests reach it through the `eo9:svc` host functions.
pub struct ServiceRegistry {
    engine: Engine,
    services: Vec<Service>,
    epoch: Instant,
}

/// The embedder's (and the linker's) handle to one registry.
pub type SharedRegistry = Arc<Mutex<ServiceRegistry>>;

/// What granting `eo9:svc` to a task means: which registry, and which halves.
/// (`detach` = may start things that outlive you; `services` = may inspect/stop them.)
pub struct SvcGrant {
    pub registry: SharedRegistry,
    pub detach: bool,
    pub services: bool,
}

impl SvcGrant {
    /// Both halves of the capability against one registry (the common grant).
    pub fn full(registry: SharedRegistry) -> Self {
        SvcGrant {
            registry,
            detach: true,
            services: true,
        }
    }
}

/// The interfaces a detached composition may still (require-)import: the registry's log
/// capture satisfies text, and the rt riders are runtime contract on every target.
fn import_allowed_for_service(interface: &str) -> bool {
    interface.starts_with("eo9:text/")
        || interface.starts_with("eo9:rt/")
        || interface.starts_with("eo9:io/")
}

/// The interfaces a *pure policy* may import: types and runtime-contract riders only.
fn import_allowed_for_policy(interface: &str) -> bool {
    interface.starts_with("eo9:rt/") || interface == "eo9:svc/types"
}

/// Same name rules as the shell's `save`: letters, digits, `-`, `_`, interior `.`.
fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
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

impl ServiceRegistry {
    /// A new, empty registry compiling and running services on `engine`.
    pub fn new(engine: &Engine) -> SharedRegistry {
        Arc::new(Mutex::new(ServiceRegistry {
            engine: engine.clone(),
            services: Vec::new(),
            epoch: Instant::now(),
        }))
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.name == name)
    }

    /// Hand a composed child off to run under this registry.
    ///
    /// Validates the name, the child (a binary, closed except for text/rt/io), and the
    /// restart policy (a provider exporting `eo9:svc/restart-policy`, importing nothing
    /// but types and rt riders), compiles the child, spawns its first run, and registers
    /// it. The wiring tree is recorded for inspection before the component is consumed.
    pub fn detach(
        &mut self,
        child: eo9_component::Component,
        policy: eo9_component::Component,
        name: &str,
        args: Vec<NamedArg>,
        logs: LogPolicy,
    ) -> Result<String, DetachError> {
        if !valid_service_name(name) {
            return Err(DetachError::InvalidName(name.to_string()));
        }
        if self.find(name).is_some() {
            return Err(DetachError::NameTaken(name.to_string()));
        }
        if self.services.len() >= MAX_SERVICES {
            return Err(DetachError::Exhausted);
        }

        // --- the child: a binary, closed except for what the registry supplies ---------
        let info = child.describe();
        if info.kind != eo9_component::ComponentKind::Binary {
            return Err(DetachError::NotABinary);
        }
        let unsupplied: Vec<String> = info
            .imports
            .iter()
            .filter(|need| need.required && !import_allowed_for_service(&need.interface))
            .map(|need| need.interface.clone())
            .collect();
        if !unsupplied.is_empty() {
            return Err(DetachError::NotClosed(unsupplied));
        }

        // --- the restart policy: pure, and actually a restart policy -------------------
        let policy_info = policy.describe();
        if policy_info.kind != eo9_component::ComponentKind::Provider {
            return Err(DetachError::InvalidPolicy(
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
            return Err(DetachError::InvalidPolicy(format!(
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
            return Err(DetachError::InvalidPolicy(format!(
                "restart policies must be pure (import nothing), but this one requires: {} — \
                 purity is what lets the registry trust a policy's answer",
                impure.join(", ")
            )));
        }

        // --- compile and spawn the first run -------------------------------------------
        let wiring = child.wiring_tree();
        let image = Image::compile(&self.engine, child.executable_bytes())
            .map_err(|err| DetachError::Internal(format!("compiling the service failed: {err}")))?;

        let log = Arc::new(Mutex::new(LogRing::default()));
        let mut service = Service {
            name: name.to_string(),
            run: RunState::Finished, // replaced below
            image,
            args,
            policy,
            wiring,
            log,
            log_policy: logs,
            history: Vec::new(),
            restarts: 0,
            fuel_used: 0,
            last_outcome: None,
        };
        let task = Task::spawn(
            &service.image,
            &service.args,
            SpawnLimits::default(),
            service.providers(),
        )
        .map_err(|err| DetachError::Internal(format!("spawning the service failed: {err}")))?;
        service.run = RunState::Running(task);

        self.services.push(service);
        Ok(name.to_string())
    }

    /// Give every runnable service one fuel slice; complete runs, consult restart
    /// policies, start due restarts. Returns `true` when any service made progress
    /// (ran, finished, or restarted) — the embedder's drive loop uses this to decide
    /// whether to park.
    pub fn pump(&mut self, fuel_per_service: u64) -> bool {
        let mut progressed = false;
        let now_ms = self.elapsed_ms();
        // Indexed loop: consult_policy needs &self while we hold a service entry, so the
        // mutation is done in two phases per service.
        for index in 0..self.services.len() {
            // Phase 1: run / check timers, recording a completed outcome if any.
            let completed: Option<Outcome> = {
                let service = &mut self.services[index];
                match &mut service.run {
                    RunState::Running(task) => {
                        if !task.is_runnable() && task.outcome().is_none() {
                            None // parked on I/O (or on nothing — services have no inputs)
                        } else {
                            service.fuel_used = service.fuel_used.saturating_add(fuel_per_service);
                            match task.resume(fuel_per_service) {
                                ResumeOutcome::Done(outcome) => {
                                    progressed = true;
                                    Some(outcome)
                                }
                                ResumeOutcome::OutOfFuel => {
                                    progressed = true;
                                    None
                                }
                                ResumeOutcome::Blocked => None,
                            }
                        }
                    }
                    RunState::WaitingRestart { until } => {
                        if Instant::now() >= *until {
                            // The delay elapsed: respawn.
                            progressed = true;
                            let task = Task::spawn(
                                &service.image,
                                &service.args,
                                SpawnLimits::default(),
                                service.providers(),
                            );
                            match task {
                                Ok(task) => {
                                    service.run = RunState::Running(task);
                                }
                                Err(err) => {
                                    service.last_outcome =
                                        Some(truncated(format!("restart failed: {err}")));
                                    service.run = RunState::Finished;
                                }
                            }
                        }
                        None
                    }
                    RunState::Finished => None,
                }
            };

            // Phase 2: a run completed — record it and ask the policy what to do.
            if let Some(outcome) = completed {
                let class = OutcomeClass::of(&outcome);
                let rendered = truncated(render_outcome(&outcome));
                {
                    let service = &mut self.services[index];
                    service.last_outcome = Some(rendered.clone());
                    service.history.push(FailureRecord {
                        at_ms: now_ms,
                        class,
                        detail: rendered,
                    });
                    if service.history.len() > MAX_HISTORY {
                        let excess = service.history.len() - MAX_HISTORY;
                        service.history.drain(..excess);
                    }
                }

                // A killed run never consults the policy: `stop` means stop.
                let action = if class == OutcomeClass::Killed {
                    RestartAction::GiveUp
                } else {
                    let service = &self.services[index];
                    consult_policy(
                        &self.engine,
                        &service.policy,
                        &service.history,
                        service.restarts,
                    )
                };

                let service = &mut self.services[index];
                match action {
                    RestartAction::Restart => {
                        service.restarts += 1;
                        match Task::spawn(
                            &service.image,
                            &service.args,
                            SpawnLimits::default(),
                            service.providers(),
                        ) {
                            Ok(task) => service.run = RunState::Running(task),
                            Err(err) => {
                                service.last_outcome =
                                    Some(truncated(format!("restart failed: {err}")));
                                service.run = RunState::Finished;
                            }
                        }
                    }
                    RestartAction::RestartAfterMs(delay_ms) => {
                        service.restarts += 1;
                        service.run = RunState::WaitingRestart {
                            until: Instant::now() + std::time::Duration::from_millis(delay_ms),
                        };
                    }
                    RestartAction::GiveUp => {
                        service.run = RunState::Finished;
                    }
                }
                progressed = true;
            }
        }
        progressed
    }

    /// True when any service can use CPU right now (runnable, or a due restart).
    pub fn any_runnable(&self) -> bool {
        let now = Instant::now();
        self.services.iter().any(|service| match &service.run {
            RunState::Running(task) => task.is_runnable(),
            RunState::WaitingRestart { until } => now >= *until,
            RunState::Finished => false,
        })
    }

    /// True when any service is still alive (running, blocked, or waiting on a restart).
    pub fn any_alive(&self) -> bool {
        self.services
            .iter()
            .any(|service| !matches!(service.run, RunState::Finished))
    }

    pub fn list(&self) -> Vec<ServiceInfo> {
        self.services.iter().map(Service::info).collect()
    }

    pub fn status(&self, name: &str) -> Option<ServiceInfo> {
        self.find(name).map(|index| self.services[index].info())
    }

    /// A window of a service's captured log; `None` for unknown services or discarded logs.
    pub fn log(&self, name: &str, offset: u64, max_len: u32) -> Option<Vec<u8>> {
        let index = self.find(name)?;
        let service = &self.services[index];
        if service.log_policy == LogPolicy::Discard {
            return None;
        }
        Some(service.log.lock().unwrap().window(offset, max_len))
    }

    /// Kill a running service (or cancel its pending restart); returns the rendered
    /// final outcome. `None` for unknown services.
    pub fn stop(&mut self, name: &str) -> Option<String> {
        let index = self.find(name)?;
        let service = &mut self.services[index];
        let rendered = match &mut service.run {
            RunState::Running(task) => {
                let outcome = task.kill_in_place();
                truncated(render_outcome(&outcome))
            }
            RunState::WaitingRestart { .. } => {
                "abnormal: killed (pending restart cancelled)".to_string()
            }
            RunState::Finished => service
                .last_outcome
                .clone()
                .unwrap_or_else(|| "finished".to_string()),
        };
        service.run = RunState::Finished;
        service.last_outcome = Some(rendered.clone());
        Some(rendered)
    }

    /// Remove a finished service's record. False for unknown or still-alive services.
    pub fn clear(&mut self, name: &str) -> bool {
        match self.find(name) {
            Some(index) if matches!(self.services[index].run, RunState::Finished) => {
                self.services.remove(index);
                true
            }
            _ => false,
        }
    }

    /// Stop everything (the registry is shutting down with its embedder).
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.services.iter().map(|s| s.name.clone()).collect();
        for name in names {
            self.stop(&name);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Restart-policy invocation (the cold-path policy-component binding)
// ---------------------------------------------------------------------------------------

/// Instantiate `policy` and ask it what to do about `history`.
///
/// Failure policy: any problem — compile, instantiate, a trap, fuel exhaustion, an
/// unparseable answer — is `give-up` (never an error): a broken policy must not wedge
/// the registry or keep a service restarting.
fn consult_policy(
    engine: &Engine,
    policy: &eo9_component::Component,
    history: &[FailureRecord],
    total_restarts: u32,
) -> RestartAction {
    match try_consult_policy(engine, policy, history, total_restarts) {
        Ok(action) => action,
        Err(_) => RestartAction::GiveUp,
    }
}

fn try_consult_policy(
    engine: &Engine,
    policy: &eo9_component::Component,
    history: &[FailureRecord],
    total_restarts: u32,
) -> wasmtime::Result<RestartAction> {
    // Compile the policy. Policies are tiny; v1 recompiles per decision (a restart is a
    // rare event). The executable form strips algebra annotations exactly as the exec
    // compile path does.
    let compiled = wasmtime::component::Component::new(engine, policy.executable_bytes())?;

    // The policy's world: types-only svc imports plus the rt riders. `Providers::none()`
    // wires exactly that — every optional interface absent, diagnostics present.
    let providers = Providers::none();
    let mut linker: Linker<crate::task::TaskState> = Linker::new(engine);
    link::add_providers(&mut linker, &providers)?;
    let mut store: Store<crate::task::TaskState> =
        Store::new(engine, crate::task::TaskState::bare(providers));
    store.set_fuel(POLICY_FUEL)?;

    // Policy calls are pure compute under a plain fuel budget (no async yield interval),
    // so polls only return Pending across genuine completion boundaries; a no-op waker
    // plus a bounded poll loop is all that is needed.
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);

    // Instantiate (bounded poll loop; pure compute completes promptly).
    let instance = {
        let instantiate = linker.instantiate_async(&mut store, &compiled);
        let mut instantiate = std::pin::pin!(instantiate);
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
        result.ok_or_else(|| wasmtime::Error::msg("policy instantiation did not complete"))??
    };

    // Configured policies (restart.backoff) carry the bind entrypoint: apply it.
    if let Some(bind) = crate::task::bind_entrypoint(&instance, &mut store) {
        let mut results = vec![Val::Bool(false); bind.ty(&store).results().len()];
        {
            let call = bind.call_async(&mut store, &[], &mut results);
            let mut call = std::pin::pin!(call);
            let mut done = None;
            for _ in 0..4096 {
                match call.as_mut().poll(&mut cx) {
                    Poll::Ready(r) => {
                        done = Some(r);
                        break;
                    }
                    Poll::Pending => continue,
                }
            }
            done.ok_or_else(|| wasmtime::Error::msg("policy configuration did not complete"))??;
        }
        if let Some(refused) = crate::task::configuration_refused(&results) {
            return Err(wasmtime::Error::msg(format!(
                "the policy refused its configuration: {refused}"
            )));
        }
    }

    // Find the exported decide function.
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

    // Call it.
    let params = [history_val];
    let mut results = vec![Val::Bool(false)];
    {
        let call = decide.call_async(&mut store, &params, &mut results);
        let mut call = std::pin::pin!(call);
        let mut done = None;
        for _ in 0..4096 {
            match call.as_mut().poll(&mut cx) {
                Poll::Ready(r) => {
                    done = Some(r);
                    break;
                }
                Poll::Pending => continue,
            }
        }
        done.ok_or_else(|| wasmtime::Error::msg("the policy call did not complete"))??;
    }

    // Parse the action.
    match &results[0] {
        Val::Variant(case, payload) => match case.as_str() {
            "restart" => Ok(RestartAction::Restart),
            "restart-after-ms" => match payload.as_deref() {
                Some(Val::U64(ms)) => Ok(RestartAction::RestartAfterMs(*ms)),
                _ => Ok(RestartAction::GiveUp),
            },
            "give-up" => Ok(RestartAction::GiveUp),
            _ => Ok(RestartAction::GiveUp),
        },
        _ => Ok(RestartAction::GiveUp),
    }
}
