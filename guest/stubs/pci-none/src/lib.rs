//! `pci.none` — absence of the PCI capability.
//!
//! Targets the `eo9:pci/none` stub world: exports `eo9:pci/pci-optional` with
//! `default()` answering `none`, plus the types interface that owns the root-handle
//! resource (which is therefore never instantiated). The loader and `only` use this
//! provider to seal absent optional imports (see SPEC.md, "The capability algebra");
//! a program that imports PCI optionally observes "no devices" as plain absence and
//! nothing ever traps.

#![no_std]

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "none",
    path: "../../../wit/pci",
});

use exports::eo9::pci::pci_optional;
use exports::eo9::pci::types;

/// The `pci.none` provider.
struct Stub;

/// The root-handle resource type. `default()` always answers `none`, so no instance of
/// this type is ever created.
struct NoImpl;

impl types::Guest for Stub {
    type PciImpl = NoImpl;
}

impl types::GuestPciImpl for NoImpl {}

impl pci_optional::Guest for Stub {
    fn default() -> Option<types::PciImpl> {
        None
    }
}

export!(Stub);
