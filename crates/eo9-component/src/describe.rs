//! Component inspection: validation, kind classification, and slot-level metadata.
//!
//! This is the implementation behind `load` and `describe`: the component's imports and
//! exports are read back as slots (name, interface, version, required/optional) and the
//! argument signature of `main` (binary) or `configure` (provider) is extracted from the
//! component type, per SPEC.md "Execution APIs" and "Arguments vs. imports".

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use wit_parser::decoding::{DecodedWasm, decode};
use wit_parser::{Function, Handle, InterfaceId, Resolve, Type, TypeDefKind, WorldId, WorldItem};

use crate::error::LoadError;
use crate::slots;
use crate::{ArgSpec, ComponentInfo, ComponentKind, ExportSlot, ImportNeed};

/// The suffix that marks the mechanically-derived optional flavor of an API interface
/// (SPEC.md "The capability algebra").
pub(crate) const OPTIONAL_SUFFIX: &str = "-optional";

/// The suffix that marks a provider's compose-time configuration interface
/// (SPEC.md "Binary or provider, never both").
pub(crate) const CONFIG_SUFFIX: &str = "-config";

/// The entry point of a `*-config` interface.
pub(crate) const CONFIGURE: &str = "configure";

/// Slot-level metadata for one import of a component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportMeta {
    /// The extern name as it appears in the component binary (world key), e.g.
    /// `eo9:fs/fs@0.1.0` or `system-fs`.
    pub extern_name: String,
    /// The versionless slot name, e.g. `eo9:fs/fs` or `system-fs`.
    pub slot: String,
    /// The imported interface, e.g. `eo9:fs/fs` (empty for inline interfaces).
    pub interface: String,
    /// The interface version text, e.g. `0.1.0` (empty if unversioned).
    pub version: String,
    /// Mandatory vs. optional import (optional = the `-optional` interface flavor).
    pub required: bool,
    /// Whether the interface carries no authority at all (it has no functions, only
    /// types) -- e.g. the `eo9:*/types` interfaces pulled in by `use`.
    pub authority_free: bool,
}

/// Slot-level metadata for one interface export of a component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExportMeta {
    /// The extern name as it appears in the component binary.
    pub extern_name: String,
    /// The versionless slot name.
    pub slot: String,
    /// The exported interface name (empty for inline interfaces).
    pub interface: String,
    /// The interface version text (empty if unversioned).
    pub version: String,
}

/// Everything `load` learns about a component, kept alongside its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Meta {
    pub kind: ComponentKind,
    pub imports: Vec<ImportMeta>,
    pub exports: Vec<ExportMeta>,
    pub args: Vec<ArgSpec>,
}

impl Meta {
    /// Validates `bytes` as a component, classifies its kind, and extracts slot-level
    /// metadata and the argument signature.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LoadError> {
        // 1. It must be a valid Component Model *component* (not a core module).
        if !wasmparser::Parser::is_component(bytes) {
            return Err(LoadError::InvalidComponent(
                "not a binary-encoded component (core modules are not Eo9 modules)".to_string(),
            ));
        }
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(bytes)
            .map_err(|err| LoadError::InvalidComponent(err.to_string()))?;

        // 2. Recover the component's world (imports, exports, and their types).
        let decoded = decode(bytes).map_err(|err| LoadError::InvalidComponent(err.to_string()))?;
        let (resolve, world) = match decoded {
            DecodedWasm::Component(resolve, world) => (resolve, world),
            DecodedWasm::WitPackage(..) => {
                return Err(LoadError::NotAnEo9Module(
                    "a wasm-encoded WIT package, not a concrete component".to_string(),
                ));
            }
        };

