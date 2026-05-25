//! The Eo9 scheduler: policy and bookkeeping over abstract task ids.
//!
//! This crate is the one scheduler used by both the usermode `eo9` binary and the bare-metal
//! kernel (SPEC.md, Implementation Details). It is `no_std` + `alloc` and depends on nothing
//! else — no Wasmtime types, no OS types, nothing beyond `core`, `alloc`, and
//! `core::sync::atomic`. The embedder (the runtime in usermode, the kernel on metal) owns the
//! actual execution of tasks; this crate decides *which* task runs next and keeps the books:
//! task states, parent/child structure, and a conserved fuel ledger.
//!
//! # Pieces
//!
//! * [`Scheduler`] — the task table (runnable / running / blocked / done, parent/child links),
//!   the run queue (behind the [`Policy`] trait), and per-task fuel accounts.
//! * [`DeterministicPolicy`] and [`FairPolicy`] — the two shipped run-queue policies: a stable
//!   lowest-id-first order for deterministic execution, and round-robin for fairness.
//! * [`FuelLedger`] — standalone conserved fuel accounting, reusable by nested schedulers.
//! * [`CompletionQueue`] and [`Doorbell`] — the per-task readiness primitives of the async
//!   host side (SPEC.md, "How readiness is implemented").
//! * [`Platform`] — the little the scheduler's embedder needs from the machine: `idle()`, and
//!   optionally `now()`.
//!
//! # The resume cycle
//!
//! The embedder drives the scheduler from an ordinary loop: [`pick`](Scheduler::pick) the next
//! runnable task, [`donate`](Scheduler::donate) it fuel from the node's pool, resume it on the
//! real execution engine, and [`report`](Scheduler::report) how much fuel the resume consumed
//! and how it ended (out of fuel, blocked, or done). Completions arriving from providers go
//! through per-task [`CompletionQueue`]s and [`Doorbell`]s and are surfaced to the scheduler
//! with [`ready`](Scheduler::ready); when nothing is runnable the embedder waits on
//! [`Platform::idle`].
//!
//! ```
//! use eo9_sched::{ResumeOutcome, Scheduler};
//!
//! let mut sched = Scheduler::deterministic();
//!
//! // The platform (or this node's parent) donated us a quantum of fuel.
//! sched.refuel(1_000).unwrap();
//!
//! // Two root tasks; each gets part of the quantum.
//! let a = sched.spawn(None).unwrap();
//! let b = sched.spawn(None).unwrap();
//! sched.donate(a, 600).unwrap();
//! sched.donate(b, 400).unwrap();
//!
//! // The embedder's loop: pick, resume (simulated here), report.
//! let mut finished = Vec::new();
//! while let Some(task) = sched.pick() {
//!     let fuel = sched.fuel_of(task).unwrap();
//!     // ... resume the real task with `fuel` here; pretend it spent it all and finished.
//!     sched.report(task, fuel, ResumeOutcome::Done).unwrap();
//!     finished.push(task);
//! }
//!
//! assert_eq!(finished, vec![a, b]); // deterministic policy: lowest id first
//! assert!(sched.fuel_audit().is_conserved());
//! ```
//!
//! # Invariants
//!
//! * **Single resumer per task.** A task is resumed by at most one resumer at a time: once
//!   [`pick`](Scheduler::pick) hands a task out, that caller is its resumer and must
//!   [`report`](Scheduler::report) before anything is picked again. The scheduler enforces
//!   this — a picked task leaves the run queue and is marked [`Running`](TaskState::Running),
//!   and `pick` panics if called while a task is still running. On a single core this
//!   degenerates to "one task in flight"; under SMP the same invariant becomes "at most one
//!   core resumes a given task", which is a later milestone (see below).
//! * **Fuel is conserved.** A scheduler node can only donate fuel it was itself donated
//!   ([`refuel`](Scheduler::refuel) is the node's own incoming donation); fuel only ever moves
//!   between the pool and task accounts, is burned by execution, or is exported back up.
//!   [`Scheduler::fuel_audit`] exposes the books; the conservation law is checked by property
//!   tests and by debug assertions after every ledger operation.
//! * **Task ids are never reused.** A stale [`TaskId`] can never alias a new task.
//!
//! # Single-core, for now
//!
//! The crate currently models a single-core node: one run queue, one task in flight. SMP
//! (per-core run queues, cross-core wakes, the multi-core reading of the single-resumer rule)
//! is documented where it will land — see [`Platform`] — but deliberately not implemented yet.
//!
//! # `no_std`
//!
//! The crate is `no_std` + `alloc` (std is linked only under `cfg(test)` for the host test
//! harness). The bare-metal build is verified with
//! `cargo check -p eo9-sched --target aarch64-unknown-none` — the same target xtask uses to
//! keep the kernel workspace honest — and the unit and property tests run on the host triple
//! via `cargo test -p eo9-sched`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod completion;
mod fuel;
mod platform;
mod policy;
mod sched;

pub use completion::{CompletionQueue, Doorbell};
pub use fuel::{Fuel, FuelError, FuelLedger};
pub use platform::Platform;
pub use policy::{DeterministicPolicy, FairPolicy, Policy};
pub use sched::{FuelAudit, ResumeOutcome, SchedError, Scheduler, TaskId, TaskState};
