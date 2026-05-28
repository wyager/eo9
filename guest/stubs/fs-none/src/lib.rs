//! `fs.none` — absence of the fs capability.
//!
//! Targets the `eo9:fs/none` stub world: exports `eo9:fs/fs-optional` with
//! `default()` answering `none`. The root-handle type it mentions is the *imported*
//! `eo9:fs/fs.fs-impl` (a types-only use — no operation is ever linked or called), so
//! no instance of it is ever created. The loader and `only` use this provider to seal
//! absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/fs",
    // Pull in bindings for the use-dependencies of the imported fs interface
    // (eo9:io/buffers), which the world does not name directly.
    generate_all,
});

use eo9::fs::fs::FsImpl;
use exports::eo9::fs::fs_optional;

/// The `fs.none` provider.
struct Stub;

impl fs_optional::Guest for Stub {
    fn default() -> Option<FsImpl> {
        None
    }
}

export!(Stub);
