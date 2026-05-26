//! The shell session's execution providers (kernel side): `eo9:exec/component-algebra`,
//! `compile`, and `task`.
//!
//! The semantics mirror the usermode runtime (`crates/eo9-runtime`), restricted to what
//! the bare-metal kernel can honestly do today (plan/12-kernel.md Decision 21):
//!
//! * **component-algebra** — `load` recognises exactly the components baked into the
//!   read-only store image (matched by content); `describe` replays the metadata xtask
//!   computed at image-assembly time with the same `eo9-component` crate usermode uses;
//!   `save` returns the original bytes. The combinators (`compose`, `extend`, `restrict`,
//!   `rename`, `configure`) are not implemented on metal yet — they need the component
//!   tooling that arrives with the on-target-codegen rung — and fail with a clear error.
//! * **compile** — a content lookup, not codegen: the component's baked-in host-AOT
//!   artifact *is* its image. Anything not in the store gets a clean `codegen` error.
//! * **task** — `spawn` instantiates the artifact against the kernel root providers
//!   (text/time/entropy — children never receive fs or exec, same policy as usermode) and
//!   binds `main`'s WAVE arguments against its signature; the child then executes on the
//!   shell's drive loop (`drive_children`), interleaved with the shell itself, exactly as
//!   usermode children execute inside their parent's resume — wasmtime forbids re-entering
//!   the event loop from a host function. `wait`/`runnable`/`kill` observe the child;
//!   `resume` (guest-directed fuel donation) is unsupported, as in usermode (E5).
//!
//! Fuel metering for children is not enabled yet: `consume_fuel` is compile-relevant and
//! would invalidate the precompiled artifacts; it lands together with the scheduler work.

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

