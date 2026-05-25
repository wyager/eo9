//! Program outcomes: the host-side form of `eo9:exec/task.program-outcome`.
//!
//! A binary's `main` returns `result<program-success, program-failure>` in the program's own
//! vocabulary; the runtime renders that generically as WAVE text plus WIT type text (the
//! `wave-value` record from `wit/exec`), so an outcome can outlive its component. Traps and
//! kills have no arm in the WIT `program-outcome` yet (escalated — see plan/04-runtime.md
//! § Decisions), so the host-side type carries them explicitly.

/// A WAVE-encoded value carrying its WIT type text (`eo9:exec/task.wave-value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveValue {
    /// WIT type text, e.g. `result<string, string>`'s payload type.
    pub ty: String,
    /// The value in WAVE text encoding.
    pub value: String,
}

/// How a task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `main` returned `ok(_)`: the program's own success value.
    Success(WaveValue),
    /// `main` returned `err(_)`: the program's own failure value.
    Failure(WaveValue),
    /// The task trapped (including out-of-fuel exhaustion of the *whole* store, memory
    /// ceiling hits that the guest did not handle, or any other wasm trap).
    Trapped(String),
    /// The task was killed before it finished.
    Killed,
}

impl Outcome {
    /// True for the two normal arms (`main` actually returned).
    pub fn is_normal(&self) -> bool {
        matches!(self, Outcome::Success(_) | Outcome::Failure(_))
    }
}
