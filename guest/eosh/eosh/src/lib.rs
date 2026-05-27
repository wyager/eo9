//! eosh — the Eo9 shell component.
//!
//! Targets the `eo9-eosh:eosh/eosh` world (see `wit/world.wit`): imports the execution
//! APIs (`eo9:exec/component-algebra`, `compile`, `task`), the text streams, and a
//! filesystem, and exports an async `main`. All of the language — the grammar, the
//! evaluator, argument handling, the builtins, the top-level rule — lives in the
//! `eosh-core` library; this crate only binds `eosh-core`'s [`Backend`] trait to the
//! real WIT imports and runs the read–eval loop.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eo9_guest::buffer;

use eosh_core::{
    Backend, BackendError, LineResult, Session,
    backend::{
        AbnormalExit, ArgSpec, ComponentInfo, ComponentKind, ExportSlot, ImportNeed, Outcome,
        WaveValue,
    },
};

mod bindings {
    // The eo9:text / eo9:fs / eo9:io interfaces are mapped onto the shared SDK modules
    // (the same Rust types every guest crate uses); the eo9:exec interfaces are not part
    // of the SDK world yet, so they are generated here.
    wit_bindgen::generate!({
        world: "eosh",
        generate_all,
        with: {
            "eo9:io/buffers@0.1.0": eo9_guest::api::io::buffers,
            "eo9:text/types@0.1.0": eo9_guest::api::text::types,
            "eo9:text/text@0.1.0": eo9_guest::api::text::text,
            "eo9:fs/types@0.1.0": eo9_guest::api::fs::types,
            "eo9:fs/fs@0.1.0": eo9_guest::api::fs::fs,
        },
    });
}

use bindings::eo9::exec::{compile, component_algebra, task};
use bindings::{Guest, ProgramFailure, ProgramSuccess, export};
use eo9_guest::api::fs::fs;
use eo9_guest::api::text::text;

/// The shell's [`Backend`]: `eosh-core` operations mapped one to one onto the WIT
/// imports. Name resolution follows the interim convention in
/// [`eosh_core::module_path`] (open `/bin/<name>.wasm` for execution, read it through
/// the immutable handle, `load` the bytes); the store-backed resolution of area 11
/// replaces only this `resolve` method.
struct WitBackend {
    text: text::TextImpl,
    fs: fs::FsImpl,
}

impl WitBackend {
    fn new() -> Self {
        WitBackend {
            text: text::default(),
            fs: fs::default(),
        }
    }

    fn write(&self, stream: text::OutputStream, line: &str) {
        // The shell cannot report an output failure anywhere but the output that just
        // failed; ignore the error and keep going.
        let _ = text::write(&self.text, stream, line);
        let _ = text::write(&self.text, stream, "\n");
    }

    /// Read the whole contents of an immutable execution handle.
    async fn read_exec(handle: &fs::ImmutableHandle) -> Result<Vec<u8>, BackendError> {
        let size = fs::exec_size(handle);
        let mut bytes = Vec::with_capacity(size as usize);
        while (bytes.len() as u64) < size {
            let offset = bytes.len() as u64;
            let chunk = buffer::with_capacity(size - offset);
            let (chunk, result) = fs::exec_read(handle, offset, chunk).await;
            let read = result.map_err(|err| fs_error("reading", err))?;
            if read.bytes_read == 0 {
                return Err(BackendError::new(
                    "reading the module ended early (zero-length read)",
                ));
            }
            bytes.extend_from_slice(&buffer::prefix_to_vec(&chunk, read.bytes_read));
        }
        Ok(bytes)
    }
}

fn fs_error(doing: &str, err: fs::FsError) -> BackendError {
    BackendError::new(format!("{doing} the module failed: {err:?}"))
}

fn algebra_error(operation: &str, err: impl core::fmt::Debug) -> BackendError {
    BackendError::new(format!("{operation} failed: {err:?}"))
}

/// Render an `only` failure as a sentence naming the offending imports, not as the raw
/// error variant (user studies flagged the debug form as unreadable).
fn restrict_error(err: component_algebra::RestrictError) -> BackendError {
    use component_algebra::RestrictError as E;
    BackendError::new(match err {
        E::RequiredOutsideAllowList(needs) => format!(
            "`only` refused: the program still requires {}, which the allow-list does not \
             include (allow it, compose a provider for it, or drop the requirement)",
            needs.join(", ")
        ),
        E::InvalidAllowList(msg) => format!("`only` refused: invalid allow-list: {msg}"),
        E::Internal(msg) => format!("`only` failed: {msg}"),
    })
}

/// Render a `$` / `&` failure in plain language.
fn compose_error(operation: &str, err: component_algebra::ComposeError) -> BackendError {
    use component_algebra::ComposeError as E;
    BackendError::new(match err {
        E::NotAProvider => format!(
            "{operation} refused: the left operand is not a provider (only providers can \
             satisfy imports)"
        ),
        E::TypeMismatch(msg) => {
            format!("{operation} refused: capability types do not match: {msg}")
        }
        E::Internal(msg) => format!("{operation} failed: {msg}"),
    })
}

