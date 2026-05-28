//! Embed a usermode Eo9 instance in a Rust program.
//!
//! `eo9-embed` is the reusable core behind the future `eo9 bundle` command and the /try
//! v2 browser blob (plan/15, plan/16): it wraps the Eo9 runtime and a pluggable
//! root-provider *backend* behind a small builder API, so a host program can decide
//! exactly which capabilities a component receives and run it to a three-way outcome.
//!
//! ```no_run
//! use eo9_embed::{Eo9, NamedArg};
//!
//! // A host-backed instance that grants text + time (the default capability set).
//! let eo9 = Eo9::builder().build()?;
//! let bytes = std::fs::read("hello.wasm")?;
//! let outcome = eo9.run_bytes(&bytes, &[NamedArg::new("name", "\"world\"")])?;
//! println!("{}", eo9_embed::render_outcome(&outcome).0);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Capabilities
//!
//! An [`Eo9`] instance is built from a [`Grants`] set (which root capabilities the
//! embedder hands out) and a [`ProviderSource`] *backend* (where those capabilities come
//! from). Two backends ship today:
//!
//! * [`Sandbox`] — deterministic, in-memory providers (captured text, a frozen clock, a
//!   seeded RNG, an in-memory filesystem). No host access, reproducible, portable. This is
//!   the shape the wasm32/Pulley embedding will reuse with browser-side shims.
//! * [`Host`] — the host OS: stdio text, the wall clock, the OS RNG, and a rooted host
//!   filesystem (only when a root directory is configured). Requires the `host` feature
//!   (on by default).
//!
//! Handing capabilities to a program never widens it: the runtime links only the
//! interfaces the component actually imports (the loader rule), an import with no provider
//! is a spawn error, and a program that *requires* `eo9:fs` without an fs grant is refused
//! up front with a clear message. Children spawned by an exec-holding program receive the
//! same root grants *minus* exec — exec is never inherited.
//!
//! # Backend-agnostic by design
//!
//! The provider backend is a trait object, so the wasm32/Pulley path (plan/15 Decisions
//! 15–20) is a new [`ProviderSource`] implementation — thin in-memory shims over the
//! browser's text/time/entropy — rather than an API change. See `plan/16-embed.md`.

use std::sync::Arc;

use eo9_component::{Component, ComponentKind, ImportNeed};
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    ChildPolicy, EngineOptions, EntropyProvider, ExecProvider, FsProvider, Image, Providers,
    ResumeOutcome, SpawnLimits, Task, TextProvider, TimeProvider, new_engine,
};

mod sandbox;
pub use sandbox::Sandbox;

#[cfg(feature = "host")]
mod host;
#[cfg(feature = "host")]
pub use host::{ExecSnapshotPolicy, Host};

// Re-exports so embedders need not also depend on the sibling crates for the common path.
pub use eo9_component::{
    ComponentInfo, ComponentKind as Kind, ComposeError, compose, configure, extend, restrict,
};
pub use eo9_runtime::{NamedArg, Outcome, WaveValue};

/// Fuel donated per resume by the built-in drive loop. The loop keeps donating until the
/// program finishes, so this is a scheduling granule, not a budget (mirrors `eo9 run`).
const RESUME_DONATION: u64 = 100 * FUEL_QUANTUM;

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// Anything that can go wrong building or running an embedded Eo9 instance.
#[derive(Debug)]
pub enum EmbedError {
    /// The bytes were not a loadable component.
    Load(String),
    /// The program is a provider, not a binary (providers are composed, never run).
    NotABinary,
    /// The program requires a capability the instance does not grant (the message names
    /// it, e.g. a required `eo9:fs` import without an fs grant).
    MissingCapability(String),
    /// Engine creation or codegen failed.
    Compile(String),
    /// The backend could not build a requested provider (e.g. an fs root that is missing
    /// or not a directory).
    Provider(String),
    /// Spawning the task failed (e.g. an unsatisfiable import, or an argument that does
    /// not type-check against `main`'s signature).
    Spawn(String),
    /// Reading a program from disk failed.
    Io(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Load(m) => write!(f, "not a loadable component: {m}"),
            EmbedError::NotABinary => write!(
                f,
                "this component is a provider, not a binary: providers are composed (`$`), never run"
            ),
            EmbedError::MissingCapability(m) => write!(f, "{m}"),
            EmbedError::Compile(m) => write!(f, "compilation failed: {m}"),
            EmbedError::Provider(m) => write!(f, "cannot build a root provider: {m}"),
            EmbedError::Spawn(m) => write!(f, "cannot spawn the program: {m}"),
            EmbedError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for EmbedError {}

// ---------------------------------------------------------------------------------------
// Grants and limits
// ---------------------------------------------------------------------------------------

/// Which root capabilities an [`Eo9`] instance grants to the programs it runs.
///
/// The default is the safe minimal set for a useful run: text, time, and entropy on;
/// filesystem and exec off. Filesystem and exec are opt-in, mirroring the `eo9` CLI's
/// `--fs-root`-only fs grant and exec-only-for-the-shell policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grants {
    /// `eo9:text` — standard output/error and `read-line`.
    pub text: bool,
    /// `eo9:time` — clocks and `sleep`.
    pub time: bool,
    /// `eo9:entropy` — random bytes.
    pub entropy: bool,
    /// `eo9:fs` — a filesystem (the backend decides what it is rooted at).
    pub fs: bool,
    /// `eo9:exec` — the ability to compose, compile, and spawn child programs. Never
    /// inherited by those children.
    pub exec: bool,
}

impl Default for Grants {
    fn default() -> Self {
        Grants {
            text: true,
            time: true,
            entropy: true,
            fs: false,
            exec: false,
        }
    }
}

impl Grants {
    /// No capabilities at all.
    pub fn none() -> Self {
        Grants {
            text: false,
            time: false,
            entropy: false,
            fs: false,
            exec: false,
        }
    }

