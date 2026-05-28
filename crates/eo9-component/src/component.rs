//! The `Component` value: validated bytes plus the metadata `load` extracted from them.

use alloc::string::String;
use alloc::vec::Vec;

use crate::describe::Meta;
use crate::error::LoadError;
use crate::wiring::Wiring;
use crate::{ComponentInfo, ComponentKind};

/// An open program value: a binary or a provider (SPEC.md "Programs as values").
///
/// A `Component` is pure data -- naming or composing one never runs it. Every value of
/// this type holds bytes that have already been validated and classified by
/// [`Component::load`], so the algebra's operations can assume a well-formed Eo9 module.
///
/// A component also carries its composition [`Wiring`] -- in-memory provenance recording
/// how the algebra built it. Provenance is metadata only: it is NOT in the bytes and is
/// NOT part of equality (two components with identical bytes are equal regardless of how
/// each was built), so the content-addressed store and compile cache are unaffected.
#[derive(Debug, Clone)]
pub struct Component {
    bytes: Vec<u8>,
    meta: Meta,
    wiring: Wiring,
}

// Identity is byte identity (`meta` is a pure function of `bytes`); the in-memory wiring
// provenance is deliberately excluded so it cannot affect the content hash or any
// equality-keyed structure.
impl PartialEq for Component {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}
impl Eq for Component {}

impl Component {
    /// Validates `bytes` as a Component Model component and classifies it as an Eo9
    /// module (binary or provider).
    ///
    /// Mirrors `load` in `eo9:exec/component-algebra`. The result is a [`Wiring::Leaf`]:
    /// loading recovers no composition history (it is not in the bytes).
    pub fn load(bytes: impl Into<Vec<u8>>) -> Result<Self, LoadError> {
        let bytes = bytes.into();
        let meta = Meta::from_bytes(&bytes)?;
        let wiring = Wiring::leaf(&meta);
        Ok(Self {
            bytes,
            meta,
            wiring,
        })
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

    /// The component's composition provenance (see [`Wiring`]).
    pub fn wiring(&self) -> &Wiring {
        &self.wiring
    }

    /// Render the composition provenance as an indented tree (see [`Wiring::render`]).
    pub fn wiring_tree(&self) -> alloc::string::String {
        self.wiring.render()
    }

    /// Attach a human label to this component's leaf wiring (e.g. the store name it was
    /// resolved from), so a wiring tree can name it. A no-op once the component has been
    /// composed (only leaves carry a label).
    pub fn with_label(mut self, name: impl Into<String>) -> Self {
        self.wiring.set_label(name);
        self
    }

    /// Replace the component's wiring provenance (used by the algebra operations to record
    /// how a result was built). Never touches the bytes.
    pub(crate) fn with_wiring(mut self, wiring: Wiring) -> Self {
        self.wiring = wiring;
        self
    }

    /// The cached slot-level metadata (internal: richer than the public info).
    pub(crate) fn meta(&self) -> &Meta {
        &self.meta
    }
}
