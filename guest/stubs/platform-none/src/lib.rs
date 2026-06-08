//! `platform.none` — absence of the platform-device capability.
//!
//! Targets the `eo9:platform/none` stub world: exports
//! `eo9:platform/platform-optional` with `default()` answering `none`, plus the types
//! interface that owns the root-handle resource (which is therefore never
//! instantiated). The loader and `only` use this provider to seal absent optional
//! imports (see SPEC.md, "The capability algebra"); a program that imports the
//! capability optionally observes "no regions" as plain absence and nothing ever
//! traps.

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/platform",
});

use exports::eo9::platform::platform_optional;
use exports::eo9::platform::types;

/// The `platform.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type PlatformImpl = NoImpl;
}

impl types::GuestPlatformImpl for NoImpl {}

impl platform_optional::Guest for Stub {
    fn default() -> Option<types::PlatformImpl> {
        None
    }
}

export!(Stub);
