//! `time.none` — absence of the time capability.
//!
//! Targets the `eo9:time/none` stub world: exports `eo9:time/time-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/time",
});

use exports::eo9::time::time_optional;
use exports::eo9::time::types;

/// The `time.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type TimeImpl = NoImpl;
}

impl types::GuestTimeImpl for NoImpl {}

impl time_optional::Guest for Stub {
    fn default() -> Option<types::TimeImpl> {
        None
    }
}

export!(Stub);
