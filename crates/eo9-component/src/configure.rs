//! `configure` -- binding a provider's compose-time configuration constants
//! (SPEC.md "Binary or provider, never both": `configure : provider × args → provider`).
//!
//! A configurable provider ships a small exported `*-config` interface whose `configure`
//! entry binds the configuration and returns the provider's root capability handle.
//! `configure(provider, args)` bakes the given constants in:
//!
//! * the WAVE-encoded `args` are type-checked against `configure`'s declared parameters
//!   and lowered to canonical-ABI constants;
//! * a small *binder* component is synthesized that imports the provider's config
//!   interface **and** its API interfaces, and re-exports the API interfaces with
//!   forwarding shims: the first forwarded call first invokes the provider's `configure`
//!   with the baked-in constants (exactly once -- a flag guards it), trapping if
//!   `configure` reports an invalid value or would block, and every call then forwards
//!   to the provider unchanged;
//! * provider and binder are wired together: the wrapper exports the binder's gated API
//!   interfaces plus the provider's remaining (types-only) exports, while the config
//!   interface is sealed away -- the consumer can neither observe nor re-run the
//!   configuration.
//!
//! Binding on first use, rather than at instantiation, is what makes the configured
//! provider runnable under the Component Model's concurrency rules: `configure` is an
//! `async func`, and a synchronous task (a composed consumer's `main`) may not make a
//! blocking call to an async-lifted export, nor may anything call out of a component
//! while it is still being instantiated. The binder therefore *async-lowers* `configure`
//! and makes the call lazily, from within the consumer's own task, accepting only an
//! immediately-completed result (compose-time configuration must not block).
//!
//! The result is an ordinary provider: composable, sealable, and byte-deterministic for
//! the same operands. The configured behavior end-to-end is exercised by the runtime and
//! integration suites.

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, FunctionSection,
    GlobalSection, GlobalType, ImportSection, Instruction, MemArg, MemorySection, MemoryType,
    Module, TypeSection, ValType,
};
use wasm_wave::value::{self, Value};
use wasm_wave::wasm::WasmValue;
use wit_parser::abi::{AbiVariant, FlatTypes, WasmSignature, WasmType};
use wit_parser::decoding::{DecodedWasm, decode};
use wit_parser::{
    Function, FunctionKind, Handle, InterfaceId, Resolve, SizeAlign, Type, TypeDefKind, TypeId,
    TypeOwner, WorldItem,
};

use crate::compose::{
    encode as encode_graph, export_all, register, slot_annotations, wire_matching_imports,
};
use crate::describe::{CONFIG_SUFFIX, CONFIGURE};
use crate::error::ConfigureError;
use crate::{Component, ComponentKind, synth};

/// The subtask status code meaning "the callee already returned" (Component Model async
/// ABI: the low four bits of an async-lowered call's packed return value).
const SUBTASK_RETURNED: i32 = 2;

/// One canonical-ABI constant baked into the binder's call to `configure`.
enum FlatConst {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// A string constant: stored in the binder's data segment, passed as (ptr, len).
    Str(String),
}

/// One provider API function the binder forwards (gated on configuration).
struct ForwardFunction {
    /// The binder's core export name, e.g. `eo9:entropy/entropy@0.1.0#get-bytes`.
    export_name: String,
    /// The core import module (the interface extern name) and field (function name).
    import_module: String,
    import_field: String,
    /// The sync-lowered (caller-side) core signature.
    import_sig: WasmSignature,
    /// The sync-lifted (callee-side) core signature.
    export_sig: WasmSignature,
    /// Borrow-handle parameters this function receives, as (flat parameter index,
    /// index into [`BinderPlan::drop_intrinsics`]): the canonical ABI requires the
    /// callee to drop every borrow it was lent before returning.
    borrow_drops: Vec<(u32, usize)>,
}

/// A `[resource-drop]` intrinsic the binder needs for releasing lent borrow handles.
struct DropIntrinsic {
    /// The core import module: the interface that owns the resource.
    module: String,
    /// The core import field: `[resource-drop]<resource-name>`.
    field: String,
}