    /// The grants a child receives: the same roots minus exec (exec is never inherited).
    fn child(self) -> Self {
        Grants {
            exec: false,
            ..self
        }
    }
}

/// Per-run resource limits handed to `spawn`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    /// Maximum linear memory the program may grow to, in bytes (`None` = engine default).
    pub max_memory: Option<u64>,
    /// Maximum table elements the program may grow to (`None` = engine default).
    pub max_table_elements: Option<u64>,
}

impl Limits {
    fn to_spawn(self) -> SpawnLimits {
        SpawnLimits {
            max_memory: self.max_memory,
            max_table_elements: self.max_table_elements,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Provider backend
// ---------------------------------------------------------------------------------------

/// The root providers for one run, before the exec capability is layered on.
///
/// A [`ProviderSource`] produces these per run (and per child). The `eo9-embed` core adds
/// the exec capability itself when granted, so backends never deal with exec.
#[derive(Default)]
pub struct Roots {
    pub text: Option<Box<dyn TextProvider>>,
    pub time: Option<Box<dyn TimeProvider>>,
    pub entropy: Option<Box<dyn EntropyProvider>>,
    pub fs: Option<Box<dyn FsProvider>>,
}

/// A source of root providers — the *backend* of an embedded instance.
///
/// Implementations decide where text/time/entropy/fs come from; the `eo9-embed` core
/// decides *whether* each is granted (via [`Grants`]) and adds the exec capability. A
/// backend is asked for fresh providers on every run and every child spawn, so it must be
/// cheap to call repeatedly and safe to share across threads.
pub trait ProviderSource: Send + Sync + 'static {
    /// Build the root providers for the given grants. Only the granted capabilities need
    /// be populated; the core enforces the loader rule regardless.
    fn roots(&self, grants: Grants) -> Result<Roots, EmbedError>;
}

// ---------------------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------------------

/// Builder for an [`Eo9`] instance.
#[derive(Default)]
pub struct Builder {
    grants: Grants,
    limits: Limits,
    debug_info: bool,
    source: Option<Arc<dyn ProviderSource>>,
}

impl Builder {
    /// Set the full grant set at once.
    pub fn grants(mut self, grants: Grants) -> Self {
        self.grants = grants;
        self
    }

    /// Grant (or revoke) the filesystem capability. With the [`Host`] backend this needs
    /// a root directory configured on the backend; with [`Sandbox`] it is the in-memory
    /// filesystem.
    pub fn grant_fs(mut self, grant: bool) -> Self {
        self.grants.fs = grant;
        self
    }

    /// Grant (or revoke) the exec capability (compose/compile/spawn children).
    pub fn grant_exec(mut self, grant: bool) -> Self {
        self.grants.exec = grant;
        self
    }

    /// Set the per-run resource limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Cap linear memory growth, in bytes.
    pub fn max_memory(mut self, bytes: u64) -> Self {
        self.limits.max_memory = Some(bytes);
        self
    }

    /// Compile with debug info (off by default).
    pub fn debug_info(mut self, on: bool) -> Self {
        self.debug_info = on;
        self
    }

    /// Use a specific provider backend (e.g. [`Sandbox`] or [`Host`]). Without this, the
    /// default backend is [`Host`] when the `host` feature is on, else [`Sandbox`].
    pub fn backend(mut self, source: impl ProviderSource) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    /// Build the instance.
    pub fn build(self) -> Result<Eo9, EmbedError> {
        let source = match self.source {
            Some(source) => source,
            None => default_source(),
        };
        Ok(Eo9 {
            engine_options: EngineOptions {
                debug_info: self.debug_info,
            },
            grants: self.grants,
            limits: self.limits,
            source,
        })
    }
}

#[cfg(feature = "host")]
fn default_source() -> Arc<dyn ProviderSource> {
    Arc::new(Host::new())
}

#[cfg(not(feature = "host"))]
fn default_source() -> Arc<dyn ProviderSource> {
    Arc::new(Sandbox::new())
}

// ---------------------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------------------

/// An embedded Eo9 instance: a capability environment plus the runtime that runs programs
/// in it.
///
/// Cheap to clone-by-reference through its [`ProviderSource`]; one instance can run many
/// programs. Each run compiles the program fresh — the compile cache integration that the
/// `eo9` binary has is a follow-up (see `plan/16-embed.md`).
pub struct Eo9 {
    engine_options: EngineOptions,
    grants: Grants,
    limits: Limits,
    source: Arc<dyn ProviderSource>,
}

impl Eo9 {
    /// Start building an instance.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// The grants this instance hands to the programs it runs.
    pub fn grants(&self) -> Grants {
        self.grants
    }

    /// Describe a component (kind, imports, `main`'s argument signature) without running
    /// it.
    pub fn describe(&self, bytes: &[u8]) -> Result<ComponentInfo, EmbedError> {
        Ok(load(bytes)?.describe())
    }

    /// Compile and run a binary component from bytes, returning its three-way outcome.
    pub fn run_bytes(&self, bytes: &[u8], args: &[NamedArg]) -> Result<Outcome, EmbedError> {
        let component = load(bytes)?;
        self.run_component(&component, args)
    }

    /// Run a program read from a path on the host filesystem.
    pub fn run_path(
        &self,
        path: impl AsRef<std::path::Path>,
        args: &[NamedArg],
    ) -> Result<Outcome, EmbedError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|err| EmbedError::Io(format!("cannot read {}: {err}", path.display())))?;
        self.run_bytes(&bytes, args)
    }

