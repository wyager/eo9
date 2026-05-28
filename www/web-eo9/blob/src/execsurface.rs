//! The `eo9:exec` surface in the browser — the capability eosh imports to compose, compile,
//! and run programs. The real component algebra (`eo9-component`) backs the
//! `component-algebra` interface; `compile` is an artifact lookup against the page's
//! pre-AOT'd store (there is no in-blob codegen — `$`/`&` fusing new bytes is refused with a
//! clean message); `task` runs a child program to completion (single-child, no concurrency:
//! eosh awaits each command fully before reading the next line).
//!
//! Hand-rolled on the raw component `Linker` (the same approach as `providers.rs`/`fs.rs`),
//! not `bindgen!`: the blob deliberately hand-wires every host interface because the SDK
//! path is finicky with the custom platform + fiberless component-model-async configuration.

use std::string::{String, ToString};
use std::vec::Vec;

use eo9_component::{
    Component, ComponentKind, ComposeError, ConfigureError, InterfaceRef, LoadError, RenameError,
    RestrictError, compose, configure, extend, rename, restrict,
};
use wasmtime::component::{
    Accessor, Component as WtComponent, ComponentType, Lift, Linker, Lower, Resource, ResourceType,
    Val,
};
use wasmtime::{Result, Store, StoreContextMut};

use crate::providers::WebState;
use crate::store::render_val;
use crate::{block_on, engine};

// The programs eosh can resolve from `/bin` in the browser: raw component bytes (for the
// algebra's `load`) and the pre-AOT'd pulley32 artifact (for execution). Embedded by
// `cargo xtask build-web-vm`. (hello today; more programs follow once their raw+pulley
// forms are seeded — recorded in plan/18.)
static HELLO_RAW: &[u8] = include_bytes!("../artifacts/example-hello.wasm");
static HELLO_PULLEY: &[u8] = include_bytes!("../artifacts/example-hello.cwasm");

struct BinProgram {
    name: &'static str,
    raw: &'static [u8],
    pulley: &'static [u8],
}

static BIN: &[BinProgram] = &[BinProgram {
    name: "hello",
    raw: HELLO_RAW,
    pulley: HELLO_PULLEY,
}];

/// Seed `/bin/<name>.wasm` with each program's raw component bytes so eosh's `resolve`
/// (which opens `/bin/<name>.wasm` for execution and `load`s the bytes) finds them.
pub fn seed_bin(fs: &mut crate::fs::MemFs) {
    fs.seed_dir("/bin");
    for program in BIN {
        fs.seed_file(&std::format!("/bin/{}.wasm", program.name), program.raw);
    }
}

/// Cheap content hash (FNV-1a, the same family the server/asset fingerprinting uses) — maps
/// a loaded component's raw bytes to its pre-AOT'd artifact, so `compile` of a plain program
/// finds the `.cwasm` to run without in-blob codegen.
fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn artifact_for(raw: &[u8]) -> Option<Vec<u8>> {
    let want = hash(raw);
    BIN.iter()
        .find(|p| hash(p.raw) == want)
        .map(|p| p.pulley.to_vec())
}

// --- Resource tables (rep = index; freed by the resource destructors) --------------------

struct ComponentEntry {
    component: Component,
    /// The pre-AOT'd artifact to run this component, if known (a plain program, or an
    /// `only`/`rename` of one — those keep the base artifact). `None` for `compose`/`extend`/
    /// `configure` results, which produce new bytes with no precompiled form.
    artifact: Option<Vec<u8>>,
    /// When this component was produced by `only`, the admitted interfaces — used to
    /// restrict the child's linker at spawn (the algebra sealed them in the bytes, but we
    /// run the base artifact, so the restriction is reapplied as a narrowed linker).
    allow: Option<Vec<String>>,
}

struct ImageEntry {
    artifact: Vec<u8>,
    allow: Option<Vec<String>>,
    arg_specs: Vec<(String, String)>,
}

#[derive(Default)]
pub struct ExecTables {
    components: Vec<Option<ComponentEntry>>,
    images: Vec<Option<ImageEntry>>,
    tasks: Vec<Option<ProgramOutcome>>,
}