/// Everything the binder core module is generated from.
struct BinderPlan {
    /// The config interface's extern name (import module of the async-lowered call).
    config_extern: String,
    /// `configure`'s async-lowered core signature.
    config_sig: WasmSignature,
    /// The baked-in arguments, in parameter order.
    constants: Vec<FlatConst>,
    /// The forwarded API functions, in interface/declaration order.
    forwards: Vec<ForwardFunction>,
    /// The resource-drop intrinsics referenced by [`ForwardFunction::borrow_drops`].
    drop_intrinsics: Vec<DropIntrinsic>,
    /// Bytes reserved for indirect results (configure's and forwarded calls').
    scratch_size: u32,
}

/// Binds `provider`'s compose-time configuration to the given WAVE-encoded constants,
/// yielding a configured provider that exports the API interfaces (and types) but not
/// the config interface.
pub fn configure<N, V>(provider: &Component, args: &[(N, V)]) -> Result<Component, ConfigureError>
where
    N: AsRef<str>,
    V: AsRef<str>,
{
    let internal = |msg: String| ConfigureError::Internal(msg);

    if provider.kind() != ComponentKind::Provider {
        return Err(ConfigureError::NotAProvider);
    }

    // The provider's single `*-config` export is what gets bound (and sealed away).
    let config_exports: Vec<_> = provider
        .meta()
        .exports
        .iter()
        .filter(|e| e.interface.ends_with(CONFIG_SUFFIX))
        .collect();
    let config_export = match config_exports.as_slice() {
        [] => return Err(ConfigureError::NoConfigInterface),
        [one] => (*one).clone(),
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.slot.as_str()).collect();
            return Err(internal(format!(
                "the provider exports more than one config interface ({}); configuring them \
                 one at a time is not supported",
                names.join(", ")
            )));
        }
    };

    // Recover the config interface's `configure` signature from the provider itself.
    let decoded = decode(provider.bytes())
        .map_err(|err| internal(format!("failed to re-decode the provider: {err:#}")))?;
    let (mut resolve, world) = match decoded {
        DecodedWasm::Component(resolve, world) => (resolve, world),
        DecodedWasm::WitPackage(..) => {
            return Err(internal("provider decoded as a WIT package".to_string()));
        }
    };
    let exported_interfaces: Vec<(String, InterfaceId)> = resolve.worlds[world]
        .exports
        .iter()
        .filter_map(|(key, item)| match item {
            WorldItem::Interface { id, .. } => Some((resolve.name_world_key(key), *id)),
            _ => None,
        })
        .collect();
    let config_interface = exported_interfaces
        .iter()
        .find(|(name, _)| *name == config_export.extern_name)
        .map(|(_, id)| *id)
        .ok_or_else(|| {
            internal(format!(
                "config export `{}` not found after decoding",
                config_export.extern_name
            ))
        })?;
    let Some(function) = resolve.interfaces[config_interface]
        .functions
        .get(CONFIGURE)
    else {
        return Err(internal(format!(
            "config interface `{}` has no `configure` function",
            config_export.interface
        )));
    };
    let function = function.clone();

    // Type-check the WAVE arguments against the declared parameters and lower them.
    let constants = bind_arguments(&resolve, &function, args)?;
    let config_sig = resolve.wasm_signature(AbiVariant::GuestImportAsync, &function);
    if config_sig.indirect_params {
        return Err(internal(
            "`configure` takes too many (or too large) parameters for compose-time baking"
                .to_string(),
        ));
    }

    // The provider's API interfaces (everything exported with functions, other than the
    // config interface) are re-exported through gating forwarders.
    let mut forward_interfaces = Vec::new();
    for (extern_name, id) in &exported_interfaces {
        if *extern_name == config_export.extern_name || resolve.interfaces[*id].functions.is_empty()
        {
            continue;
        }
        check_forwardable(&resolve, extern_name, *id).map_err(internal)?;
        forward_interfaces.push((extern_name.clone(), *id));
    }
    if forward_interfaces.is_empty() {
        return Err(internal(
            "the provider exports no API functions to gate the configuration on".to_string(),
        ));
    }

    let plan = plan_binder(
        &resolve,
        &config_export.extern_name,
        config_sig,
        &function,
        constants,
        &forward_interfaces,
    )
    .map_err(internal)?;

    // Synthesize the binder and wire it in front of the provider.
    let binder = build_binder(&mut resolve, &plan, &forward_interfaces).map_err(internal)?;

    let compose_err =
        |err: crate::ComposeError| internal(format!("failed to assemble the wrapper: {err}"));
    let mut graph = wac_graph::CompositionGraph::new();
    let provider_pkg = register(&mut graph, "provider", provider.bytes()).map_err(compose_err)?;
    let binder_pkg = register(&mut graph, "binder", &binder).map_err(compose_err)?;
    let provider_inst = graph.instantiate(provider_pkg);
    let binder_inst = graph.instantiate(binder_pkg);

    // The binder's config, types, and API imports are all satisfied by the provider.
    wire_matching_imports(
        &mut graph,
        provider_pkg,
        provider_inst,
        binder_pkg,
        binder_inst,
    )
    .map_err(compose_err)?;
    // The wrapper exports the binder's gated API interfaces plus whatever else the
    // provider exports (its types interfaces), but not the config interface and not the
    // provider's own, ungated API exports.
    export_all(&mut graph, binder_pkg, binder_inst, None).map_err(compose_err)?;
    let mut skip_slots: Vec<String> = vec![config_export.slot.clone()];
    for (extern_name, _) in &forward_interfaces {
        skip_slots.push(crate::slots::slot_name(extern_name).to_string());
    }
    export_all(&mut graph, provider_pkg, provider_inst, Some(&skip_slots)).map_err(compose_err)?;

    encode_graph(&graph, &slot_annotations(&[provider])).map_err(compose_err)
}

