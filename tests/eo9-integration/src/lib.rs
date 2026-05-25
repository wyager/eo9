//! Shared harness for the Eo9 cross-area integration tests (plan/13-tests.md).
//!
//! The integration suites (under `tests/`) exercise the capability algebra end-to-end:
//! components are composed with `eo9-component` and then actually executed with
//! `eo9-runtime`, so the spec's capability claims (sealing, `only`, deny, slots,
//! optional absence), its determinism claims, and the kill/linearity contract are
//! observed through program behaviour, not just through `describe()`.
//!
//! Two pieces are reusable by other areas and by later milestones:
//!
//! * [`fixtures`] — building executable fixture components in-process from WIT text plus
//!   a hand-written core module (the same pipeline `wit-bindgen`-built guests go through,
//!   minus the Rust). No prebuilt guest artifacts and no area-09 stub components are
//!   needed, so the suite runs from a clean checkout before `xtask build-guest`.
//! * [`run`] — compiling a component with the pinned engine and driving a task to its
//!   outcome under a given set of root providers.

pub mod fixtures;
pub mod run;
