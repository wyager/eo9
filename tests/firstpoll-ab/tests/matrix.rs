//! The async-hardening matrix (plan/13), verbatim, against the vendored wasmtime.
//!
//! These five suites pin the suspension/cancellation semantics that
//! first-poll-inline must NOT change: deep parked chains, kill/cancel cascades,
//! fan-out completion order, trap surfacing, and bind interplay. They must pass
//! with the same outcomes in BOTH arms of the A/B (`first-poll-inline` off and on);
//! the suites are included by `#[path]` so the pins are the originals, not copies.

#[path = "../../eo9-integration/tests/async_chains.rs"]
mod async_chains;

#[path = "../../eo9-integration/tests/async_kill.rs"]
mod async_kill;

#[path = "../../eo9-integration/tests/async_fanout.rs"]
mod async_fanout;

#[path = "../../eo9-integration/tests/async_trap.rs"]
mod async_trap;

#[path = "../../eo9-integration/tests/async_bind.rs"]
mod async_bind;