/// Poll every running child once. Called from the shell's drive loop between polls of the
/// shell itself — the bare-metal counterpart of children executing inside their parent's
/// resume in usermode.
pub fn drive_children() {
    CHILDREN.with(|children| {
        for slot in children.iter_mut() {
            let completed = match slot {
                Some(ChildSlot::Running(drive)) => {
                    let waker = Waker::from(Arc::new(ChildWaker));
                    let mut cx = Context::from_waker(&waker);
                    match drive.as_mut().poll(&mut cx) {
                        Poll::Ready(outcome) => Some(outcome),
                        Poll::Pending => None,
                    }
                }
                _ => None,
            };
            if let Some(outcome) = completed {
                *slot = Some(ChildSlot::Done(outcome));
            }
        }
    });
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
fn spawn_child(
    engine: &Engine,
    component: &Component,
    args: &[WitNamedArg],
    max_memory: Option<u64>,
) -> Result<u32, WitSpawnError> {
    let internal = |err: wasmtime::Error| WitSpawnError::Internal(format!("{err:?}"));

    let mut linker: Linker<KernelState> = Linker::new(engine);
    providers::add_providers(&mut linker).map_err(internal)?;

    let mut store = Store::new(engine, KernelState::new());
    if let Some(max_memory) = max_memory {
        store.data_mut().set_max_memory(max_memory);
        store.limiter(|state| state.limiter());
    }

    // Instantiation must not depend on external completions (eo9 components have no
    // start-time code); drive it with a bounded poll loop, as usermode `spawn` does.
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
            .map_err(internal)?
    };

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

/// One open component value: an index into the baked-in store entries.
struct KComponent {
    entry: usize,
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

/// The clear refusal used by every algebra combinator the kernel cannot perform yet.
fn unsupported(operation: &str) -> String {
    format!(
        "the bare-metal kernel does not implement `{operation}` yet: the component algebra \
         needs the component tooling that arrives with on-target codegen; only programs \
         baked into the read-only store can be run as-is"
    )
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
                    let rep = store
                        .data_mut()
                        .shell_exec()?
                        .insert_component(KComponent { entry });
                    Ok(Resource::new_own(rep))
                }
                None => Err(WitLoadError::NotAnEo9Module(
                    "this component is not in the kernel's baked-in store; the bare-metal \
                     kernel cannot load arbitrary components until on-target codegen lands"
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
            let entries = store.data_mut().shell_entries()?;
            let entry = store
                .data_mut()
                .shell_exec()?
                .component(component.rep())?
                .entry;
            Ok((entries[entry].component.to_vec(),))
        },
    )?;

    algebra.func_wrap(
        "describe",
        |mut store: StoreContextMut<'_, KernelState>,
         (component,): (Resource<AlgComponentRes>,)|
         -> Result<(WitComponentInfo,)> {
            let entries = store.data_mut().shell_entries()?;
            let entry = store
                .data_mut()
                .shell_exec()?
                .component(component.rep())?
                .entry;
            Ok((parse_metadata(entries[entry].metadata),))
        },
    )?;

    algebra.func_wrap(
        "compose",
        |mut store: StoreContextMut<'_, KernelState>,
         (provider, consumer): (Resource<AlgComponentRes>, Resource<AlgComponentRes>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitComposeError>,)> {
            let exec = store.data_mut().shell_exec()?;
            let _ = exec.take_component(provider.rep());
            let _ = exec.take_component(consumer.rep());
            Ok((Err(WitComposeError::Internal(unsupported("$ (compose)"))),))
        },
    )?;

    algebra.func_wrap(
        "extend",
        |mut store: StoreContextMut<'_, KernelState>,
         (base, layer): (Resource<AlgComponentRes>, Resource<AlgComponentRes>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitComposeError>,)> {
            let exec = store.data_mut().shell_exec()?;
            let _ = exec.take_component(base.rep());
            let _ = exec.take_component(layer.rep());
            Ok((Err(WitComposeError::Internal(unsupported("& (extend)"))),))
        },
    )?;

    algebra.func_wrap(
        "restrict",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, _allow): (Resource<AlgComponentRes>, Vec<WitInterfaceRef>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitRestrictError>,)> {
            let exec = store.data_mut().shell_exec()?;
            let _ = exec.take_component(component.rep());
            Ok((Err(WitRestrictError::Internal(unsupported(
                "only (restrict)",
            ))),))
        },
    )?;

    algebra.func_wrap(
        "rename",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, _old, _new): (Resource<AlgComponentRes>, String, String)|
         -> Result<(Result<Resource<AlgComponentRes>, WitRenameError>,)> {
            let exec = store.data_mut().shell_exec()?;
            let _ = exec.take_component(component.rep());
            Ok((Err(WitRenameError::Internal(unsupported("rename"))),))
        },
    )?;

    algebra.func_wrap(
        "configure",
        |mut store: StoreContextMut<'_, KernelState>,
         (component, _args): (Resource<AlgComponentRes>, Vec<WitNamedArg>)|
         -> Result<(Result<Resource<AlgComponentRes>, WitConfigureError>,)> {
            let exec = store.data_mut().shell_exec()?;
            let _ = exec.take_component(component.rep());
            Ok((Err(WitConfigureError::Internal(unsupported("configure"))),))
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
            let exec = store.data_mut().shell_exec()?;
            let component = exec.take_component(component.rep())?;
            let entry = &entries[component.entry];
            if parse_metadata(entry.metadata).kind == WitComponentKind::Provider {
                return Ok((Err(WitCompileError::NotABinary),));
            }
            // "Compilation" on the kernel is a lookup: the baked-in host-AOT artifact is
            // the image (real codegen is the on-target-codegen rung).
            // SAFETY: the artifact comes from the store image produced by `cargo xtask
            // build-kernel` with the same wasmtime version and engine configuration.
            let deserialized = unsafe { Component::deserialize(&engine, entry.artifact) };
            Ok((match deserialized {
                Ok(image) => {
                    let rep = store
                        .data_mut()
                        .shell_exec()?
                        .insert_image(KImage { component: image });
                    Ok(Resource::new_own(rep))
                }
                Err(err) => Err(WitCompileError::Codegen(format!(
                    "the baked-in artifact for this component failed to load: {err:?}"
                ))),
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
            let component = {
                let exec = store.data_mut().shell_exec()?;
                exec.image(image.rep())?.component.clone()
            };
            Ok((
                match spawn_child(&engine, &component, &args, limits.max_memory) {
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
                        Some(Some(ChildSlot::Running(_))) => None,
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
                        Some(ChildSlot::Running(_)) => {
                            // Dropping the drive future drops the child's store, guest
                            // state, and in-flight work (SPEC "Kill and linearity").
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