impl ExecTables {
    fn put_component(&mut self, entry: ComponentEntry) -> u32 {
        let rep = self.components.len() as u32;
        self.components.push(Some(entry));
        rep
    }
    fn component(&self, rep: u32) -> Result<&ComponentEntry> {
        self.components
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| wasmtime::Error::msg("no such component handle"))
    }
    fn take_component(&mut self, rep: u32) -> Result<ComponentEntry> {
        self.components
            .get_mut(rep as usize)
            .and_then(Option::take)
            .ok_or_else(|| wasmtime::Error::msg("no such component handle"))
    }
    fn free_component(&mut self, rep: u32) {
        if let Some(slot) = self.components.get_mut(rep as usize) {
            *slot = None;
        }
    }
    fn put_image(&mut self, entry: ImageEntry) -> u32 {
        let rep = self.images.len() as u32;
        self.images.push(Some(entry));
        rep
    }
    fn image(&self, rep: u32) -> Result<&ImageEntry> {
        self.images
            .get(rep as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| wasmtime::Error::msg("no such image handle"))
    }
    fn free_image(&mut self, rep: u32) {
        if let Some(slot) = self.images.get_mut(rep as usize) {
            *slot = None;
        }
    }
    fn put_task(&mut self, outcome: ProgramOutcome) -> u32 {
        let rep = self.tasks.len() as u32;
        self.tasks.push(Some(outcome));
        rep
    }
    fn task(&self, rep: u32) -> Result<ProgramOutcome> {
        self.tasks
            .get(rep as usize)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| wasmtime::Error::msg("no such task handle"))
    }
    fn free_task(&mut self, rep: u32) {
        if let Some(slot) = self.tasks.get_mut(rep as usize) {
            *slot = None;
        }
    }
}

impl WebState {
    fn exec(&mut self) -> &mut ExecTables {
        &mut self.exec
    }
}

// --- Resource markers --------------------------------------------------------------------

struct ComponentRes;
struct ImageRes;
struct TaskRes;

