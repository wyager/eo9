//! `perf.null` — accept and discard performance measurement.
//!
//! Targets the `eo9:perf/null` stub world: exports `eo9:perf/perf` where the cycle
//! counter always reads zero. Perf counters are themselves a timing side channel and are
//! gated like time (see SPEC.md, "Perf Measurement API" and Security); composing a
//! program with `perf.null` gives it the API surface without any measurement authority.

#![no_std]

extern crate alloc;

use alloc::string::String;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "null",
    path: "../../../wit/perf",
});

use exports::eo9::perf::null_config;
use exports::eo9::perf::perf;
use exports::eo9::perf::types;

/// The `perf.null` provider.
struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct NullPerf;

impl types::Guest for Stub {
    type PerfImpl = NullPerf;
}

impl types::GuestPerfImpl for NullPerf {}

impl null_config::Guest for Stub {
    fn configure() -> Result<types::PerfImpl, String> {
        Ok(types::PerfImpl::new(NullPerf))
    }
}

impl perf::Guest for Stub {
    fn default() -> types::PerfImpl {
        types::PerfImpl::new(NullPerf)
    }

    fn cycle_counter(_p: perf::PerfImplBorrow<'_>) -> u64 {
        0
    }
}

export!(Stub);
