//! `disk.none` — absence of the disk capability.
//!
//! Targets the `eo9:disk/none` stub world: exports `eo9:disk/disk-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra").

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/disk",
});

use exports::eo9::disk::disk_optional;
use exports::eo9::disk::types;

/// The `disk.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type DiskImpl = NoImpl;
}

impl types::GuestDiskImpl for NoImpl {}

impl disk_optional::Guest for Stub {
    fn default() -> Option<types::DiskImpl> {
        None
    }
}

export!(Stub);
