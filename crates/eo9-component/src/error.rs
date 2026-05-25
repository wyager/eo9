//! Error types for the component algebra.
//!
//! Each enum mirrors the corresponding `variant` in `wit/exec/exec.wit`
//! (`load-error`, `compose-error`, `restrict-error`, `rename-error`) so the runtime can
//! translate between the two without losing information.

use std::error::Error;
use std::fmt;

/// Errors from [`Component::load`](crate::Component::load).
///
/// Mirrors `load-error` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The bytes are not a valid Component Model component.
    InvalidComponent(String),
    /// The bytes are a valid component, but not an Eo9 module (neither binary nor
    /// provider under the classification rule in SPEC.md "WASM runtime").
    NotAnEo9Module(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidComponent(msg) => write!(f, "not a valid component: {msg}"),
            Self::NotAnEo9Module(msg) => write!(f, "not an Eo9 module: {msg}"),
        }
    }
}

impl Error for LoadError {}

/// Errors from [`compose`](crate::compose) (`$`) and [`extend`](crate::extend) (`&`).
///
/// Mirrors `compose-error` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeError {
    /// The operand that must be a provider is not one (the left operand of `$`;
    /// either operand of `&`).
    NotAProvider,
    /// An export and the import it would satisfy (matched by slot name) have
    /// incompatible types.
    TypeMismatch(String),
    /// An unexpected failure in the underlying wiring/encoding machinery.
    Internal(String),
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAProvider => write!(f, "operand is not a provider"),
            Self::TypeMismatch(msg) => write!(f, "export/import type mismatch: {msg}"),
            Self::Internal(msg) => write!(f, "internal composition error: {msg}"),
        }
    }
}

impl Error for ComposeError {}

/// Errors from [`restrict`](crate::restrict) (`only`).
///
/// Mirrors `restrict-error` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestrictError {
    /// Required residual imports outside the allow-list (the compose-time error of
    /// `only`), naming the offenders as `slot (interface@version)` strings.
    RequiredOutsideAllowList(Vec<String>),
    /// An allow-list entry is malformed (not an interface name, or a bad version).
    InvalidAllowList(String),
    /// An unexpected failure while sealing optional residuals.
    Internal(String),
}

impl fmt::Display for RestrictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiredOutsideAllowList(names) => write!(
                f,
                "required imports outside the allow-list: {}",
                names.join(", ")
            ),
            Self::InvalidAllowList(msg) => write!(f, "invalid allow-list: {msg}"),
            Self::Internal(msg) => write!(f, "internal restriction error: {msg}"),
        }
    }
}

impl Error for RestrictError {}

/// Errors from [`rename`](crate::rename).
///
/// Mirrors `rename-error` in `eo9:exec/component-algebra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// Neither an import nor an export slot has the old name.
    NoSuchSlot(String),
    /// A slot with the new name already exists on a side being renamed, or the new
    /// name is not usable as a slot name for this slot.
    SlotCollision(String),
    /// An unexpected failure in the underlying wiring/encoding machinery.
    Internal(String),
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchSlot(name) => write!(f, "no such slot: {name}"),
            Self::SlotCollision(msg) => write!(f, "slot collision: {msg}"),
            Self::Internal(msg) => write!(f, "internal rename error: {msg}"),
        }
    }
}

impl Error for RenameError {}
