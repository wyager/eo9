//! `net.l2.none` — absence of the link-layer network capability.
//!
//! Targets the `eo9:net/l2-none` stub world: exports `eo9:net/l2-optional` with
//! `default()` answering `none`. The root-handle type it mentions is the *imported*
//! `eo9:net/l2.l2-impl` (a types-only use — no operation is ever linked or called), so
//! no instance of it is ever created. The loader and `only` use this provider to seal
//! absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l2-none",
    path: "../../../wit/net",
    // Pull in bindings for the use-dependencies of the imported l2 interface
    // (eo9:io/buffers), which the world does not name directly.
    generate_all,
});

use eo9::net::l2::L2Impl;
use exports::eo9::net::l2_optional;

/// The `net.l2.none` provider.
struct Stub;

impl l2_optional::Guest for Stub {
    fn default() -> Option<L2Impl> {
        None
    }
}

export!(Stub);