/// Checks that an exported API interface can be forwarded by the binder: only
/// freestanding, synchronous functions, no resources of its own (resources shared
/// through a types-only interface are fine -- they pass through as plain handles), and
/// borrow handles only as direct parameters (so the forwarder can locate and release
/// them).
fn check_forwardable(resolve: &Resolve, extern_name: &str, id: InterfaceId) -> Result<(), String> {
    let interface = &resolve.interfaces[id];
    let owns_resource = interface.types.values().any(|ty| {
        matches!(resolve.types[*ty].kind, TypeDefKind::Resource)
            && resolve.types[*ty].owner == TypeOwner::Interface(id)
    });
    if owns_resource {
        return Err(format!(
            "API interface `{extern_name}` defines its own resources; compose-time \
             configuration of such providers is not supported yet"
        ));
    }
    for (name, function) in &interface.functions {
        if !matches!(function.kind, FunctionKind::Freestanding) {
            return Err(format!(
                "API function `{extern_name}#{name}` is not a freestanding synchronous \
                 function; compose-time configuration of such providers is not supported yet"
            ));
        }
        for param in &function.params {
            let top_level_borrow = as_borrow(resolve, &param.ty).is_some();
            if !top_level_borrow && contains_borrow(resolve, &param.ty) {
                return Err(format!(
                    "API function `{extern_name}#{name}` takes a borrow nested inside \
                     parameter `{}`; compose-time configuration of such providers is not \
                     supported yet",
                    param.name
                ));
            }
        }
    }
    Ok(())
}

/// If `ty` is (an alias of) `borrow<R>`, the resource `R` it borrows.
fn as_borrow(resolve: &Resolve, ty: &Type) -> Option<TypeId> {
    let mut ty = *ty;
    loop {
        let Type::Id(id) = ty else { return None };
        match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => ty = *inner,
            TypeDefKind::Handle(Handle::Borrow(resource)) => {
                return Some(resolve_resource(resolve, *resource));
            }
            _ => return None,
        }
    }
}

