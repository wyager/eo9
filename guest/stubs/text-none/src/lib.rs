//! `text.none` — absence of the text capability.
//!
//! Targets the `eo9:text/none` stub world: exports `eo9:text/text-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/text",
});

use exports::eo9::text::text_optional;
use exports::eo9::text::types;

/// The `text.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type TextImpl = NoImpl;
}

impl types::GuestTextImpl for NoImpl {}

impl text_optional::Guest for Stub {
    fn default() -> Option<types::TextImpl> {
        None
    }
}

export!(Stub);
