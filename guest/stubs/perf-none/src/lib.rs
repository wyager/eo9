//! `perf.none` — absence of the perf capability.
//!
//! Targets the `eo9:perf/none` stub world: exports `eo9:perf/perf-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/perf",
});

use exports::eo9::perf::perf_optional;
use exports::eo9::perf::types;

/// The `perf.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type PerfImpl = NoImpl;
}

impl types::GuestPerfImpl for NoImpl {}

impl perf_optional::Guest for Stub {
    fn default() -> Option<types::PerfImpl> {
        None
    }
}

export!(Stub);