/// Follows aliases down to the defining `resource` type.
fn resolve_resource(resolve: &Resolve, id: TypeId) -> TypeId {
    let mut id = id;
    loop {
        match &resolve.types[id].kind {
            TypeDefKind::Type(Type::Id(inner)) => id = *inner,
            _ => return id,
        }
    }
}

/// Whether a type contains a borrow handle anywhere in its structure.
fn contains_borrow(resolve: &Resolve, ty: &Type) -> bool {
    let Type::Id(id) = ty else { return false };
    match &resolve.types[*id].kind {
        TypeDefKind::Handle(Handle::Borrow(_)) => true,
        TypeDefKind::Handle(Handle::Own(_)) | TypeDefKind::Resource | TypeDefKind::Flags(_) => {
            false
        }
        TypeDefKind::Type(inner)
        | TypeDefKind::List(inner)
        | TypeDefKind::FixedLengthList(inner, _)
        | TypeDefKind::Option(inner)
        | TypeDefKind::Future(Some(inner))
        | TypeDefKind::Stream(Some(inner)) => contains_borrow(resolve, inner),
        TypeDefKind::Future(None) | TypeDefKind::Stream(None) => false,
        TypeDefKind::Map(k, v) => contains_borrow(resolve, k) || contains_borrow(resolve, v),
        TypeDefKind::Tuple(t) => t.types.iter().any(|t| contains_borrow(resolve, t)),
        TypeDefKind::Record(r) => r.fields.iter().any(|f| contains_borrow(resolve, &f.ty)),
        TypeDefKind::Variant(v) => v
            .cases
            .iter()
            .any(|c| c.ty.as_ref().is_some_and(|t| contains_borrow(resolve, t))),
        TypeDefKind::Result(r) => {
            r.ok.as_ref().is_some_and(|t| contains_borrow(resolve, t))
                || r.err.as_ref().is_some_and(|t| contains_borrow(resolve, t))
        }
        TypeDefKind::Enum(_) | TypeDefKind::Unknown => false,
    }
}

/// Lays out everything the binder core module needs.
fn plan_binder(
    resolve: &Resolve,
    config_extern: &str,
    config_sig: WasmSignature,
    config_function: &Function,
    constants: Vec<FlatConst>,
    forward_interfaces: &[(String, InterfaceId)],
) -> Result<BinderPlan, String> {
    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let result_size = |function: &Function| -> u32 {
        function
            .result
            .as_ref()
            .map(|ty| sizes.size(ty).size_wasm32() as u32)
            .unwrap_or(0)
    };

    fn drop_index_of(
        resolve: &Resolve,
        drop_intrinsics: &mut Vec<DropIntrinsic>,
        resource: TypeId,
    ) -> usize {
        let name = resolve.types[resource]
            .name
            .clone()
            .unwrap_or_else(|| "resource".to_string());
        let module = match resolve.types[resource].owner {
            TypeOwner::Interface(owner) => resolve.id_of(owner).unwrap_or_default(),
            _ => String::new(),
        };
        let field = format!("[resource-drop]{name}");
        if let Some(index) = drop_intrinsics
            .iter()
            .position(|d| d.module == module && d.field == field)
        {
            return index;
        }
        drop_intrinsics.push(DropIntrinsic { module, field });
        drop_intrinsics.len() - 1
    }

    let mut scratch_size = result_size(config_function);
    let mut forwards = Vec::new();
    let mut drop_intrinsics: Vec<DropIntrinsic> = Vec::new();
    for (extern_name, id) in forward_interfaces {
        for (name, function) in &resolve.interfaces[*id].functions {
            let import_sig = resolve.wasm_signature(AbiVariant::GuestImport, function);
            let export_sig = resolve.wasm_signature(AbiVariant::GuestExport, function);
            if import_sig.retptr {
                scratch_size = scratch_size.max(result_size(function));
            }

            // Locate every borrow-handle parameter: the forwarder must release them
            // before it returns. Flat positions only exist when parameters are passed
            // flat, so the (unrealistic) indirect-parameters-with-borrows combination is
            // rejected rather than mishandled.
            let mut borrow_drops = Vec::new();
            let mut flat_index = 0u32;
            for param in &function.params {
                let mut storage = [WasmType::I32; 32];
                let mut flat = FlatTypes::new(&mut storage);
                resolve.push_flat(&param.ty, &mut flat);
                let width = flat.to_vec().len() as u32;
                if let Some(resource) = as_borrow(resolve, &param.ty) {
                    if export_sig.indirect_params {
                        return Err(format!(
                            "API function `{extern_name}#{name}` passes its parameters \
                             indirectly and borrows a resource; compose-time configuration \
                             of such providers is not supported yet"
                        ));
                    }
                    let drop = drop_index_of(resolve, &mut drop_intrinsics, resource);
                    borrow_drops.push((flat_index, drop));
                }
                flat_index += width;
            }

            forwards.push(ForwardFunction {
                export_name: format!("{extern_name}#{name}"),
                import_module: extern_name.clone(),
                import_field: name.clone(),
                import_sig,
                export_sig,
                borrow_drops,
            });
        }
    }

    Ok(BinderPlan {
        config_extern: config_extern.to_string(),
        config_sig,
        constants,
        forwards,
        drop_intrinsics,
        scratch_size: scratch_size.next_multiple_of(8).max(16),
    })
}