    /// Run an already-loaded (possibly composed) component. Use the re-exported algebra
    /// ([`compose`], [`extend`], [`restrict`], [`configure`]) to build a capability
    /// environment around a program and run the closed result here.
    pub fn run_component(
        &self,
        component: &Component,
        args: &[NamedArg],
    ) -> Result<Outcome, EmbedError> {
        let info = component.describe();
        if info.kind == ComponentKind::Provider {
            return Err(EmbedError::NotABinary);
        }
        // A required fs import without an fs grant is refused here with a clear message
        // (the raw linker error would just say an import is unsatisfied).
        if !self.grants.fs && requires_fs(&info.imports) {
            return Err(EmbedError::MissingCapability(
                "this program requires the eo9:fs filesystem capability, which this \
                 instance does not grant: build it with `grant_fs(true)` and a backend \
                 that supplies a filesystem"
                    .to_string(),
            ));
        }

        let engine = new_engine(&self.engine_options)
            .map_err(|err| EmbedError::Compile(format!("cannot create the engine: {err:#}")))?;
        let image = Image::compile(&engine, component.bytes())
            .map_err(|err| EmbedError::Compile(format!("{err}")))?;

        // Root providers for this run, plus exec if granted. The exec capability's child
        // policy hands children the same roots minus exec (never inherited).
        let roots = self.source.roots(self.grants)?;
        let exec = if self.grants.exec {
            let source = Arc::clone(&self.source);
            let child_grants = self.grants.child();
            let policy =
                ChildPolicy::with_providers(move || child_providers(&source, child_grants));
            Some(ExecProvider::new(image.engine(), policy))
        } else {
            None
        };
        let providers = assemble(roots, exec);

        let mut task = Task::spawn(&image, args, self.limits.to_spawn(), providers)
            .map_err(|err| EmbedError::Spawn(format!("{err}")))?;
        Ok(drive_to_completion(&mut task))
    }
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn load(bytes: &[u8]) -> Result<Component, EmbedError> {
    Component::load(bytes.to_vec()).map_err(|err| EmbedError::Load(format!("{err}")))
}

/// Assemble a [`Providers`] from backend roots plus an optional exec provider.
fn assemble(roots: Roots, exec: Option<ExecProvider>) -> Providers {
    Providers {
        text: roots.text,
        time: roots.time,
        entropy: roots.entropy,
        fs: roots.fs,
        exec,
    }
}

/// Build a child's providers, degrading gracefully: if the backend cannot satisfy the full
/// child grants (e.g. a transient fs-root failure), drop fs and try again, and failing
/// that hand over no capabilities rather than panicking inside the exec machinery.
fn child_providers(source: &Arc<dyn ProviderSource>, grants: Grants) -> Providers {
    let roots = source.roots(grants).or_else(|_| {
        source.roots(Grants {
            fs: false,
            ..grants
        })
    });
    match roots {
        Ok(roots) => assemble(roots, None),
        Err(_) => Providers::none(),
    }
}

/// Whether the component has a *required* import of an `eo9:fs` interface. Optional fs
/// imports do not count — the runtime seals those with absence.
fn requires_fs(imports: &[ImportNeed]) -> bool {
    imports
        .iter()
        .any(|need| need.required && !need.authority_free && need.interface.starts_with("eo9:fs/"))
}

/// Drive a task to completion: donate fuel, run, park the thread on I/O, repeat. Shared
/// by every run (mirrors the `eo9` binary's built-in drive loop).
fn drive_to_completion(task: &mut Task) -> Outcome {
    loop {
        match task.resume(RESUME_DONATION) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => {}
            ResumeOutcome::Blocked => wait_until_runnable(task),
        }
    }
}