// --- WIT-shaped types (mirror eo9:exec) --------------------------------------------------

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
enum WitKind {
    #[component(name = "binary")]
    Binary,
    #[component(name = "provider")]
    Provider,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitArgSpec {
    name: String,
    ty: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitNamedArg {
    name: String,
    value: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitImportNeed {
    slot: String,
    #[component(name = "interface")]
    interface: String,
    version: String,
    required: bool,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitExportSlot {
    name: String,
    #[component(name = "interface")]
    interface: String,
    version: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitComponentInfo {
    kind: WitKind,
    imports: Vec<WitImportNeed>,
    exports: Vec<WitExportSlot>,
    args: Vec<WitArgSpec>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitInterfaceRef {
    #[component(name = "interface")]
    interface: String,
    version: Option<String>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitLoadError {
    #[component(name = "invalid-component")]
    InvalidComponent(String),
    #[component(name = "not-an-eo9-module")]
    NotAnEo9Module(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitComposeError {
    #[component(name = "not-a-provider")]
    NotAProvider,
    #[component(name = "type-mismatch")]
    TypeMismatch(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitRestrictError {
    #[component(name = "required-outside-allow-list")]
    RequiredOutsideAllowList(Vec<String>),
    #[component(name = "invalid-allow-list")]
    InvalidAllowList(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitRenameError {
    #[component(name = "no-such-slot")]
    NoSuchSlot(String),
    #[component(name = "slot-collision")]
    SlotCollision(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitConfigureError {
    #[component(name = "not-a-provider")]
    NotAProvider,
    #[component(name = "no-config-interface")]
    NoConfigInterface,
    #[component(name = "invalid-args")]
    InvalidArgs(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitCompileOpts {
    #[component(name = "debug-info")]
    debug_info: bool,
    #[component(name = "safepoint-maps")]
    safepoint_maps: bool,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitCompileError {
    #[component(name = "not-a-binary")]
    NotABinary,
    #[component(name = "not-closed")]
    NotClosed(Vec<String>),
    #[component(name = "codegen")]
    Codegen(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(record)]
struct WitWaveValue {
    ty: String,
    value: String,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitAbnormalExit {
    #[component(name = "trapped")]
    Trapped(String),
    #[component(name = "killed")]
    Killed,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum ProgramOutcome {
    #[component(name = "success")]
    Success(WitWaveValue),
    #[component(name = "failure")]
    Failure(WitWaveValue),
    #[component(name = "abnormal")]
    Abnormal(WitAbnormalExit),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitSpawnLimits {
    #[component(name = "max-memory")]
    max_memory: Option<u64>,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitSpawnError {
    #[component(name = "bad-arguments")]
    BadArguments(String),
    #[component(name = "internal")]
    Internal(String),
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
enum WitResumeOutcome {
    #[component(name = "out-of-fuel")]
    OutOfFuel,
    #[component(name = "blocked")]
    Blocked,
    #[component(name = "done")]
    Done(ProgramOutcome),
}

// --- mapping helpers ----------------------------------------------------------------------

fn info_to_wit(info: eo9_component::ComponentInfo) -> WitComponentInfo {
    WitComponentInfo {
        kind: match info.kind {
            ComponentKind::Binary => WitKind::Binary,
            ComponentKind::Provider => WitKind::Provider,
        },
        imports: info
            .imports
            .into_iter()
            .map(|i| WitImportNeed {
                slot: i.slot,
                interface: i.interface,
                version: i.version,
                required: i.required,
            })
            .collect(),
        exports: info
            .exports
            .into_iter()
            .map(|e| WitExportSlot {
                name: e.name,
                interface: e.interface,
                version: e.version,
            })
            .collect(),
        args: info
            .args
            .into_iter()
            .map(|a| WitArgSpec {
                name: a.name,
                ty: a.ty,
            })
            .collect(),
    }
}

fn load_err(e: LoadError) -> WitLoadError {
    match e {
        LoadError::InvalidComponent(m) => WitLoadError::InvalidComponent(m),
        LoadError::NotAnEo9Module(m) => WitLoadError::NotAnEo9Module(m),
    }
}

fn compose_err(e: ComposeError) -> WitComposeError {
    match e {
        ComposeError::NotAProvider => WitComposeError::NotAProvider,
        ComposeError::TypeMismatch(m) => WitComposeError::TypeMismatch(m),
        ComposeError::Internal(m) => WitComposeError::Internal(m),
    }
}

fn restrict_err(e: RestrictError) -> WitRestrictError {
    match e {
        RestrictError::RequiredOutsideAllowList(v) => WitRestrictError::RequiredOutsideAllowList(v),
        RestrictError::InvalidAllowList(m) => WitRestrictError::InvalidAllowList(m),
        RestrictError::Internal(m) => WitRestrictError::Internal(m),
    }
}

fn rename_err(e: RenameError) -> WitRenameError {
    match e {
        RenameError::NoSuchSlot(m) => WitRenameError::NoSuchSlot(m),
        RenameError::SlotCollision(m) => WitRenameError::SlotCollision(m),
        RenameError::Internal(m) => WitRenameError::Internal(m),
    }
}

fn configure_err(e: ConfigureError) -> WitConfigureError {
    match e {
        ConfigureError::NotAProvider => WitConfigureError::NotAProvider,
        ConfigureError::NoConfigInterface => WitConfigureError::NoConfigInterface,
        ConfigureError::UnknownArgument(m) => {
            WitConfigureError::InvalidArgs(std::format!("unknown argument `{m}`"))
        }
        ConfigureError::MissingArgument(m) => {
            WitConfigureError::InvalidArgs(std::format!("missing argument `{m}`"))
        }
        ConfigureError::InvalidArgument { name, message } => {
            WitConfigureError::InvalidArgs(std::format!("`{name}`: {message}"))
        }
        ConfigureError::Internal(m) => WitConfigureError::Internal(m),
    }
}

// --- WAVE-lite argument parsing -----------------------------------------------------------

/// Parse a WAVE scalar value against its declared type text into a wasmtime `Val`. Covers
/// the value shapes the page's programs use (string/bool/integers/char/option); anything
/// else is reported, so an unsupported argument fails cleanly rather than silently.
fn wave_to_val(ty: &str, value: &str) -> std::result::Result<Val, String> {
    let ty = ty.trim();
    let value = value.trim();
    if let Some(inner) = ty.strip_prefix("option<").and_then(|t| t.strip_suffix('>')) {
        if value == "none" {
            return Ok(Val::Option(None));
        }
        let body = value
            .strip_prefix("some(")
            .and_then(|v| v.strip_suffix(')'))
            .unwrap_or(value);
        return Ok(Val::Option(Some(std::boxed::Box::new(wave_to_val(
            inner, body,
        )?))));
    }
    match ty {
        "string" => {
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value)
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            Ok(Val::String(unquoted))
        }
        "bool" => match value {
            "true" => Ok(Val::Bool(true)),
            "false" => Ok(Val::Bool(false)),
            other => Err(std::format!("`{other}` is not a bool")),
        },
        "u8" => value.parse().map(Val::U8).map_err(|_| bad(value, ty)),
        "u16" => value.parse().map(Val::U16).map_err(|_| bad(value, ty)),
        "u32" => value.parse().map(Val::U32).map_err(|_| bad(value, ty)),
        "u64" => value.parse().map(Val::U64).map_err(|_| bad(value, ty)),
        "s8" => value.parse().map(Val::S8).map_err(|_| bad(value, ty)),
        "s16" => value.parse().map(Val::S16).map_err(|_| bad(value, ty)),
        "s32" => value.parse().map(Val::S32).map_err(|_| bad(value, ty)),
        "s64" => value.parse().map(Val::S64).map_err(|_| bad(value, ty)),
        "char" => {
            let inner = value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .unwrap_or(value);
            inner
                .chars()
                .next()
                .filter(|_| inner.chars().count() == 1)
                .map(Val::Char)
                .ok_or_else(|| bad(value, ty))
        }
        other => Err(std::format!(
            "the browser shell cannot bind arguments of type `{other}` yet"
        )),
    }
}

fn bad(value: &str, ty: &str) -> String {
    std::format!("`{value}` is not a {ty}")
}

/// Interfaces the browser executor's root environment serves to a spawned program — a
/// binary importing only these (plus authority-free types) is runnable.
fn is_root_provided(interface: &str) -> bool {
    matches!(
        interface,
        "eo9:text/text" | "eo9:time/time" | "eo9:entropy/entropy" | "eo9:fs/fs" | "eo9:io/buffers"
    )
}

/// Order the named args against the component's arg specs and bind each to a `Val`.
fn bind_args(
    specs: &[(String, String)],
    args: &[WitNamedArg],
) -> std::result::Result<Vec<Val>, String> {
    let mut vals = Vec::with_capacity(specs.len());
    for (name, ty) in specs {
        let matching: Vec<&WitNamedArg> = args.iter().filter(|a| a.name == *name).collect();
        let arg = match matching.as_slice() {
            [] => return Err(std::format!("missing argument `{name}`")),
            [a] => *a,
            _ => return Err(std::format!("argument `{name}` supplied more than once")),
        };
        vals.push(wave_to_val(ty, &arg.value)?);
    }
    for a in args {
        if !specs.iter().any(|(name, _)| *name == a.name) {
            return Err(std::format!("unknown argument `{}`", a.name));
        }
    }
    Ok(vals)
}

// --- running a child program to completion (single-child, run-to-completion) -------------

/// Map a program's `result<success, failure>` return value to a `program-outcome`.
fn val_to_outcome(val: &Val) -> ProgramOutcome {
    let wave = |v: &Val| WitWaveValue {
        ty: String::new(),
        value: render_val(v),
    };
    match val {
        Val::Result(Ok(Some(v))) => ProgramOutcome::Success(wave(v)),
        Val::Result(Ok(None)) => ProgramOutcome::Success(WitWaveValue {
            ty: String::new(),
            value: String::new(),
        }),
        Val::Result(Err(Some(v))) => ProgramOutcome::Failure(wave(v)),
        Val::Result(Err(None)) => ProgramOutcome::Failure(WitWaveValue {
            ty: String::new(),
            value: String::new(),
        }),
        other => ProgramOutcome::Success(wave(other)),
    }
}

/// Instantiate a child artifact against the (possibly restricted) browser root providers and
/// run `main` to completion. A separate `Store`/`Engine` from the caller — eosh awaits the
/// whole call, so there is at most one child running at a time.
fn run_child(artifact: &[u8], _allow: Option<&[String]>, vals: Vec<Val>) -> ProgramOutcome {
    match run_child_inner(artifact, vals) {
        Ok(outcome) => outcome,
        Err(error) => ProgramOutcome::Abnormal(WitAbnormalExit::Trapped(std::format!("{error}"))),
    }
}

fn run_child_inner(artifact: &[u8], vals: Vec<Val>) -> Result<ProgramOutcome> {
    let engine = engine(false)?;
    // SAFETY: produced by `cargo xtask build-web-vm` with the matching configuration.
    let component = unsafe { WtComponent::deserialize(&engine, artifact)? };
    let mut linker: Linker<WebState> = Linker::new(&engine);
    crate::providers::add_providers(&mut linker)?;
    crate::fs::add_fs_io(&mut linker)?;
    let mut store = Store::new(&engine, WebState::new());
    let instance = block_on(
        "child instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;
    let index = instance
        .get_export_index(&mut store, None, "main")
        .ok_or_else(|| wasmtime::Error::msg("the program does not export `main`"))?;
    let main = instance
        .get_func(&mut store, index)
        .ok_or_else(|| wasmtime::Error::msg("`main` is not a function"))?;
    let outcome = block_on(
        "child main",
        store.run_concurrent(async move |accessor| -> Result<Val> {
            let mut result = [Val::Bool(false)];
            main.call_concurrent(accessor, &vals, &mut result).await?;
            Ok(result[0].clone())
        }),
    )???;
    Ok(val_to_outcome(&outcome))
}

// --- registration -------------------------------------------------------------------------

/// Register the `eo9:exec` surface eosh imports on the linker.
pub fn add_exec(linker: &mut Linker<WebState>) -> Result<()> {
    add_component_algebra(linker)?;
    add_images(linker)?;
    add_compile(linker)?;
    add_task(linker)?;
    Ok(())
}

fn add_component_algebra(linker: &mut Linker<WebState>) -> Result<()> {
    let mut ca = linker.instance("eo9:exec/component-algebra@0.1.0")?;
    ca.resource(
        "component",
        ResourceType::host::<ComponentRes>(),
        |mut store: StoreContextMut<'_, WebState>, rep| {
            store.data_mut().exec().free_component(rep);
            Ok(())
        },
    )?;

    ca.func_wrap(
        "load",
        |mut store: StoreContextMut<'_, WebState>,
         (image,): (Vec<u8>,)|
         -> Result<(std::result::Result<Resource<ComponentRes>, WitLoadError>,)> {
            match Component::load(image.clone()) {
                Ok(component) => {
                    let artifact = artifact_for(&image);
                    let rep = store.data_mut().exec().put_component(ComponentEntry {
                        component,
                        artifact,
                        allow: None,
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                Err(e) => Ok((Err(load_err(e)),)),
            }
        },
    )?;

    ca.func_wrap(
        "save",
        |mut store: StoreContextMut<'_, WebState>,
         (c,): (Resource<ComponentRes>,)|
         -> Result<(Vec<u8>,)> {
            let bytes = store.data_mut().exec().component(c.rep())?.component.save();
            Ok((bytes,))
        },
    )?;

    ca.func_wrap(
        "describe",
        |mut store: StoreContextMut<'_, WebState>,
         (c,): (Resource<ComponentRes>,)|
         -> Result<(WitComponentInfo,)> {
            let info = store
                .data_mut()
                .exec()
                .component(c.rep())?
                .component
                .describe();
            Ok((info_to_wit(info),))
        },
    )?;

    ca.func_wrap(
        "compose",
        |mut store: StoreContextMut<'_, WebState>,
         (p, c): (Resource<ComponentRes>, Resource<ComponentRes>)|
         -> Result<(std::result::Result<Resource<ComponentRes>, WitComposeError>,)> {
            let exec = store.data_mut().exec();
            let provider = exec.take_component(p.rep())?;
            let consumer = exec.take_component(c.rep())?;
            match compose(&provider.component, &consumer.component) {
                Ok(component) => {
                    let rep = store.data_mut().exec().put_component(ComponentEntry {
                        component,
                        artifact: None,
                        allow: consumer.allow,
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                Err(e) => Ok((Err(compose_err(e)),)),
            }
        },
    )?;

    ca.func_wrap(
        "extend",
        |mut store: StoreContextMut<'_, WebState>,
         (base, layer): (Resource<ComponentRes>, Resource<ComponentRes>)|
         -> Result<(std::result::Result<Resource<ComponentRes>, WitComposeError>,)> {
            let exec = store.data_mut().exec();
            let base_e = exec.take_component(base.rep())?;
            let layer_e = exec.take_component(layer.rep())?;
            match extend(&base_e.component, &layer_e.component) {
                Ok(component) => {
                    let rep = store.data_mut().exec().put_component(ComponentEntry {
                        component,
                        artifact: None,
                        allow: base_e.allow,
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                Err(e) => Ok((Err(compose_err(e)),)),
            }
        },
    )?;

    ca.func_wrap(
        "restrict",
        |mut store: StoreContextMut<'_, WebState>,
         (c, allow): (Resource<ComponentRes>, Vec<WitInterfaceRef>)|
         -> Result<(std::result::Result<Resource<ComponentRes>, WitRestrictError>,)> {
            let entry = store.data_mut().exec().take_component(c.rep())?;
            let refs: Vec<InterfaceRef> = allow
                .iter()
                .map(|r| InterfaceRef {
                    interface: r.interface.clone(),
                    version: r.version.clone(),
                })
                .collect();
            match restrict(&entry.component, &refs) {
                Ok(component) => {
                    let admitted: Vec<String> = allow.iter().map(|r| r.interface.clone()).collect();
                    let rep = store.data_mut().exec().put_component(ComponentEntry {
                        component,
                        artifact: entry.artifact,
                        allow: Some(admitted),
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                Err(e) => Ok((Err(restrict_err(e)),)),
            }
        },
    )?;

    ca.func_wrap(
        "rename",
        |mut store: StoreContextMut<'_, WebState>,
         (c, old_name, new_name): (Resource<ComponentRes>, String, String)|
         -> Result<(std::result::Result<Resource<ComponentRes>, WitRenameError>,)> {
            let entry = store.data_mut().exec().take_component(c.rep())?;
            match rename(&entry.component, &old_name, &new_name) {
                Ok(component) => {
                    let rep = store.data_mut().exec().put_component(ComponentEntry {
                        component,
                        artifact: entry.artifact,
                        allow: entry.allow,
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                Err(e) => Ok((Err(rename_err(e)),)),
            }
        },
    )?;

    ca.func_wrap(
        "configure",
        |mut store: StoreContextMut<'_, WebState>,
         (p, args): (Resource<ComponentRes>, Vec<WitNamedArg>)|
         -> Result<(std::result::Result<Resource<ComponentRes>, WitConfigureError>,)> {
            let entry = store.data_mut().exec().take_component(p.rep())?;
            let pairs: Vec<(String, String)> =
                args.into_iter().map(|a| (a.name, a.value)).collect();
            match configure(&entry.component, &pairs) {
                Ok(component) => {
                    let rep = store.data_mut().exec().put_component(ComponentEntry {
                        component,
                        artifact: None,
                        allow: None,
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                Err(e) => Ok((Err(configure_err(e)),)),
            }
        },
    )?;

    Ok(())
}

fn add_images(linker: &mut Linker<WebState>) -> Result<()> {
    linker.instance("eo9:exec/images@0.1.0")?.resource(
        "image",
        ResourceType::host::<ImageRes>(),
        |mut store: StoreContextMut<'_, WebState>, rep| {
            store.data_mut().exec().free_image(rep);
            Ok(())
        },
    )?;
    Ok(())
}

fn add_compile(linker: &mut Linker<WebState>) -> Result<()> {
    let mut compile = linker.instance("eo9:exec/compile@0.1.0")?;
    compile.func_wrap(
        "compile",
        |mut store: StoreContextMut<'_, WebState>,
         (c, _opts): (Resource<ComponentRes>, WitCompileOpts)|
         -> Result<(std::result::Result<Resource<ImageRes>, WitCompileError>,)> {
            let entry = store.data_mut().exec().take_component(c.rep())?;
            let info = entry.component.describe();
            if info.kind != ComponentKind::Binary {
                return Ok((Err(WitCompileError::NotABinary),));
            }
            // A binary is runnable if its required imports are all satisfiable by the
            // executor's root environment (the browser root providers below) — exactly as a
            // bare `hello` runs on the kernel, its text/time imports served at spawn. Only a
            // required import the root environment cannot provide is genuinely unmet.
            let unmet: Vec<String> = info
                .imports
                .iter()
                .filter(|i| i.required && !i.authority_free && !is_root_provided(&i.interface))
                .map(|i| i.interface.clone())
                .collect();
            if !unmet.is_empty() {
                return Ok((Err(WitCompileError::NotClosed(unmet)),));
            }
            match entry.artifact {
                Some(artifact) => {
                    let arg_specs = info.args.into_iter().map(|a| (a.name, a.ty)).collect();
                    let rep = store.data_mut().exec().put_image(ImageEntry {
                        artifact,
                        allow: entry.allow,
                        arg_specs,
                    });
                    Ok((Ok(Resource::new_own(rep)),))
                }
                None => Ok((Err(WitCompileError::Codegen(
                    "composition needs the compiler, which isn't available in the browser yet \
                     (in-blob codegen is std/mmap-blocked); native Eo9 and the bare-metal kernel \
                     compile it on-target"
                        .to_string(),
                )),)),
            }
        },
    )?;
    Ok(())
}

fn add_task(linker: &mut Linker<WebState>) -> Result<()> {
    let mut task = linker.instance("eo9:exec/task@0.1.0")?;
    task.resource(
        "task",
        ResourceType::host::<TaskRes>(),
        |mut store: StoreContextMut<'_, WebState>, rep| {
            store.data_mut().exec().free_task(rep);
            Ok(())
        },
    )?;

    // spawn: run the program to completion now (single-child), store its outcome.
    task.func_wrap(
        "spawn",
        |mut store: StoreContextMut<'_, WebState>,
         (image, args, _limits): (Resource<ImageRes>, Vec<WitNamedArg>, WitSpawnLimits)|
         -> Result<(std::result::Result<Resource<TaskRes>, WitSpawnError>,)> {
            let (artifact, allow, specs) = {
                let entry = store.data_mut().exec().image(image.rep())?;
                (
                    entry.artifact.clone(),
                    entry.allow.clone(),
                    entry.arg_specs.clone(),
                )
            };
            let vals = match bind_args(&specs, &args) {
                Ok(vals) => vals,
                Err(message) => return Ok((Err(WitSpawnError::BadArguments(message)),)),
            };
            let outcome = run_child(&artifact, allow.as_deref(), vals);
            let rep = store.data_mut().exec().put_task(outcome);
            Ok((Ok(Resource::new_own(rep)),))
        },
    )?;

    // resume: the task already ran to completion; report it done.
    task.func_wrap(
        "resume",
        |mut store: StoreContextMut<'_, WebState>,
         (t, _fuel): (Resource<TaskRes>, u64)|
         -> Result<(WitResumeOutcome,)> {
            let outcome = store.data_mut().exec().task(t.rep())?;
            Ok((WitResumeOutcome::Done(outcome),))
        },
    )?;

    // runnable: always immediately runnable (it already completed).
    task.func_wrap_concurrent(
        "runnable",
        |_accessor: &Accessor<WebState>, (_t,): (Resource<TaskRes>,)| {
            std::boxed::Box::pin(async move { Ok(()) })
        },
    )?;

    // wait: return the stored outcome.
    task.func_wrap_concurrent(
        "wait",
        |accessor: &Accessor<WebState>,
         (t,): (Resource<TaskRes>,)|
         -> core::pin::Pin<
            std::boxed::Box<dyn core::future::Future<Output = Result<(ProgramOutcome,)>> + Send>,
        > {
            std::boxed::Box::pin(async move {
                let outcome = accessor.with(|mut access| access.data_mut().exec().task(t.rep()))?;
                Ok((outcome,))
            })
        },
    )?;

    // kill: the task already finished; return its outcome.
    task.func_wrap_concurrent(
        "kill",
        |accessor: &Accessor<WebState>,
         (t,): (Resource<TaskRes>,)|
         -> core::pin::Pin<
            std::boxed::Box<dyn core::future::Future<Output = Result<(ProgramOutcome,)>> + Send>,
        > {
            std::boxed::Box::pin(async move {
                let outcome = accessor.with(|mut access| access.data_mut().exec().task(t.rep()))?;
                Ok((outcome,))
            })
        },
    )?;

    Ok(())
}

// --- booting eosh -------------------------------------------------------------------------

static EOSH_PULLEY: &[u8] = include_bytes!("../artifacts/eosh.cwasm");

/// Instantiate eosh against the browser root providers + fs/io + the exec surface. The
/// floor: proves eosh links against the in-blob `eo9:exec` surface.
pub fn boot_eosh_instantiate() -> Result<()> {
    let engine = engine(false)?;
    // SAFETY: produced by `cargo xtask build-web-vm`.
    let component = unsafe { WtComponent::deserialize(&engine, EOSH_PULLEY)? };
    let mut linker: Linker<WebState> = Linker::new(&engine);
    crate::providers::add_providers(&mut linker)?;
    crate::fs::add_fs_io(&mut linker)?;
    add_exec(&mut linker)?;
    let mut store = Store::new(&engine, WebState::new());
    let _instance = block_on(
        "eosh instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;
    crate::out_line(
        "eosh: instantiated against the in-browser exec/text/fs surface (the shell links).",
    );
    Ok(())
}

/// Boot eosh one-shot: run a single command line and return. Drives eosh's `main(some(cmd))`,
/// which resolves the program from `/bin`, compiles (artifact lookup), spawns it
/// run-to-completion, and prints the outcome through the page terminal.
pub fn boot_eosh(command: &str) -> Result<()> {
    let engine = engine(false)?;
    let component = unsafe { WtComponent::deserialize(&engine, EOSH_PULLEY)? };
    let mut linker: Linker<WebState> = Linker::new(&engine);
    crate::providers::add_providers(&mut linker)?;
    crate::fs::add_fs_io(&mut linker)?;
    add_exec(&mut linker)?;
    let mut store = Store::new(&engine, WebState::new());
    let instance = block_on(
        "eosh instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;
    let index = instance
        .get_export_index(&mut store, None, "main")
        .ok_or_else(|| wasmtime::Error::msg("eosh does not export `main`"))?;
    let main = instance
        .get_func(&mut store, index)
        .ok_or_else(|| wasmtime::Error::msg("eosh `main` is not a function"))?;
    let command = command.to_string();
    let outcome = block_on(
        "eosh main",
        store.run_concurrent(async move |accessor| -> Result<Val> {
            let arg = Val::Option(Some(std::boxed::Box::new(Val::String(command))));
            let mut result = [Val::Bool(false)];
            main.call_concurrent(accessor, &[arg], &mut result).await?;
            Ok(result[0].clone())
        }),
    )???;
    crate::outf!("eosh: session outcome = {}", render_val(&outcome));
    Ok(())
}
