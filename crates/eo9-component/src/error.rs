//! Error types for the component algebra.
//!
//! Each enum mirrors the corresponding `variant` in `wit/exec/exec.wit`
//! (`load-error`, `compose-error`, `restrict-error`, `rename-error`) so the runtime can
//! translate between the two without losing information.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

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

/// Errors from [`configure`](crate::configure) -- binding a provider's compose-time
/// configuration constants.
///
/// The reference for the `configure-error` variant being added to
/// `eo9:exec/component-algebra` (area 02 mirrors this surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigureError {
    /// The operand is not a provider.
    NotAProvider,
    /// The provider exports no `*-config` interface, so there is nothing to bind --
    /// either it takes no configuration at all, or it has already been configured.
    NoConfigInterface,
    /// A supplied argument does not name a parameter of `configure`.
    UnknownArgument(String),
    /// A parameter of `configure` was not supplied.
    MissingArgument(String),
    /// An argument's WAVE value does not type-check against the declared parameter type
    /// (or was supplied more than once).
    InvalidArgument {
        /// The parameter name.
        name: String,
        /// What went wrong.
        message: String,
    },
    /// An unexpected failure in the underlying synthesis/wiring machinery, or a
    /// configuration signature this implementation cannot bake in yet.
    Internal(String),
}

impl fmt::Display for ConfigureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAProvider => write!(f, "operand is not a provider"),
            Self::NoConfigInterface => {
                write!(f, "the provider exports no `*-config` interface to bind")
            }
            Self::UnknownArgument(name) => {
                write!(f, "`{name}` is not a parameter of `configure`")
            }
            Self::MissingArgument(name) => {
                write!(f, "missing argument `{name}`")
            }
            Self::InvalidArgument { name, message } => {
                write!(f, "invalid argument `{name}`: {message}")
            }
            Self::Internal(msg) => write!(f, "internal configure error: {msg}"),
        }
    }
}

impl Error for ConfigureError {}

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
