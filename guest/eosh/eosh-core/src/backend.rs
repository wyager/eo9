//! The [`Backend`] trait: everything the shell needs from the outside world.
//!
//! The shell's own imports are `eo9:exec` (component algebra, compile, task),
//! `eo9:text`, and `eo9:fs`; this trait is their image inside the library, so the
//! parser/evaluator/session can be exercised on the host with a mock and the component
//! crate can bind the same code to the real WIT imports unchanged. The data types here
//! mirror the `eo9:exec` records (`component-info`, `import-need`, `named-arg`,
//! `program-outcome`, …) field for field.
//!
//! Name resolution goes through [`Backend::resolve`] so that the interim convention
//! (open `/bin/<name>.wasm` for execution via the fs API — see [`crate::module_path`])
//! can be swapped for the store-backed resolution of area 11 by changing one
//! implementation, not the shell.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Which of the two module kinds a component is (binary or provider, never both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Binary,
    Provider,
}

/// One residual import of a component (mirrors `eo9:exec/component-algebra.import-need`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportNeed {
    pub slot: String,
    pub interface: String,
    pub version: String,
    pub required: bool,
}

/// One export slot of a component (mirrors `export-slot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSlot {
    pub name: String,
    pub interface: String,
    pub version: String,
}

/// One named, typed argument of `main`/`configure`; `ty` is WIT type text and drives
/// the type-directed argument handling (mirrors `arg-spec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: String,
    pub ty: String,
}

/// Kind, imports, exports, and argument signature of a component (mirrors `component-info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentInfo {
    pub kind: ComponentKind,
    pub imports: Vec<ImportNeed>,
    pub exports: Vec<ExportSlot>,
    pub args: Vec<ArgSpec>,
}

/// A reference to an interface for `only` allow-lists (mirrors `interface-ref`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceRef {
    pub interface: String,
    pub version: Option<String>,
}

/// One WAVE-encoded `main` argument (mirrors `named-arg`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedArg {
    pub name: String,
    pub value: String,
}

/// A WAVE-encoded value carrying its WIT type text (mirrors `wave-value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveValue {
    pub ty: String,
    pub value: String,
}

/// A run that never returned an outcome of its own (mirrors `abnormal-exit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbnormalExit {
    Trapped(String),
    Killed,
}

/// A program's outcome, the executor-side three-way view (mirrors `program-outcome`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success(WaveValue),
    Failure(WaveValue),
    Abnormal(AbnormalExit),
}

/// An error reported by the backend, already rendered for the user. The component
/// backend formats the `eo9:exec`/`eo9:fs` error variants into these messages; the
/// shell only relays them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub message: String,
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        BackendError {
            message: message.into(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// What the shell needs from its imports, as one trait.
///
/// `resolve` and `wait` are `async` because their WIT counterparts return Component
/// Model futures (`open-exec`/`exec-read` and `task.wait`); everything else in
/// `eo9:exec` is synchronous. Component values are moved into the operations that
/// consume them, mirroring the ownership of the WIT algebra; [`Backend::duplicate`]
/// (save + load in the component backend) is what makes `let`-bound values reusable.
// Guests are single-threaded and the host-side tests drive ready futures directly, so
// the returned futures deliberately carry no `Send` bound.
#[allow(async_fn_in_trait)]
pub trait Backend {
    /// An open program value (binary or provider).
    type Component;
    /// A compiled artifact, admitted for execution via spawn.
    type Image;
    /// A spawned task.
    type Task;

    /// Resolve a (possibly dotted) program name to an open component value.
    async fn resolve(&mut self, name: &str) -> Result<Self::Component, BackendError>;

    /// Load a component from raw bytes (the `load` half of `save`/`load`).
    fn load(&mut self, bytes: &[u8]) -> Result<Self::Component, BackendError>;

    /// Duplicate a component value (used for `let` bindings and the granted environment).
    fn duplicate(&mut self, component: &Self::Component) -> Result<Self::Component, BackendError>;

    /// Kind, imports, exports, and argument signature.
    fn describe(&mut self, component: &Self::Component) -> ComponentInfo;

    /// `$` — satisfy `consumer`'s imports from `provider`'s matching exports.
    fn compose(
        &mut self,
        provider: Self::Component,
        consumer: Self::Component,
    ) -> Result<Self::Component, BackendError>;

    /// `&` — `base` extended and, where they overlap, overridden by `layer`.
    fn extend(
        &mut self,
        base: Self::Component,
        layer: Self::Component,
    ) -> Result<Self::Component, BackendError>;

    /// `only` — bound `component` to the allow-list.
    fn restrict(
        &mut self,
        component: Self::Component,
        allow: &[InterfaceRef],
    ) -> Result<Self::Component, BackendError>;

    /// `rename` — relabel slot `from` to `to` on imports and exports alike.
    fn rename(
        &mut self,
        component: Self::Component,
        from: &str,
        to: &str,
    ) -> Result<Self::Component, BackendError>;

    /// Bind a provider's `configure` arguments as compose-time constants, yielding the
    /// configured provider (its config interface is no longer visible afterwards).
    /// This is how a provider's flags are applied before it is used by `$`, `&`, or `with`.
    fn configure(
        &mut self,
        provider: Self::Component,
        args: &[NamedArg],
    ) -> Result<Self::Component, BackendError>;

    /// Compile a closed binary to an image.
    fn compile(&mut self, component: Self::Component) -> Result<Self::Image, BackendError>;

    /// Spawn a task from an image with WAVE-encoded `main` arguments.
    fn spawn(&mut self, image: &Self::Image, args: &[NamedArg])
    -> Result<Self::Task, BackendError>;

    /// Wait for a task to finish and return its outcome.
    async fn wait(&mut self, task: Self::Task) -> Outcome;

    /// Write a line to the shell's standard output.
    fn print(&mut self, text: &str);

    /// Write a line to the shell's standard error.
    fn print_error(&mut self, text: &str);
}
