//! `net.l4.none` — absence of the transport-layer network capability.
//!
//! Targets the `eo9:net/l4-none` stub world: exports `eo9:net/l4-optional` with
//! `default()` answering `none`. The root-handle type it mentions is the *imported*
//! `eo9:net/l4.l4-impl` (a types-only use — no operation is ever linked or called), so
//! no instance of it is ever created. The loader and `only` use this provider to seal
//! absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l4-none",
    path: "../../../wit/net",
    // Pull in bindings for the use-dependencies of the imported l4 interface
    // (eo9:io/buffers), which the world does not name directly.
    generate_all,
});

use eo9::net::l4::L4Impl;
use exports::eo9::net::l4_optional;

/// The `net.l4.none` provider.
struct Stub;

impl l4_optional::Guest for Stub {
    fn default() -> Option<L4Impl> {
        None
    }
}

export!(Stub);
