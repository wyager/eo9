//! `net.none` — absence of the net capability.
//!
//! Targets the `eo9:net/none` stub world: exports `eo9:net/net-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/net",
});

use exports::eo9::net::net_optional;
use exports::eo9::net::types;

/// The `net.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type NetImpl = NoImpl;
}

impl types::GuestNetImpl for NoImpl {}

impl net_optional::Guest for Stub {
    fn default() -> Option<types::NetImpl> {
        None
    }
}

export!(Stub);
