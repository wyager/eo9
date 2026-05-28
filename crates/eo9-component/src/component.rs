//! The `Component` value: validated bytes plus the metadata `load` extracted from them.

use alloc::vec::Vec;

use crate::describe::Meta;
use crate::error::LoadError;
use crate::{ComponentInfo, ComponentKind};

/// An open program value: a binary or a provider (SPEC.md "Programs as values").
///
/// A `Component` is pure data -- naming or composing one never runs it. Every value of
/// this type holds bytes that have already been validated and classified by
/// [`Component::load`], so the algebra's operations can assume a well-formed Eo9 module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    bytes: Vec<u8>,
    meta: Meta,
}

impl Component {
    /// Validates `bytes` as a Component Model component and classifies it as an Eo9
    /// module (binary or provider).
    ///
    /// Mirrors `load` in `eo9:exec/component-algebra`.
    pub fn load(bytes: impl Into<Vec<u8>>) -> Result<Self, LoadError> {
        let bytes = bytes.into();
        let meta = Meta::from_bytes(&bytes)?;
        Ok(Self { bytes, meta })
    }

    /// The component's bytes, exactly as loaded or produced by an operation.
    ///
    /// Mirrors `save` in `eo9:exec/component-algebra`: `load(save(c))` is identical to
    /// `c`, byte for byte.
    pub fn save(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// Borrows the component's bytes without copying.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the component, returning its bytes without copying.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The component's bytes with every `implements` extern-name annotation stripped:
    /// the form to hand to an executor's `compile`.
    ///
    /// Plain-named slots (a renamed import such as `wallclock`, or a multi-instance
    /// consumer's named slots) carry an `implements` annotation recording the interface
    /// they are an instance of. The annotation is purely descriptive -- `describe`,
    /// wiring, and validation in this crate use it, but the canonical ABI never does --
    /// and the pinned runtime's component parser predates it, so compiling bytes that
    /// still carry one fails with an opaque parse error instead of running (or being
    /// refused for the missing import) cleanly. Stripping it changes nothing about the
    /// component's behavior; it only drops the describe-side identity of named slots,
    /// which an executor does not need. `bytes()`/`save()` keep the annotation so the
    /// algebra itself stays lossless.
    pub fn executable_bytes(&self) -> Vec<u8> {
        crate::externs::strip_implements(&self.bytes).unwrap_or_else(|_| self.bytes.clone())
    }

    /// Kind, imports, exports, and argument signature of the component.
    ///
    /// Mirrors `describe` in `eo9:exec/component-algebra`.
    pub fn describe(&self) -> ComponentInfo {
        self.meta.info()
    }

    /// Which of the two module kinds this component is.
    pub fn kind(&self) -> ComponentKind {
        self.meta.kind
    }

    /// The cached slot-level metadata (internal: richer than the public info).
    pub(crate) fn meta(&self) -> &Meta {
        &self.meta
    }
}
