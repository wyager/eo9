//! `entropy.none` — absence of the entropy capability.
//!
//! Targets the `eo9:entropy/none` stub world: exports `eo9:entropy/entropy-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/entropy",
});

use exports::eo9::entropy::entropy_optional;
use exports::eo9::entropy::types;

/// The `entropy.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type EntropyImpl = NoImpl;
}

impl types::GuestEntropyImpl for NoImpl {}

impl entropy_optional::Guest for Stub {
    fn default() -> Option<types::EntropyImpl> {
        None
    }
}

export!(Stub);