/// Block the calling thread until `task` can make progress again — that is, until a
/// provider completion rings its doorbell.
fn wait_until_runnable(task: &Task) {
    use std::sync::Arc as StdArc;
    use std::task::{Context, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: StdArc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &StdArc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(StdArc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let runnable = task.runnable();
    let mut runnable = std::pin::pin!(runnable);
    use std::future::Future;
    while runnable.as_mut().poll(&mut context).is_pending() {
        std::thread::park();
    }
}

/// Render an [`Outcome`] as the spec's three-way `program-outcome` in WAVE, with the
/// matching process-style exit code (0 success, 1 failure, 2 abnormal).
pub fn render_outcome(outcome: &Outcome) -> (String, u8) {
    match outcome {
        Outcome::Success(value) => (render_arm("success", &value.value), 0),
        Outcome::Failure(value) => (render_arm("failure", &value.value), 1),
        Outcome::Trapped(reason) => (format!("abnormal(trapped({}))", wave_string(reason)), 2),
        Outcome::Killed => ("abnormal(killed)".to_string(), 2),
    }
}

fn render_arm(arm: &str, value: &str) -> String {
    if value.is_empty() {
        arm.to_string()
    } else {
        format!("{arm}({value})")
    }
}

fn wave_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{{{:x}}}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_grants_are_text_time_entropy() {
        let g = Grants::default();
        assert!(g.text && g.time && g.entropy);
        assert!(!g.fs && !g.exec);
    }

    #[test]
    fn children_never_inherit_exec() {
        let g = Grants {
            exec: true,
            fs: true,
            ..Grants::default()
        };
        let child = g.child();
        assert!(!child.exec);
        assert!(child.fs && child.text && child.time && child.entropy);
    }

    #[test]
    fn only_required_fs_imports_demand_a_grant() {
        let need = |interface: &str, required: bool| ImportNeed {
            slot: interface.to_string(),
            interface: interface.to_string(),
            version: "0.1.0".to_string(),
            required,
            authority_free: false,
        };
        assert!(requires_fs(&[need("eo9:fs/fs", true)]));
        assert!(!requires_fs(&[need("eo9:fs/fs", false)]));
        assert!(!requires_fs(&[need("eo9:text/text", true)]));
        assert!(!requires_fs(&[]));
    }

    #[test]
    fn outcomes_render_as_wave_with_exit_codes() {
        let success = Outcome::Success(WaveValue {
            ty: "variant { greeted }".to_string(),
            value: "greeted".to_string(),
        });
        assert_eq!(
            render_outcome(&success),
            ("success(greeted)".to_string(), 0)
        );
        let trapped = Outcome::Trapped("boom".to_string());
        assert_eq!(
            render_outcome(&trapped),
            ("abnormal(trapped(\"boom\"))".to_string(), 2)
        );
        assert_eq!(
            render_outcome(&Outcome::Killed),
            ("abnormal(killed)".to_string(), 2)
        );
    }
}