/// Render a `configure` failure in plain language.
fn configure_error(err: component_algebra::ConfigureError) -> BackendError {
    use component_algebra::ConfigureError as E;
    BackendError::new(match err {
        E::NotAProvider => {
            "configure refused: only providers can be configured (this is a binary)".to_string()
        }
        E::NoConfigInterface => "configure refused: this provider takes no configuration \
             (or it was already configured)"
            .to_string(),
        E::InvalidArgs(msg) => format!("configure refused: {msg}"),
        E::Internal(msg) => format!("configure failed: {msg}"),
    })
}

/// Render a spawn failure. Linker-level "missing import" internals are translated into
/// the capability story (which interface the program needs and that this session does
/// not provide it) instead of leaking the raw instantiation error.
fn spawn_error(err: task::SpawnError) -> BackendError {
    use task::SpawnError as E;
    BackendError::new(match err {
        E::BadArguments(msg) => format!("bad arguments: {msg}"),
        E::Internal(msg) => match missing_capability(&msg) {
            Some(text) => text,
            None => format!("spawn failed: {msg}"),
        },
    })
}

/// If an internal spawn/instantiation error is about an unsatisfied `eo9:*` import,
/// describe it as a missing capability instead of leaking the raw linker text.
fn missing_capability(msg: &str) -> Option<String> {
    let capability = if msg.contains("eo9:exec/") {
        ("exec", "compose, compile, or spawn other programs")
    } else if msg.contains("eo9:fs/") || msg.contains("eo9:io/") {
        ("fs", "use a filesystem")
    } else if msg.contains("eo9:net/") {
        ("net", "use the network")
    } else {
        return None;
    };
    Some(format!(
        "the program requires the {} capability ({}), which this session does not provide \
         to it — grant it explicitly or compose a provider/stub for it",
        capability.0, capability.1
    ))
}

/// Map the generated `component-info` record into `eosh-core`'s mirror types.
fn info_from_wit(info: component_algebra::ComponentInfo) -> ComponentInfo {
    ComponentInfo {
        kind: match info.kind {
            component_algebra::ComponentKind::Binary => ComponentKind::Binary,
            component_algebra::ComponentKind::Provider => ComponentKind::Provider,
        },
        imports: info
            .imports
            .into_iter()
            .map(|need| ImportNeed {
                slot: need.slot,
                interface: need.interface,
                version: need.version,
                required: need.required,
            })
            .collect(),
        exports: info
            .exports
            .into_iter()
            .map(|slot| ExportSlot {
                name: slot.name,
                interface: slot.interface,
                version: slot.version,
            })
            .collect(),
        args: info
            .args
            .into_iter()
            .map(|arg| ArgSpec {
                name: arg.name,
                ty: arg.ty,
            })
            .collect(),
    }
}

/// Map the generated three-way `program-outcome` into `eosh-core`'s mirror type.
fn outcome_from_wit(outcome: task::ProgramOutcome) -> Outcome {
    match outcome {
        task::ProgramOutcome::Success(value) => Outcome::Success(WaveValue {
            ty: value.ty,
            value: value.value,
        }),
        task::ProgramOutcome::Failure(value) => Outcome::Failure(WaveValue {
            ty: value.ty,
            value: value.value,
        }),
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Trapped(reason)) => {
            Outcome::Abnormal(AbnormalExit::Trapped(reason))
        }
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Killed) => {
            Outcome::Abnormal(AbnormalExit::Killed)
        }
    }
}

impl Backend for WitBackend {
    type Component = component_algebra::Component;
    type Image = compile::Image;
    type Task = task::Task;

    async fn resolve(&mut self, name: &str) -> Result<Self::Component, BackendError> {
        let path = eosh_core::module_path(name);
        // `open-exec` is an async import, so its string argument is passed by value.
        let handle = fs::open_exec(&self.fs, path.clone()).await.map_err(|err| {
            BackendError::new(format!("cannot resolve `{name}` ({path}): {err:?}"))
        })?;
        let bytes = Self::read_exec(&handle).await?;
        component_algebra::load(&bytes)
            .map_err(|err| BackendError::new(format!("cannot load `{name}`: {err:?}")))
    }

    fn load(&mut self, bytes: &[u8]) -> Result<Self::Component, BackendError> {
        component_algebra::load(bytes).map_err(|err| algebra_error("load", err))
    }

    fn duplicate(&mut self, component: &Self::Component) -> Result<Self::Component, BackendError> {
        // Components are linear values in the algebra; a reusable copy is save + load.
        let bytes = component_algebra::save(component);
        component_algebra::load(&bytes).map_err(|err| algebra_error("duplicating (save/load)", err))
    }

    fn describe(&mut self, component: &Self::Component) -> ComponentInfo {
        info_from_wit(component_algebra::describe(component))
    }

    fn compose(
        &mut self,
        provider: Self::Component,
        consumer: Self::Component,
    ) -> Result<Self::Component, BackendError> {
        component_algebra::compose(provider, consumer).map_err(|err| compose_error("`$`", err))
    }