/// Checks the supplied arguments against `configure`'s parameters and lowers each one to
/// its canonical-ABI constant(s), in parameter order.
fn bind_arguments<N, V>(
    resolve: &Resolve,
    function: &Function,
    args: &[(N, V)],
) -> Result<Vec<FlatConst>, ConfigureError>
where
    N: AsRef<str>,
    V: AsRef<str>,
{
    for (name, _) in args {
        let name = name.as_ref();
        if !function.params.iter().any(|p| p.name == name) {
            return Err(ConfigureError::UnknownArgument(name.to_string()));
        }
    }

    let mut constants = Vec::new();
    for param in &function.params {
        let supplied: Vec<&str> = args
            .iter()
            .filter(|(name, _)| name.as_ref() == param.name)
            .map(|(_, value)| value.as_ref())
            .collect();
        let text = match supplied.as_slice() {
            [] => return Err(ConfigureError::MissingArgument(param.name.clone())),
            [text] => *text,
            _ => {
                return Err(ConfigureError::InvalidArgument {
                    name: param.name.clone(),
                    message: "supplied more than once".to_string(),
                });
            }
        };
        constants.push(lower_argument(resolve, &param.name, &param.ty, text)?);
    }
    Ok(constants)
}

/// Parses one WAVE value against its declared WIT type and lowers it to a constant.
fn lower_argument(
    resolve: &Resolve,
    name: &str,
    ty: &Type,
    text: &str,
) -> Result<FlatConst, ConfigureError> {
    let invalid = |message: String| ConfigureError::InvalidArgument {
        name: name.to_string(),
        message,
    };

    // Follow type aliases down to the underlying type.
    let mut ty = *ty;
    while let Type::Id(id) = ty {
        match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => ty = *inner,
            _ => break,
        }
    }

    let wave_type = wave_type(resolve, &ty).ok_or_else(|| {
        ConfigureError::Internal(format!(
            "parameter `{name}` has a type that compose-time configuration cannot bake in \
             yet (only scalars, strings, and enums are supported)"
        ))
    })?;
    let value: Value = wasm_wave::from_str(&wave_type, text).map_err(|err| {
        invalid(format!(
            "does not parse as `{}`: {err}",
            crate::describe::type_text(resolve, &ty)
        ))
    })?;

    Ok(match ty {
        Type::Bool => FlatConst::I32(i32::from(value.unwrap_bool())),
        Type::U8 => FlatConst::I32(i32::from(value.unwrap_u8())),
        Type::U16 => FlatConst::I32(i32::from(value.unwrap_u16())),
        Type::U32 => FlatConst::I32(value.unwrap_u32() as i32),
        Type::S8 => FlatConst::I32(i32::from(value.unwrap_s8())),
        Type::S16 => FlatConst::I32(i32::from(value.unwrap_s16())),
        Type::S32 => FlatConst::I32(value.unwrap_s32()),
        Type::U64 => FlatConst::I64(value.unwrap_u64() as i64),
        Type::S64 => FlatConst::I64(value.unwrap_s64()),
        Type::F32 => FlatConst::F32(value.unwrap_f32()),
        Type::F64 => FlatConst::F64(value.unwrap_f64()),
        Type::Char => FlatConst::I32(value.unwrap_char() as i32),
        Type::String => FlatConst::Str(value.unwrap_string().into_owned()),
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Enum(e) => {
                let case = value.unwrap_enum();
                let index = e
                    .cases
                    .iter()
                    .position(|c| c.name == case)
                    .ok_or_else(|| invalid(format!("`{case}` is not a case of the enum")))?;
                FlatConst::I32(index as i32)
            }
            _ => unreachable!("unsupported types are rejected before parsing"),
        },
        _ => unreachable!("unsupported types are rejected before parsing"),
    })
}

