//! `compile`: turning a closed binary component into an executable [`Image`].
//!
//! This is the host side of `eo9:exec/compile` — the privileged step that asks the TCB to
//! generate native code. An [`Image`] is an opaque compiled artifact; it is admitted for
//! execution via [`crate::task::Task::spawn`] and never read back as bytes *by guests*
//! (the WIT `image` resource from the types-only `images` interface). The host-side
//! [`Image::serialize`] / [`Image::deserialize`] pair below exists solely for the
//! compilation cache (SPEC "The module store and compilation cache"): the usermode `eo9`
//! binary stores serialized images keyed by everything codegen depends on and
//! short-circuits compilation on a hit.
//!
//! The input here is component *bytes* (or WAT text in tests): the component-algebra
//! `component` resource lives in area 03's crate and the two are joined at integration
//! (area 11) by passing the algebra's `save` output to [`Image::compile`].

use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime::component::types::ComponentItem;

/// Why a component could not be compiled into (or reloaded as) an [`Image`].
#[derive(Debug)]
pub enum CompileError {
    /// The component is a provider (or exports no `main`), not a binary.
    NotABinary,
    /// The bytes are not a valid component, or codegen failed.
    Codegen(String),
    /// The bytes are not a valid serialized image, or they were produced by an
    /// incompatible engine configuration or wasmtime version.
    BadImage(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::NotABinary => {
                write!(f, "component does not export `main`: not a binary")
            }
            CompileError::Codegen(msg) => write!(f, "compilation failed: {msg}"),
            CompileError::BadImage(msg) => {
                write!(f, "serialized image was rejected: {msg}")
            }
        }
    }
}

impl std::error::Error for CompileError {}

/// An opaque compiled artifact: a binary component compiled by the pinned engine,
/// ready to be spawned any number of times.
pub struct Image {
    engine: Engine,
    component: Component,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image").finish_non_exhaustive()
    }
}

impl Image {
    /// Compile a closed binary component into an image.
    ///
    /// `bytes` is a binary-encoded component (or WAT text, accepted for tests). The
    /// component must be a *binary* (export `main`); whether its imports are satisfiable
    /// is checked at spawn time, when the root providers are known (the loader rule from
    /// SPEC.md "WASM runtime").
    pub fn compile(engine: &Engine, bytes: impl AsRef<[u8]>) -> Result<Self, CompileError> {
        let component = Component::new(engine, bytes)
            .map_err(|err| CompileError::Codegen(format!("{err:#}")))?;
        Self::from_component(engine, component)
    }

    /// Serialize this image into bytes that can later be passed to [`Image::deserialize`],
    /// skipping codegen entirely.
    ///
    /// # Compatibility (cache-key requirements for area 11)
    ///
    /// The bytes are native code plus metadata produced by this exact build of wasmtime
    /// under this exact engine configuration. [`Image::deserialize`] verifies that
    /// metadata and rejects anything produced by a different wasmtime version, a different
    /// target, or different compile-relevant settings (which for Eo9 means the
    /// [`EngineOptions`](crate::engine::EngineOptions) used, since everything else is
    /// pinned in [`crate::engine::config`]). A compilation-cache key must therefore
    /// include, at minimum, the wasmtime version, the target, and the `EngineOptions` —
    /// [`crate::engine::compatibility_hash`] folds all of that into one value — in
    /// addition to the content hash of the component bytes themselves.
    pub fn serialize(&self) -> Result<Vec<u8>, CompileError> {
        self.component
            .serialize()
            .map_err(|err| CompileError::Codegen(format!("image serialization failed: {err:#}")))
    }

    /// Reconstruct an image from bytes previously produced by [`Image::serialize`],
    /// without recompiling.
    ///
    /// Compatibility with the given `engine` (wasmtime version, target, compile settings)
    /// is checked and mismatches are rejected with [`CompileError::BadImage`]; so are
    /// bytes that are not a serialized component image at all. See [`Image::serialize`]
    /// for what a cache key must cover.
    ///
    /// # Safety
    ///
    /// The compatibility check is not an integrity or authenticity check: the bytes map
    /// native code into the process, so a maliciously crafted input could subvert the
    /// host. Callers must only pass bytes that were produced by [`Image::serialize`] and
    /// have been stored and retrieved through a trusted channel (for the usermode cache:
    /// the content-addressed store, with the content hash verified on the way out).
    pub unsafe fn deserialize(engine: &Engine, bytes: &[u8]) -> Result<Self, CompileError> {
        // SAFETY: forwarded to the caller — see this function's safety contract.
        let component = unsafe { Component::deserialize(engine, bytes) }
            .map_err(|err| CompileError::BadImage(format!("{err:#}")))?;
        Self::from_component(engine, component)
    }

    /// Shared tail of [`Image::compile`] and [`Image::deserialize`]: enforce the
    /// binary-kind rule and wrap up.
    fn from_component(engine: &Engine, component: Component) -> Result<Self, CompileError> {
        let exports_main = component
            .component_type()
            .exports(engine)
            .any(|(name, item)| name == "main" && matches!(item, ComponentItem::ComponentFunc(_)));
        if !exports_main {
            return Err(CompileError::NotABinary);
        }

        Ok(Self {
            engine: engine.clone(),
            component,
        })
    }

    /// The engine this image was compiled by (and must be spawned under).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The compiled component (crate-internal: spawn needs it for instantiation).
    pub(crate) fn component(&self) -> &Component {
        &self.component
    }
}
