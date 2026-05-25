//! `compile`: turning a closed binary component into an executable [`Image`].
//!
//! This is the host side of `eo9:exec/compile` — the privileged step that asks the TCB to
//! generate native code. An [`Image`] is an opaque compiled artifact; it is admitted for
//! execution via [`crate::task::Task::spawn`] and never read back as bytes (the WIT
//! `image` resource from the types-only `images` interface).
//!
//! The input here is component *bytes* (or WAT text in tests): the component-algebra
//! `component` resource lives in area 03's crate and the two are joined at integration
//! (area 11) by passing the algebra's `save` output to [`Image::compile`].

use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime::component::types::ComponentItem;

/// Why a component could not be compiled into an [`Image`].
#[derive(Debug)]
pub enum CompileError {
    /// The component is a provider (or exports no `main`), not a binary.
    NotABinary,
    /// The bytes are not a valid component, or codegen failed.
    Codegen(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::NotABinary => {
                write!(f, "component does not export `main`: not a binary")
            }
            CompileError::Codegen(msg) => write!(f, "compilation failed: {msg}"),
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