/// The WAVE type used to parse a supported configuration parameter (None if the type is
/// not supported for compose-time baking).
fn wave_type(resolve: &Resolve, ty: &Type) -> Option<value::Type> {
    Some(match ty {
        Type::Bool => value::Type::BOOL,
        Type::U8 => value::Type::U8,
        Type::U16 => value::Type::U16,
        Type::U32 => value::Type::U32,
        Type::U64 => value::Type::U64,
        Type::S8 => value::Type::S8,
        Type::S16 => value::Type::S16,
        Type::S32 => value::Type::S32,
        Type::S64 => value::Type::S64,
        Type::F32 => value::Type::F32,
        Type::F64 => value::Type::F64,
        Type::Char => value::Type::CHAR,
        Type::String => value::Type::STRING,
        Type::Id(id) => match &resolve.types[*id].kind {
            TypeDefKind::Enum(_) => value::resolve_wit_type(resolve, *id).ok()?,
            _ => return None,
        },
        _ => return None,
    })
}

/// Synthesizes the binder component: it imports the provider's config and API
/// interfaces and re-exports the API interfaces with configuration-gating forwarders.
fn build_binder(
    resolve: &mut Resolve,
    plan: &BinderPlan,
    forward_interfaces: &[(String, InterfaceId)],
) -> Result<Vec<u8>, String> {
    // A world importing the config and API interfaces and re-exporting the APIs;
    // wit-parser elaborates the transitive types imports for us.
    let mut wit = String::from("package eo9-internal:configure@0.1.0;\n\nworld binder {\n");
    wit.push_str(&format!("    import {};\n", plan.config_extern));
    for (extern_name, _) in forward_interfaces {
        wit.push_str(&format!("    import {extern_name};\n"));
        wit.push_str(&format!("    export {extern_name};\n"));
    }
    wit.push_str("}\n");
    let package = resolve
        .push_source("configure-binder.wit", &wit)
        .map_err(|err| format!("failed to resolve the binder world: {err:#}"))?;
    let world = resolve
        .select_world(&[package], Some("binder"))
        .map_err(|err| format!("failed to select the binder world: {err:#}"))?;

    let module = synthesize_binder_module(plan);
    synth::encode_component(module, resolve, world)
}

/// The binder's memory layout: a fixed scratch area for indirect results starting at 16,
/// then the baked-in string constants, then the bump heap.
struct Layout {
    scratch: u32,
    string_offsets: Vec<u32>,
    string_data: Vec<u8>,
    heap_base: u32,
}