        Self::from_world(&resolve, world)
    }

    /// Classifies and describes an already-decoded world.
    pub fn from_world(resolve: &Resolve, world: WorldId) -> Result<Self, LoadError> {
        let world = &resolve.worlds[world];

        let mut imports = Vec::new();
        for (key, item) in &world.imports {
            if let WorldItem::Interface { id, .. } = item {
                let extern_name = resolve.name_world_key(key);
                let (interface, version) = interface_ident(resolve, *id);
                let iface = &resolve.interfaces[*id];
                imports.push(ImportMeta {
                    slot: slots::slot_name(&extern_name).to_string(),
                    extern_name,
                    required: !interface.ends_with(OPTIONAL_SUFFIX),
                    authority_free: iface.functions.is_empty(),
                    interface,
                    version,
                });
            }
        }

        let mut exports = Vec::new();
        let mut main: Option<&Function> = None;
        let mut configure: Option<&Function> = None;
        let mut config_interface_entries: Vec<&Function> = Vec::new();
        let mut other_funcs = Vec::new();
        for (key, item) in &world.exports {
            match item {
                WorldItem::Interface { id, .. } => {
                    let extern_name = resolve.name_world_key(key);
                    let (interface, version) = interface_ident(resolve, *id);
                    // A provider's argument surface lives in its exported `*-config`
                    // interface (SPEC.md "Binary or provider, never both").
                    if interface.ends_with(CONFIG_SUFFIX)
                        && let Some(entry) = resolve.interfaces[*id].functions.get(CONFIGURE)
                    {
                        config_interface_entries.push(entry);
                    }
                    exports.push(ExportMeta {
                        slot: slots::slot_name(&extern_name).to_string(),
                        extern_name,
                        interface,
                        version,
                    });
                }
                WorldItem::Function(f) => match f.name.as_str() {
                    "main" => main = Some(f),
                    "configure" => configure = Some(f),
                    other => other_funcs.push(other.to_string()),
                },
                WorldItem::Type { .. } => {}
            }
        }

        // Kind classification (SPEC.md "WASM runtime"): a binary exports `main` and is
        // run; a provider exports interfaces plus (optionally) `configure` and is
        // composed. A module is never both. The empty component is the identity
        // provider. Anything else is not an Eo9 module.
        if !other_funcs.is_empty() {
            return Err(LoadError::NotAnEo9Module(format!(
                "unexpected function exports (only `main` or `configure` are allowed): {}",
                other_funcs.join(", ")
            )));
        }
        let (kind, entry) = match (main, configure) {
            (Some(_), Some(_)) => {
                return Err(LoadError::NotAnEo9Module(
                    "exports both `main` and `configure`; a module is a binary or a provider, \
                     never both"
                        .to_string(),
                ));
            }
            (Some(main), None) => {
                if !exports.is_empty() {
                    return Err(LoadError::NotAnEo9Module(
                        "exports both `main` and interfaces; a module is a binary or a provider, \
                         never both"
                            .to_string(),
                    ));
                }
                (ComponentKind::Binary, Some(main))
            }
            // A provider's `configure` entry may be a bare world-level export or live in
            // a single exported `*-config` interface; either way its parameters are the
            // provider's argument signature. (With several config interfaces the
            // signature is ambiguous, so no arguments are reported.)
            (None, configure) => {
                let config_entry = match config_interface_entries.as_slice() {
                    [entry] => Some(*entry),
                    _ => None,
                };
                (ComponentKind::Provider, configure.or(config_entry))
            }
        };

        let args = entry
            .map(|f| {
                f.params
                    .iter()
                    .map(|p| ArgSpec {
                        name: p.name.clone(),
                        ty: type_text(resolve, &p.ty),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            kind,
            imports,
            exports,
            args,
        })
    }

    /// The public `component-info` view of this metadata.
    pub fn info(&self) -> ComponentInfo {
        ComponentInfo {
            kind: self.kind,
            imports: self
                .imports
                .iter()
                .map(|i| ImportNeed {
                    slot: i.slot.clone(),
                    interface: i.interface.clone(),
                    version: i.version.clone(),
                    required: i.required,
                    authority_free: i.authority_free,
                })
                .collect(),
            exports: self
                .exports
                .iter()
                .map(|e| ExportSlot {
                    name: e.slot.clone(),
                    interface: e.interface.clone(),
                    version: e.version.clone(),
                })
                .collect(),
            args: self.args.clone(),
        }
    }
}

/// The `ns:pkg/name` identifier and version text of an interface (both empty for
/// inline/anonymous interfaces).
fn interface_ident(resolve: &Resolve, id: InterfaceId) -> (String, String) {
    let iface = &resolve.interfaces[id];
    match (&iface.name, iface.package) {
        (Some(name), Some(pkg)) => {
            let pkg = &resolve.packages[pkg].name;
            let ident = format!("{}:{}/{}", pkg.namespace, pkg.name, name);
            let version = pkg
                .version
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            (ident, version)
        }
        _ => (String::new(), String::new()),
    }
}

/// Renders a WIT type as the text used in `arg-spec.ty` (e.g. `string`, `list<u8>`,
/// `option<instant>`). Named types are rendered by name; anonymous constructors are
/// rendered structurally.
pub(crate) fn type_text(resolve: &Resolve, ty: &Type) -> String {
    match ty {
        Type::Bool => "bool".to_string(),
        Type::U8 => "u8".to_string(),
        Type::U16 => "u16".to_string(),
        Type::U32 => "u32".to_string(),
        Type::U64 => "u64".to_string(),
        Type::S8 => "s8".to_string(),
        Type::S16 => "s16".to_string(),
        Type::S32 => "s32".to_string(),
        Type::S64 => "s64".to_string(),
        Type::F32 => "f32".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Char => "char".to_string(),
        Type::String => "string".to_string(),
        Type::ErrorContext => "error-context".to_string(),
        Type::Id(id) => {
            let def = &resolve.types[*id];
            if let Some(name) = &def.name {
                return name.clone();
            }
            match &def.kind {
                TypeDefKind::List(t) => format!("list<{}>", type_text(resolve, t)),
                TypeDefKind::FixedLengthList(t, len) => {
                    format!("list<{}, {len}>", type_text(resolve, t))
                }
                TypeDefKind::Option(t) => format!("option<{}>", type_text(resolve, t)),
                TypeDefKind::Map(k, v) => {
                    format!("map<{}, {}>", type_text(resolve, k), type_text(resolve, v))
                }
                TypeDefKind::Result(r) => match (&r.ok, &r.err) {
                    (None, None) => "result".to_string(),
                    (Some(ok), None) => format!("result<{}>", type_text(resolve, ok)),
                    (None, Some(err)) => format!("result<_, {}>", type_text(resolve, err)),
                    (Some(ok), Some(err)) => format!(
                        "result<{}, {}>",
                        type_text(resolve, ok),
                        type_text(resolve, err)
                    ),
                },
                TypeDefKind::Tuple(t) => {
                    let parts: Vec<String> =
                        t.types.iter().map(|t| type_text(resolve, t)).collect();
                    format!("tuple<{}>", parts.join(", "))
                }
                TypeDefKind::Handle(Handle::Own(resource)) => type_name(resolve, *resource),
                TypeDefKind::Handle(Handle::Borrow(resource)) => {
                    format!("borrow<{}>", type_name(resolve, *resource))
                }
                TypeDefKind::Future(None) => "future".to_string(),
                TypeDefKind::Future(Some(t)) => format!("future<{}>", type_text(resolve, t)),
                TypeDefKind::Stream(None) => "stream".to_string(),
                TypeDefKind::Stream(Some(t)) => format!("stream<{}>", type_text(resolve, t)),
                TypeDefKind::Type(t) => type_text(resolve, t),
                // Records, variants, enums, flags, and resources are always named in
                // valid WIT; reaching here means the name was stripped somewhere.
                other => format!("<anonymous {}>", other.as_str()),
            }
        }
    }
}

/// The name of a (necessarily named) type such as a resource.
fn type_name(resolve: &Resolve, id: wit_parser::TypeId) -> String {
    resolve.types[id]
        .name
        .clone()
        .unwrap_or_else(|| "<anonymous>".to_string())
}
