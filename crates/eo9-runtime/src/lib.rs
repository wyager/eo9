//! eo9-runtime — the privileged half of execution on the usermode path.
//!
//! This crate is the Wasmtime embedding behind the `eo9:exec` interfaces (see `wit/exec`):
//! the host side of `compile` (codegen) and `task` (spawn / resume / runnable / wait / kill),
//! plus the machinery those need — the pinned engine configuration, fuel-metered resumable
//! execution, per-task doorbells for host completions, root provider wiring (text / time /
//! entropy), and WAVE argument / outcome handling.
//!
//! The scheduler is *not* here: `resume` is a donate-and-run call on the caller's own CPU
//! time, and anything that decides *which* task to resume is an ordinary program (or, on the
//! host side, area 05's `eo9-sched`). The run loop in [`task`] is deliberately small and
//! swappable.
//!
//! See `plan/04-runtime.md` (scope, milestones, decisions) and SPEC.md "Execution APIs".

pub mod engine;
pub mod exec;
pub mod image;
mod link;
pub mod outcome;
pub mod providers;
pub mod task;
mod trap;
pub mod wave;

pub use engine::{EngineOptions, compatibility_hash, new_engine};
pub use exec::{ChildPolicy, ExecProvider};
pub use image::{CompileError, Image};
pub use outcome::{Outcome, WaveValue};
pub use providers::{
    Datetime, EntropyProvider, FsError, FsHandle, FsProvider, MemFs, NodeKind, NodeStat, OpenFlags,
    OutputStream, Providers, TextError, TextProvider, TimeProvider,
};
pub use task::{NamedArg, ResumeOutcome, SpawnError, SpawnLimits, Task};
