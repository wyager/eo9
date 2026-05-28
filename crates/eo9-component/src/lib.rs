//! The Eo9 component algebra: pure, unprivileged value manipulation on program bytecode.
//!
//! This crate implements the host side of the `eo9:exec/component-algebra` WIT interface
//! (see `wit/exec/exec.wit` and SPEC.md "Execution APIs"): loading, saving, and
//! describing components, and the operators of the algebra --
//!
//! * [`compose`] -- `$`: satisfy a consumer's imports from a provider's matching exports
//!   (matched by slot name, sealed, with the provider's unconsumed exports dropped).
//! * [`extend`] -- `&`: extend an environment provider with another provider
//!   (right-biased export union, imports wired left-to-right).
//! * [`restrict`] -- `only`: bound a component to a fixed allow-list of interfaces.
//! * [`rename`] -- relabel a capability slot on imports and exports alike.
//! * [`configure`] -- bind a provider's compose-time configuration constants and seal
//!   its config interface away.
//!
//! No execution and no I/O policy lives here -- this is math on bytes. Operations are
//! deterministic: the same inputs produce byte-identical outputs, which is what the
//! content-addressed store and compile cache key on.
//!
//! # `no_std`
//!
//! The crate is `#![no_std]` (with `alloc`) when its default `std` feature is disabled,
//! so the bare-metal kernel can run the algebra on-target. With the default `std` feature
//! (used by every host build) behaviour is byte-identical to before. `std` also forwards
//! to the `std` feature of the dependencies that gate it; the dependencies that are
//! `std`-only in their published form (`wac-graph`, `wit-component`, and the `std`-gated
//! feature paths of `wit-parser`/`wasm-wave`) are swapped for `no_std` copies by the
//! kernel workspace's `[patch.crates-io]` (see `kernel/vendor/`).

#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

mod component;
mod compose;
mod configure;
mod describe;
mod error;
mod externs;
mod rename;
mod restrict;
pub mod semver;
mod slots;
mod synth;

pub use component::Component;
pub use compose::{ComposeWarning, compose, compose_checked, extend};
pub use configure::configure;
pub use error::{ComposeError, ConfigureError, LoadError, RenameError, RestrictError};
pub use rename::rename;
pub use restrict::restrict;

/// Which of the two module kinds a component is (SPEC.md: binary or provider, never
/// both). Mirrors `component-kind` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentKind {
    /// Exports `main(args)` and is run.
    Binary,
    /// Exports interfaces (plus optionally `configure(args)`) and is composed.
    Provider,
}

/// One interface a component still needs (a residual import).
/// Mirrors `import-need` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportNeed {
    /// The slot name this import is keyed by; defaults to the interface name when the
    /// import is unnamed -- e.g. `system-fs`, or `eo9:net/net` for a default slot.
    pub slot: String,
    /// The imported interface, e.g. `eo9:net/net`.
    pub interface: String,
    /// The semver it was built against (satisfied per the semver rule in the spec).
    pub version: String,
    /// Mandatory vs. optional import.
    pub required: bool,
    /// Whether the imported interface carries no authority (it has no functions, only
    /// types) — e.g. a types-only `use` of an API interface, or an `eo9:*/types` sibling.
    pub authority_free: bool,
}

/// One export slot: a name carrying an interface and version.
/// Mirrors `export-slot` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSlot {
    /// The slot name (defaults to the interface name).
    pub name: String,
    /// The exported interface, e.g. `eo9:net/net`.
    pub interface: String,
    /// The interface version text.
    pub version: String,
}

/// One named, typed argument of `main` (binary) or `configure` (provider).
/// Mirrors `arg-spec` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    /// The parameter name (one shell flag per parameter).
    pub name: String,
    /// The parameter's WIT type text; it drives WAVE parsing of invocations.
    pub ty: String,
}

/// Kind, imports, exports, and argument signature of a component.
/// Mirrors `component-info` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentInfo {
    /// Binary or provider.
    pub kind: ComponentKind,
    /// Residual imports, as capability slots.
    pub imports: Vec<ImportNeed>,
    /// Exported interfaces, as capability slots.
    pub exports: Vec<ExportSlot>,
    /// The argument signature of `main` (binary) or `configure` (provider).
    pub args: Vec<ArgSpec>,
}

/// A reference to an interface for `restrict` allow-lists, e.g. `eo9:fs/fs`.
/// An entry admits both the required and `-optional` flavor (SPEC.md "Restriction:
/// `only`"). Mirrors `interface-ref` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceRef {
    /// The interface name, e.g. `eo9:fs/fs`.
    pub interface: String,
    /// `None` means "any version of this interface".
    pub version: Option<String>,
}

impl InterfaceRef {
    /// A reference admitting any version of `interface`.
    pub fn any(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            version: None,
        }
    }
}
