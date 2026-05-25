//! Convenience wrappers over `eo9:time/time`.
//!
//! Each one-shot helper obtains the capability's root handle via the `default()`
//! accessor, performs a single operation, and drops the handle again. Programs doing
//! repeated clock reads should call [`default()`] once and use the raw bindings in
//! [`crate::api::time`] with the held handle.

pub use crate::api::time::time::{Datetime, Instant, TimeImpl};

use crate::api::time::time as raw;

/// The time capability's root handle (the `default()` accessor; see SPEC.md,
/// "The capability algebra").
pub fn default() -> TimeImpl {
    raw::default()
}

/// Current wall-clock time.
pub fn now() -> Datetime {
    raw::now(&raw::default())
}

/// Current monotonic time.
pub fn monotonic_now() -> Instant {
    raw::monotonic_now(&raw::default())
}

/// The granularity of the clock in nanoseconds.
pub fn resolution() -> u64 {
    raw::resolution(&raw::default())
}