fn layout(plan: &BinderPlan) -> Layout {
    let scratch = 16u32;
    let string_base = scratch + plan.scratch_size;
    let mut string_data = Vec::new();
    let mut string_offsets = Vec::new();
    for constant in &plan.constants {
        if let FlatConst::Str(text) = constant {
            string_offsets.push(string_base + string_data.len() as u32);
            string_data.extend_from_slice(text.as_bytes());
        }
    }
    let heap_base = (string_base + string_data.len() as u32).next_multiple_of(16);
    Layout {
        scratch,
        string_offsets,
        string_data,
        heap_base,
    }
}

/// Builds the binder's core module: the async-lowered `configure` import, a sync-lowered
/// import and a gating forwarder export for every provider API function, a one-shot
/// configuration flag, and a bump allocator for canonical-ABI lifting.
fn synthesize_binder_module(plan: &BinderPlan) -> Vec<u8> {
    let layout = layout(plan);

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    // Imported functions first (they occupy the low function indices): the async-lowered
    // `configure`, the sync-lowered API functions, then the resource-drop intrinsics.
    let configure_import = synth::push_signature(&mut types, &plan.config_sig);
    imports.import(
        &plan.config_extern,
        &format!("[async-lower]{CONFIGURE}"),
        wasm_encoder::EntityType::Function(configure_import),
    );
    let configure_func = 0u32;
    let mut forward_imports = Vec::new();
    for forward in &plan.forwards {
        let ty = synth::push_signature(&mut types, &forward.import_sig);
        imports.import(
            &forward.import_module,
            &forward.import_field,
            wasm_encoder::EntityType::Function(ty),
        );
        forward_imports.push(1 + forward_imports.len() as u32);
    }
    let drop_type = types.len();
    types.ty().function([ValType::I32], []);
    let first_drop = 1 + plan.forwards.len() as u32;
    for intrinsic in &plan.drop_intrinsics {
        imports.import(
            &intrinsic.module,
            &intrinsic.field,
            wasm_encoder::EntityType::Function(drop_type),
        );
    }

    // Defined functions: cabi_realloc, the one-shot gate, then one forwarder per API
    // function.
    let imported = first_drop + plan.drop_intrinsics.len() as u32;
    let realloc_func = imported;
    let realloc_type = types.len();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    functions.function(realloc_type);
    code.function(&realloc_body());

    let gate_func = imported + 1;
    let gate_type = types.len();
    types.ty().function([], []);
    functions.function(gate_type);
    code.function(&gate_body(plan, &layout, configure_func));

    for (forward, import_index) in plan.forwards.iter().zip(&forward_imports) {
        let ty = synth::push_signature(&mut types, &forward.export_sig);
        functions.function(ty);
        let index = imported + 2 + *import_index - 1;
        exports.export(&forward.export_name, ExportKind::Func, index);
        code.function(&forward_body(
            forward,
            &layout,
            *import_index,
            gate_func,
            first_drop,
        ));
    }

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    // Globals: 0 = bump pointer, 1 = "configure has run" flag.
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(layout.heap_base as i32),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );

    exports.export("memory", ExportKind::Memory, 0);
    exports.export("cabi_realloc", ExportKind::Func, realloc_func);

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&globals);
    module.section(&exports);
    module.section(&code);
    if !layout.string_data.is_empty() {
        let string_base = layout.scratch + plan.scratch_size;
        let mut data = DataSection::new();
        data.active(
            0,
            &ConstExpr::i32_const(string_base as i32),
            layout.string_data.clone(),
        );
        module.section(&data);
    }
    module.finish()
}

/// The binder's `cabi_realloc`: a bump allocator over the exported memory (grown on
/// demand) used by the canonical ABI to lift results into the binder. Allocations are
/// never revisited; the bump pointer is reset at the start of every forwarded call, once
/// the previous call's results have been consumed.
fn realloc_body() -> wasm_encoder::Function {
    let mut f = wasm_encoder::Function::new([(1, ValType::I32)]);
    // Locals: 0 old_ptr, 1 old_size, 2 align, 3 new_size, 4 ptr (scratch).
    let ptr = 4;

    // ptr = (bump + align - 1) & -align
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalSet(ptr));
    // bump = ptr + new_size
    f.instruction(&Instruction::LocalGet(ptr));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(0));
    // while bump > memory.size * 64KiB: memory.grow(1), trapping if growth fails.
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::MemorySize(0));
    f.instruction(&Instruction::I32Const(65536));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32LeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::MemoryGrow(0));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(ptr));
    f.instruction(&Instruction::End);
    f
}

