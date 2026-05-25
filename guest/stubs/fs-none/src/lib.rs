//! `fs.none` — absence of the fs capability.
//!
//! Targets the `eo9:fs/none` stub world: exports `eo9:fs/fs-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/fs",
});

use exports::eo9::fs::fs_optional;
use exports::eo9::fs::types;

/// The `fs.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type FsImpl = NoImpl;
}

impl types::GuestFsImpl for NoImpl {}

impl fs_optional::Guest for Stub {
    fn default() -> Option<types::FsImpl> {
        None
    }
}

export!(Stub);