    fn extend(
        &mut self,
        base: Self::Component,
        layer: Self::Component,
    ) -> Result<Self::Component, BackendError> {
        component_algebra::extend(base, layer).map_err(|err| compose_error("`&`", err))
    }

    fn restrict(
        &mut self,
        component: Self::Component,
        allow: &[eosh_core::InterfaceRef],
    ) -> Result<Self::Component, BackendError> {
        let allow: Vec<component_algebra::InterfaceRef> = allow
            .iter()
            .map(|entry| component_algebra::InterfaceRef {
                interface: entry.interface.clone(),
                version: entry.version.clone(),
            })
            .collect();
        component_algebra::restrict(component, &allow).map_err(restrict_error)
    }

    fn rename(
        &mut self,
        component: Self::Component,
        from: &str,
        to: &str,
    ) -> Result<Self::Component, BackendError> {
        component_algebra::rename(component, from, to).map_err(|err| algebra_error("`rename`", err))
    }

    fn configure(
        &mut self,
        provider: Self::Component,
        args: &[eosh_core::NamedArg],
    ) -> Result<Self::Component, BackendError> {
        let args: Vec<component_algebra::NamedArg> = args
            .iter()
            .map(|arg| component_algebra::NamedArg {
                name: arg.name.clone(),
                value: arg.value.clone(),
            })
            .collect();
        component_algebra::configure(provider, &args).map_err(configure_error)
    }

    fn compile(&mut self, component: Self::Component) -> Result<Self::Image, BackendError> {
        let opts = compile::CompileOpts {
            debug_info: false,
            safepoint_maps: false,
        };
        compile::compile(component, opts).map_err(|err| algebra_error("compile", err))
    }

    fn spawn(
        &mut self,
        image: &Self::Image,
        args: &[eosh_core::NamedArg],
    ) -> Result<Self::Task, BackendError> {
        let args: Vec<task::NamedArg> = args
            .iter()
            .map(|arg| task::NamedArg {
                name: arg.name.clone(),
                value: arg.value.clone(),
            })
            .collect();
        let limits = task::SpawnLimits { max_memory: None };
        task::spawn(image, &args, limits).map_err(spawn_error)
    }

    async fn wait(&mut self, task: Self::Task) -> Outcome {
        outcome_from_wit(task::wait(&task).await)
    }

    async fn session_manifest(&mut self) -> Option<String> {
        // The embedder that built this session (usermode `eo9 shell`, later the kernel's
        // boot-to-shell) leaves a small manifest on the session filesystem describing
        // the grants (see eosh-core's `envinfo`). A missing or unreadable file just
        // means the information is unavailable; `env` says so and carries on.
        let path = eosh_core::SESSION_MANIFEST_PATH;
        let stat = fs::stat(&self.fs, path.to_string()).await.ok()?;
        let file = fs::open(&self.fs, path.to_string(), fs::OpenFlags::READ)
            .await
            .ok()?;
        let mut bytes: Vec<u8> = Vec::with_capacity(stat.size as usize);
        while (bytes.len() as u64) < stat.size {
            let offset = bytes.len() as u64;
            let chunk = buffer::with_capacity(stat.size - offset);
            let (chunk, result) = fs::read(&file, offset, chunk).await;
            let read = result.ok()?;
            if read.bytes_read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer::prefix_to_vec(&chunk, read.bytes_read));
        }
        String::from_utf8(bytes).ok()
    }

    fn print(&mut self, line: &str) {
        self.write(text::OutputStream::Out, line);
    }

    fn print_error(&mut self, line: &str) {
        self.write(text::OutputStream::Err, line);
    }
}

struct Eosh;

impl Guest for Eosh {
    async fn main(command: Option<String>) -> Result<ProgramSuccess, ProgramFailure> {
        let mut session = Session::new(WitBackend::new());

        match command {
            // One-shot mode: run the single command line and report its result as the
            // shell's own outcome.
            Some(line) => match session.execute_line(&line).await {
                LineResult::Ok | LineResult::Exit => Ok(ProgramSuccess::Exited),
                LineResult::ProgramFailed(rendered) | LineResult::Error(rendered) => {
                    Err(ProgramFailure::CommandFailed(rendered))
                }
            },
            // Interactive mode: read lines until end of input or `exit`.
            None => {
                let text = text::default();
                session
                    .backend_mut()
                    .print("eosh — the Eo9 shell (type `help`)");
                loop {
                    if text::write(&text, text::OutputStream::Out, "eosh> ").is_err() {
                        return Err(ProgramFailure::Io("writing the prompt failed".to_string()));
                    }
                    let line = text::read_line(&text).await.map_err(|err| {
                        ProgramFailure::Io(format!("reading a line failed: {err:?}"))
                    })?;
                    let Some(line) = line else {
                        // End of input.
                        return Ok(ProgramSuccess::Exited);
                    };
                    if session.execute_line(&line).await == LineResult::Exit {
                        return Ok(ProgramSuccess::Exited);
                    }
                }
            }
        }
    }
}

export!(Eosh with_types_in bindings);