/// The one-shot gate: call `configure` (async-lowered, so a synchronous caller task is
/// allowed to make the call) with the baked-in constants, require that it completed
/// immediately and successfully, and mark the provider configured. Any failure -- an
/// error from `configure`, or a configuration that would block -- traps, so an invalid
/// value fails before the consumer observes any API behavior.
fn gate_body(plan: &BinderPlan, layout: &Layout, configure_func: u32) -> wasm_encoder::Function {
    let mut f = wasm_encoder::Function::new([]);
    let mut next_string = 0usize;
    for constant in &plan.constants {
        match constant {
            FlatConst::I32(v) => {
                f.instruction(&Instruction::I32Const(*v));
            }
            FlatConst::I64(v) => {
                f.instruction(&Instruction::I64Const(*v));
            }
            FlatConst::F32(v) => {
                f.instruction(&Instruction::F32Const((*v).into()));
            }
            FlatConst::F64(v) => {
                f.instruction(&Instruction::F64Const((*v).into()));
            }
            FlatConst::Str(text) => {
                f.instruction(&Instruction::I32Const(
                    layout.string_offsets[next_string] as i32,
                ));
                f.instruction(&Instruction::I32Const(text.len() as i32));
                next_string += 1;
            }
        }
    }
    if plan.config_sig.retptr {
        f.instruction(&Instruction::I32Const(layout.scratch as i32));
    }
    f.instruction(&Instruction::Call(configure_func));

    // The async-lowered call returns a packed subtask status; only "already returned"
    // (i.e. `configure` completed without blocking) is acceptable here.
    f.instruction(&Instruction::I32Const(0xF));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(SUBTASK_RETURNED));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);

    if plan.config_sig.retptr {
        // The first byte of the written result is the `result<_, _>` discriminant.
        f.instruction(&Instruction::I32Const(layout.scratch as i32));
        f.instruction(&Instruction::I32Load8U(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Unreachable);
        f.instruction(&Instruction::End);
    }

    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::GlobalSet(1));
    f.instruction(&Instruction::End);
    f
}

/// A gating forwarder: ensure `configure` has run, reset the bump allocator (the
/// previous call's results have been consumed by then), forward the call to the
/// provider -- passing flat values through unchanged and routing indirect results via
/// the shared scratch area -- and release any borrow handles the caller lent us, as the
/// canonical ABI requires before an export returns.
fn forward_body(
    forward: &ForwardFunction,
    layout: &Layout,
    import_index: u32,
    gate_func: u32,
    first_drop: u32,
) -> wasm_encoder::Function {
    let mut f = wasm_encoder::Function::new([]);

    f.instruction(&Instruction::GlobalGet(1));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Call(gate_func));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(layout.heap_base as i32));
    f.instruction(&Instruction::GlobalSet(0));

    for index in 0..forward.export_sig.params.len() as u32 {
        f.instruction(&Instruction::LocalGet(index));
    }
    if forward.import_sig.retptr {
        f.instruction(&Instruction::I32Const(layout.scratch as i32));
    }
    f.instruction(&Instruction::Call(import_index));

    // Release the borrows we were lent (any direct results stay untouched further down
    // the operand stack).
    for (flat_index, drop) in &forward.borrow_drops {
        f.instruction(&Instruction::LocalGet(*flat_index));
        f.instruction(&Instruction::Call(first_drop + *drop as u32));
    }

    if forward.export_sig.retptr {
        f.instruction(&Instruction::I32Const(layout.scratch as i32));
    }
    f.instruction(&Instruction::End);
    f
}
