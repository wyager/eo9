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
//! Binding on first use, rather than at instantiation, is what keeps the configured
//! provider runnable: nothing may call out of a component while it is still being
//! instantiated, so the binder makes the `configure` call lazily, from within the
//! consumer's own task, on the first forwarded API call. `configure` is a *synchronous*
//! export (it binds compile-time constants and must not block), so the binder sync-lowers
//! it -- a plain canonical call that may itself synchronously reenter another configured
//! provider's `configure`. (It was once async-lowered to dodge the "a sync task may not
//! block on an async-lifted export" rule; that made a configured provider whose
//! `configure` reentered another configured provider untypable -- the bug-1 trap. Making
//! `configure` sync removes the gamble entirely; see plan/03 D17 and SPEC.)
//!
//! Forwarding follows each API function's own ABI:
//!
//! * synchronous functions are forwarded with sync-lowered calls -- flat values pass
//!   through unchanged and indirect results land in a per-call buffer;
//! * `async` functions are re-exported as async (callback) lifts and forwarded with
//!   async-lowered calls. When the provider completes immediately the forwarder returns
//!   the result within the same task; when the provider genuinely suspends, the forwarder
//!   parks the call in its own waitable set and finishes it from its callback once the
//!   provider's subtask returns -- a configured provider keeps the provider's own
//!   blocking behavior. Cancellation of an in-flight forwarded call is not supported yet
//!   (it traps); see plan/03 Decisions.
//!
//! The result is an ordinary provider: composable, sealable, and byte-deterministic for
//! the same operands. The configured behavior end-to-end is exercised by the runtime and
//! integration suites.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

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
    Function, FunctionKind, Handle, InterfaceId, Mangling, Resolve, SizeAlign, Type, TypeDefKind,
    TypeId, TypeOwner, WorldItem, WorldKey,
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

/// The mask selecting the subtask status from an async-lowered call's packed return
/// value; the remaining upper bits are the subtask's waitable handle.
const SUBTASK_STATUS_MASK: i32 = 0xF;

/// Callback code returned by an async-lifted export (or its callback) when the task has
/// completed (`task.return` has been called).
const CALLBACK_CODE_EXIT: i32 = 0;

/// Callback code returned by an async-lifted export (or its callback) to wait for an
/// event on the waitable set packed into the upper bits.
const CALLBACK_CODE_WAIT: i32 = 2;

/// The event code delivered to an async export's callback when a subtask it is waiting
/// on changes state; the accompanying payload is the subtask's new status.
const EVENT_SUBTASK: i32 = 1;

/// One canonical-ABI constant baked into the binder's call to `configure`.
enum FlatConst {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// A string constant: stored in the binder's data segment, passed as (ptr, len).
    Str(String),
}

/// One memory load the binder performs to turn a canonically-stored value into a flat
/// core value (used to re-read an async call's results for `task.return`).
struct ResultLoad {
    /// Byte offset from the start of the result area.
    offset: u32,
    /// The load instruction shape.
    op: LoadOp,
}

/// The load instruction shapes [`ResultLoad`] distinguishes.
enum LoadOp {
    I32U8,
    I32S8,
    I32U16,
    I32S16,
    I32,
    I64,
    F32,
    F64,
}

/// How one forwarded API function is forwarded.
enum ForwardKind {
    /// A synchronous function: sync-lowered call, flat values pass through, indirect
    /// results land in a per-call buffer of this many bytes.
    Sync { result_area: u32 },
    /// An `async` function: async-lifted (callback) export forwarding to an
    /// async-lowered call, with a per-call frame for the suspended case.
    Async(AsyncForward),
}

/// Everything the async forwarder for one function needs beyond [`ForwardFunction`].
struct AsyncForward {
    /// The `task.return` intrinsic's core import module and field.
    task_return_module: String,
    task_return_field: String,
    /// The `task.return` intrinsic's core signature (the function's flat results).
    task_return_sig: WasmSignature,
    /// Loads that reconstruct the flat results from the per-call result area.
    result_loads: Vec<ResultLoad>,
    /// Bytes of the per-call result area (0 when the function has no result).
    result_size: u32,
    /// Bytes of the per-call frame header (subtask, waitable set, lent borrows).
    frame_header: u32,
}

/// One provider API function the binder forwards (gated on configuration).
struct ForwardFunction {
    /// The binder's core export name, e.g. `eo9:entropy/entropy@0.1.0#get-bytes`
    /// (async functions get the `[async-lift]` / `[callback][async-lift]` prefixes).
    export_name: String,
    /// The core import module (the interface extern name) and field (function name).
    import_module: String,
    import_field: String,
    /// The lowered (caller-side) core signature.
    import_sig: WasmSignature,
    /// The lifted (callee-side) core signature.
    export_sig: WasmSignature,
    /// Borrow-handle parameters this function receives, as (flat parameter index,
    /// index into [`BinderPlan::drop_intrinsics`]): the canonical ABI requires the
    /// callee to drop every borrow it was lent before its task completes.
    borrow_drops: Vec<(u32, usize)>,
    /// How the function is forwarded (sync passthrough or async with a callback).
    kind: ForwardKind,
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
    /// The config interface's extern name (import module of the sync `configure` call).
    config_extern: String,
    /// `configure`'s sync-lowered core signature.
    config_sig: WasmSignature,
    /// The baked-in arguments, in parameter order.
    constants: Vec<FlatConst>,
    /// The forwarded API functions, in interface/declaration order.
    forwards: Vec<ForwardFunction>,
    /// The resource-drop intrinsics referenced by [`ForwardFunction::borrow_drops`].
    drop_intrinsics: Vec<DropIntrinsic>,
    /// Whether any forwarded function is async (and the binder therefore needs the
    /// root async intrinsics).
    any_async: bool,
    /// Bytes reserved at the fixed scratch offset for `configure`'s indirect result.
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
    // `configure` is a synchronous export (it binds compile-time constants and must not
    // block), so it is sync-lowered: a plain canonical call that may itself synchronously
    // reenter another configured provider's `configure`. (It used to be async-lowered to
    // dodge the "a sync task may not block on an async export" rule; that made nested
    // configured compositions untypable -- the bug-1 trap. See plan/03 D17 + SPEC.)
    let config_sig = resolve.wasm_signature(AbiVariant::GuestImport, &function);
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
        &[],
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
/// freestanding functions (synchronous or `async`), no resources of its own (resources
/// shared through a types-only interface are fine -- they pass through as plain
/// handles), and borrow handles only as direct parameters (so the forwarder can locate
/// and release them). Per-function ABI limits (parameter flattening, async result
/// shapes) are checked by [`plan_binder`].
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
        if !matches!(
            function.kind,
            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
        ) {
            return Err(format!(
                "API function `{extern_name}#{name}` is not a freestanding function; \
                 compose-time configuration of such providers is not supported yet"
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

/// The loads that rebuild an `async` function's flat results from its canonically-stored
/// result area (for `task.return`), plus the area's size in bytes.
///
/// Only result shapes whose canonical memory layout can be re-read with straight-line
/// loads are supported: nothing, scalars, enums, handles to shared (types-interface)
/// resources, strings, and lists. Variants (including `option`/`result`), records,
/// tuples, and flags would need discriminant-dependent reloading and are rejected with a
/// clear error instead.
fn async_result_plan(
    resolve: &Resolve,
    sizes: &SizeAlign,
    extern_name: &str,
    name: &str,
    function: &Function,
) -> Result<(Vec<ResultLoad>, u32), String> {
    let Some(result) = &function.result else {
        return Ok((Vec::new(), 0));
    };
    let unsupported = |what: &str| {
        format!(
            "API function `{extern_name}#{name}` is async and returns {what}; forwarding \
             such results through compose-time configuration is not supported yet"
        )
    };

    // Follow type aliases down to the underlying type.
    let mut ty = *result;
    while let Type::Id(id) = ty {
        match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => ty = *inner,
            _ => break,
        }
    }

    let at = |offset: u32, op: LoadOp| ResultLoad { offset, op };
    let size = sizes.size(result).size_wasm32() as u32;
    let loads = match ty {
        Type::Bool | Type::U8 => vec![at(0, LoadOp::I32U8)],
        Type::S8 => vec![at(0, LoadOp::I32S8)],
        Type::U16 => vec![at(0, LoadOp::I32U16)],
        Type::S16 => vec![at(0, LoadOp::I32S16)],
        Type::U32 | Type::S32 | Type::Char => vec![at(0, LoadOp::I32)],
        Type::U64 | Type::S64 => vec![at(0, LoadOp::I64)],
        Type::F32 => vec![at(0, LoadOp::F32)],
        Type::F64 => vec![at(0, LoadOp::F64)],
        Type::String => vec![at(0, LoadOp::I32), at(4, LoadOp::I32)],
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Handle(_) => vec![at(0, LoadOp::I32)],
            TypeDefKind::Enum(_) => vec![at(
                0,
                match size {
                    1 => LoadOp::I32U8,
                    2 => LoadOp::I32U16,
                    _ => LoadOp::I32,
                },
            )],
            TypeDefKind::List(_) => vec![at(0, LoadOp::I32), at(4, LoadOp::I32)],
            TypeDefKind::Variant(_) | TypeDefKind::Option(_) | TypeDefKind::Result(_) => {
                return Err(unsupported("a variant-shaped value"));
            }
            _ => return Err(unsupported("a composite value")),
        },
        _ => return Err(unsupported("a value of this type")),
    };
    Ok((loads, size))
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

    let scratch_size = result_size(config_function);
    let mut any_async = false;
    let mut forwards = Vec::new();
    let mut drop_intrinsics: Vec<DropIntrinsic> = Vec::new();
    for (extern_name, id) in forward_interfaces {
        for (name, function) in &resolve.interfaces[*id].functions {
            let is_async = function.kind.is_async();
            let (import_variant, export_variant) = if is_async {
                (AbiVariant::GuestImportAsync, AbiVariant::GuestExportAsync)
            } else {
                (AbiVariant::GuestImport, AbiVariant::GuestExport)
            };
            let import_sig = resolve.wasm_signature(import_variant, function);
            let export_sig = resolve.wasm_signature(export_variant, function);

            // Async forwarders move every flat parameter from the lifted export to the
            // lowered call; the async-lowered side flattens at most four parameters, so
            // anything wider (or anything indirect) is rejected with a clear error.
            if is_async && (import_sig.indirect_params || export_sig.indirect_params) {
                return Err(format!(
                    "API function `{extern_name}#{name}` is async and takes too many (or \
                     too large) parameters for the configuration binder to forward; \
                     compose-time configuration of such providers is not supported yet"
                ));
            }

            // Locate every borrow-handle parameter: the forwarder must release them
            // before its task completes. Flat positions only exist when parameters are
            // passed flat, so the (unrealistic) indirect-parameters-with-borrows
            // combination is rejected rather than mishandled.
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

            let kind = if is_async {
                any_async = true;
                let (result_loads, result_bytes) =
                    async_result_plan(resolve, &sizes, extern_name, name, function)?;
                let key = WorldKey::Name((*extern_name).clone());
                let (task_return_module, task_return_field, task_return_sig) =
                    function.task_return_import(resolve, Some(&key), Mangling::Legacy);
                if result_loads.len() != task_return_sig.params.len() {
                    return Err(format!(
                        "API function `{extern_name}#{name}`: the binder's result reload \
                         plan ({} values) does not match the task-return signature ({} \
                         parameters); this is a bug in the configuration binder",
                        result_loads.len(),
                        task_return_sig.params.len()
                    ));
                }
                ForwardKind::Async(AsyncForward {
                    task_return_module,
                    task_return_field,
                    task_return_sig,
                    result_loads,
                    result_size: result_bytes,
                    frame_header: (8 + 4 * borrow_drops.len() as u32).next_multiple_of(8),
                })
            } else {
                ForwardKind::Sync {
                    result_area: if import_sig.retptr {
                        result_size(function).max(8)
                    } else {
                        0
                    },
                }
            };

            forwards.push(ForwardFunction {
                export_name: format!("{extern_name}#{name}"),
                import_module: extern_name.clone(),
                import_field: name.clone(),
                import_sig,
                export_sig,
                borrow_drops,
                kind,
            });
        }
    }

    Ok(BinderPlan {
        config_extern: config_extern.to_string(),
        config_sig,
        constants,
        forwards,
        drop_intrinsics,
        any_async,
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

/// The function indices of the root async intrinsics the binder imports when it forwards
/// at least one async function.
#[derive(Default, Clone, Copy)]
struct RootIntrinsics {
    waitable_set_new: u32,
    waitable_join: u32,
    waitable_set_drop: u32,
    subtask_drop: u32,
    context_set: u32,
    context_get: u32,
}

/// The core function indices belonging to one forwarded function: its lowered API import
/// and (for async functions) its `task.return` intrinsic.
struct ForwardIndices {
    import: u32,
    task_return: u32,
}

/// Builds the binder's core module: the sync-lowered `configure` import, a lowered
/// import and a gating forwarder export for every provider API function (sync
/// passthroughs for sync functions, async-callback lifts for async ones), a one-shot
/// configuration flag, an in-flight call counter, and a bump allocator for canonical-ABI
/// lifting.
fn synthesize_binder_module(plan: &BinderPlan) -> Vec<u8> {
    let layout = layout(plan);

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    // Imported functions first (they occupy the low function indices): the sync-lowered
    // `configure`, the lowered API functions (with their task-return intrinsics), the
    // resource-drop intrinsics, then the root async intrinsics.
    let mut next_import = 0u32;

    let configure_import = synth::push_signature(&mut types, &plan.config_sig);
    imports.import(
        &plan.config_extern,
        CONFIGURE,
        wasm_encoder::EntityType::Function(configure_import),
    );
    let configure_func = next_import;
    next_import += 1;

    let mut forward_indices = Vec::new();
    for forward in &plan.forwards {
        let ty = synth::push_signature(&mut types, &forward.import_sig);
        let field = match &forward.kind {
            ForwardKind::Sync { .. } => forward.import_field.clone(),
            ForwardKind::Async(_) => format!("[async-lower]{}", forward.import_field),
        };
        imports.import(
            &forward.import_module,
            &field,
            wasm_encoder::EntityType::Function(ty),
        );
        let import = next_import;
        next_import += 1;

        let task_return = if let ForwardKind::Async(async_forward) = &forward.kind {
            let ty = synth::push_signature(&mut types, &async_forward.task_return_sig);
            imports.import(
                &async_forward.task_return_module,
                &async_forward.task_return_field,
                wasm_encoder::EntityType::Function(ty),
            );
            let index = next_import;
            next_import += 1;
            index
        } else {
            u32::MAX
        };
        forward_indices.push(ForwardIndices {
            import,
            task_return,
        });
    }

    let drop_type = types.len();
    types.ty().function([ValType::I32], []);
    let mut drop_funcs = Vec::new();
    for intrinsic in &plan.drop_intrinsics {
        imports.import(
            &intrinsic.module,
            &intrinsic.field,
            wasm_encoder::EntityType::Function(drop_type),
        );
        drop_funcs.push(next_import);
        next_import += 1;
    }

    let mut root = RootIntrinsics::default();
    if plan.any_async {
        let returns_i32 = types.len();
        types.ty().function([], [ValType::I32]);
        let takes_i32 = types.len();
        types.ty().function([ValType::I32], []);
        let takes_two_i32 = types.len();
        types.ty().function([ValType::I32, ValType::I32], []);

        let root_intrinsics: [(&str, u32, &mut u32); 6] = [
            (
                "[waitable-set-new]",
                returns_i32,
                &mut root.waitable_set_new,
            ),
            ("[waitable-join]", takes_two_i32, &mut root.waitable_join),
            (
                "[waitable-set-drop]",
                takes_i32,
                &mut root.waitable_set_drop,
            ),
            ("[subtask-drop]", takes_i32, &mut root.subtask_drop),
            ("[context-set-0]", takes_i32, &mut root.context_set),
            ("[context-get-0]", returns_i32, &mut root.context_get),
        ];
        for (field, ty, slot) in root_intrinsics {
            imports.import("$root", field, wasm_encoder::EntityType::Function(ty));
            *slot = next_import;
            next_import += 1;
        }
    }

    // Defined functions: cabi_realloc, the one-shot gate, then the forwarders (sync
    // forwarders are one function each; async forwarders are an entry plus a callback).
    let mut next_func = next_import;

    let realloc_type = types.len();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    functions.function(realloc_type);
    code.function(&realloc_body());
    let realloc_func = next_func;
    next_func += 1;

    let gate_type = types.len();
    types.ty().function([], []);
    functions.function(gate_type);
    code.function(&gate_body(plan, &layout, configure_func));
    let gate_func = next_func;
    next_func += 1;

    for (forward, indices) in plan.forwards.iter().zip(&forward_indices) {
        match &forward.kind {
            ForwardKind::Sync { result_area } => {
                let ty = synth::push_signature(&mut types, &forward.export_sig);
                functions.function(ty);
                code.function(&sync_forward_body(
                    forward,
                    &layout,
                    *result_area,
                    indices.import,
                    gate_func,
                    realloc_func,
                    &drop_funcs,
                ));
                exports.export(&forward.export_name, ExportKind::Func, next_func);
                next_func += 1;
            }
            ForwardKind::Async(async_forward) => {
                let ty = synth::push_signature(&mut types, &forward.export_sig);
                functions.function(ty);
                code.function(&async_entry_body(
                    forward,
                    async_forward,
                    &layout,
                    indices,
                    gate_func,
                    realloc_func,
                    &drop_funcs,
                    &root,
                ));
                exports.export(
                    &format!("[async-lift]{}", forward.export_name),
                    ExportKind::Func,
                    next_func,
                );
                next_func += 1;

                let callback_type = types.len();
                types
                    .ty()
                    .function([ValType::I32, ValType::I32, ValType::I32], [ValType::I32]);
                functions.function(callback_type);
                code.function(&async_callback_body(
                    forward,
                    async_forward,
                    indices,
                    &drop_funcs,
                    &root,
                ));
                exports.export(
                    &format!("[callback][async-lift]{}", forward.export_name),
                    ExportKind::Func,
                    next_func,
                );
                next_func += 1;
            }
        }
    }

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    // Globals: 0 = bump pointer, 1 = "configure has run" flag, 2 = in-flight call count
    // (the bump pointer is only reset when nothing is in flight).
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(layout.heap_base as i32),
    );
    for _ in 0..2 {
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
    }

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

/// The one-shot gate: call `configure` (sync-lowered) with the baked-in constants,
/// require that it returned success, and mark the provider configured. An error from
/// `configure` (an invalid baked value) traps, so it fails before the consumer observes
/// any API behavior. Because `configure` is synchronous it can run from a synchronous
/// caller and may itself reenter another configured provider's `configure`.
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

    // Sync-lowered call: there is no subtask status to inspect. Any results small enough
    // to be returned by value (rather than through the retptr) arrive on the stack; drop
    // them -- the binder only needs the side effect of `configure` having bound the
    // provider's state. All standard configs return `result<x-impl, string>`, which is
    // wide enough to use the retptr, so this loop is empty for them and the discriminant
    // is read from `scratch` below.
    for _ in &plan.config_sig.results {
        f.instruction(&Instruction::Drop);
    }

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

/// Emits the configuration gate: if `configure` has not run yet, run it now.
fn push_gate(f: &mut wasm_encoder::Function, gate_func: u32) {
    f.instruction(&Instruction::GlobalGet(1));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Call(gate_func));
    f.instruction(&Instruction::End);
}

/// Emits the per-call entry bookkeeping: reset the bump allocator if no other call is in
/// flight (its previous allocations have been consumed by then), then count this call as
/// in flight.
fn push_call_enter(f: &mut wasm_encoder::Function, layout: &Layout) {
    f.instruction(&Instruction::GlobalGet(2));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I32Const(layout.heap_base as i32));
    f.instruction(&Instruction::GlobalSet(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::GlobalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(2));
}

/// Emits the per-call exit bookkeeping: this call is no longer in flight.
fn push_call_exit(f: &mut wasm_encoder::Function) {
    f.instruction(&Instruction::GlobalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::GlobalSet(2));
}

/// Emits a bump allocation of `size` bytes (8-aligned), leaving the pointer on the stack.
fn push_alloc(f: &mut wasm_encoder::Function, realloc_func: u32, size: u32) {
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Const(size as i32));
    f.instruction(&Instruction::Call(realloc_func));
}

/// Emits the loads that rebuild a forwarded async call's flat results (for `task.return`)
/// from the canonically-stored result area at `base` bytes past the frame pointer in
/// `frame_local`.
fn push_result_loads(
    f: &mut wasm_encoder::Function,
    frame_local: u32,
    base: u32,
    loads: &[ResultLoad],
) {
    for load in loads {
        f.instruction(&Instruction::LocalGet(frame_local));
        let memarg = |align: u32| MemArg {
            offset: u64::from(base + load.offset),
            align,
            memory_index: 0,
        };
        f.instruction(&match load.op {
            LoadOp::I32U8 => Instruction::I32Load8U(memarg(0)),
            LoadOp::I32S8 => Instruction::I32Load8S(memarg(0)),
            LoadOp::I32U16 => Instruction::I32Load16U(memarg(1)),
            LoadOp::I32S16 => Instruction::I32Load16S(memarg(1)),
            LoadOp::I32 => Instruction::I32Load(memarg(2)),
            LoadOp::I64 => Instruction::I64Load(memarg(3)),
            LoadOp::F32 => Instruction::F32Load(memarg(2)),
            LoadOp::F64 => Instruction::F64Load(memarg(3)),
        });
    }
}

/// A synchronous gating forwarder: ensure `configure` has run, forward the call to the
/// provider -- passing flat values through unchanged and routing indirect results via a
/// per-call buffer -- and release any borrow handles the caller lent us, as the canonical
/// ABI requires before an export returns.
fn sync_forward_body(
    forward: &ForwardFunction,
    layout: &Layout,
    result_area: u32,
    import_index: u32,
    gate_func: u32,
    realloc_func: u32,
    drop_funcs: &[u32],
) -> wasm_encoder::Function {
    let params = forward.export_sig.params.len() as u32;
    let retptr_local = params;
    let mut f = wasm_encoder::Function::new([(1, ValType::I32)]);

    push_gate(&mut f, gate_func);
    push_call_enter(&mut f, layout);

    if forward.import_sig.retptr {
        push_alloc(&mut f, realloc_func, result_area);
        f.instruction(&Instruction::LocalSet(retptr_local));
    }

    for index in 0..params {
        f.instruction(&Instruction::LocalGet(index));
    }
    if forward.import_sig.retptr {
        f.instruction(&Instruction::LocalGet(retptr_local));
    }
    f.instruction(&Instruction::Call(import_index));

    // Release the borrows we were lent (any direct results stay untouched further down
    // the operand stack).
    for (flat_index, drop) in &forward.borrow_drops {
        f.instruction(&Instruction::LocalGet(*flat_index));
        f.instruction(&Instruction::Call(drop_funcs[*drop]));
    }

    push_call_exit(&mut f);

    if forward.export_sig.retptr {
        f.instruction(&Instruction::LocalGet(retptr_local));
    }
    f.instruction(&Instruction::End);
    f
}

/// An async gating forwarder's entry function (the `[async-lift]` export): ensure
/// `configure` has run, allocate a per-call frame, make the async-lowered call, and
/// either complete the task immediately (the provider already returned) or park the
/// provider's subtask in a fresh waitable set and wait for the callback.
#[expect(
    clippy::too_many_arguments,
    reason = "one-shot generator, plain data in"
)]
fn async_entry_body(
    forward: &ForwardFunction,
    async_forward: &AsyncForward,
    layout: &Layout,
    indices: &ForwardIndices,
    gate_func: u32,
    realloc_func: u32,
    drop_funcs: &[u32],
    root: &RootIntrinsics,
) -> wasm_encoder::Function {
    let params = forward.export_sig.params.len() as u32;
    let frame = params;
    let status = params + 1;
    let set = params + 2;
    let header = async_forward.frame_header;
    let mut f = wasm_encoder::Function::new([(3, ValType::I32)]);

    push_gate(&mut f, gate_func);
    push_call_enter(&mut f, layout);

    // The per-call frame: subtask, waitable set, the lent borrow handles, then the
    // result area the async-lowered call writes into.
    push_alloc(&mut f, realloc_func, header + async_forward.result_size);
    f.instruction(&Instruction::LocalSet(frame));
    for (slot, (flat_index, _)) in forward.borrow_drops.iter().enumerate() {
        f.instruction(&Instruction::LocalGet(frame));
        f.instruction(&Instruction::LocalGet(*flat_index));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: u64::from(8 + 4 * slot as u32),
            align: 2,
            memory_index: 0,
        }));
    }

    // The async-lowered call: flat parameters pass through unchanged; results (if any)
    // go to the frame's result area.
    for index in 0..params {
        f.instruction(&Instruction::LocalGet(index));
    }
    if forward.import_sig.retptr {
        f.instruction(&Instruction::LocalGet(frame));
        f.instruction(&Instruction::I32Const(header as i32));
        f.instruction(&Instruction::I32Add);
    }
    f.instruction(&Instruction::Call(indices.import));
    f.instruction(&Instruction::LocalSet(status));

    // Already returned: release the lent borrows, return the results, and exit the task.
    f.instruction(&Instruction::LocalGet(status));
    f.instruction(&Instruction::I32Const(SUBTASK_STATUS_MASK));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(SUBTASK_RETURNED));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    for (flat_index, drop) in &forward.borrow_drops {
        f.instruction(&Instruction::LocalGet(*flat_index));
        f.instruction(&Instruction::Call(drop_funcs[*drop]));
    }
    push_result_loads(&mut f, frame, header, &async_forward.result_loads);
    f.instruction(&Instruction::Call(indices.task_return));
    push_call_exit(&mut f);
    f.instruction(&Instruction::I32Const(CALLBACK_CODE_EXIT));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // The provider suspended: park its subtask in a fresh waitable set, remember
    // everything the callback needs in the frame (and the frame in the task-local
    // context slot), and wait.
    f.instruction(&Instruction::LocalGet(status));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32ShrU);
    f.instruction(&Instruction::LocalSet(status));
    f.instruction(&Instruction::Call(root.waitable_set_new));
    f.instruction(&Instruction::LocalSet(set));
    f.instruction(&Instruction::LocalGet(status));
    f.instruction(&Instruction::LocalGet(set));
    f.instruction(&Instruction::Call(root.waitable_join));
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::LocalGet(status));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::LocalGet(set));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::Call(root.context_set));
    f.instruction(&Instruction::LocalGet(set));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32Const(CALLBACK_CODE_WAIT));
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::End);
    f
}

/// An async gating forwarder's callback: invoked when the parked provider subtask
/// changes state. Anything other than this call's own subtask completing -- including a
/// cancellation request -- is unsupported and traps.
fn async_callback_body(
    forward: &ForwardFunction,
    async_forward: &AsyncForward,
    indices: &ForwardIndices,
    drop_funcs: &[u32],
    root: &RootIntrinsics,
) -> wasm_encoder::Function {
    // Parameters: 0 = event code, 1 = the waitable that changed, 2 = its payload.
    let frame = 3;
    let header = async_forward.frame_header;
    let mut f = wasm_encoder::Function::new([(1, ValType::I32)]);

    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(EVENT_SUBTASK));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::Call(root.context_get));
    f.instruction(&Instruction::LocalSet(frame));

    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);

    // Not finished yet (e.g. a starting -> started transition): keep waiting.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(SUBTASK_RETURNED));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32Const(CALLBACK_CODE_WAIT));
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // The provider returned: retire the subtask and the waitable set, release the lent
    // borrows, return the results, and exit the task.
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::Call(root.subtask_drop));
    f.instruction(&Instruction::LocalGet(frame));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::Call(root.waitable_set_drop));
    for (slot, (_, drop)) in forward.borrow_drops.iter().enumerate() {
        f.instruction(&Instruction::LocalGet(frame));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: u64::from(8 + 4 * slot as u32),
            align: 2,
            memory_index: 0,
        }));
        f.instruction(&Instruction::Call(drop_funcs[*drop]));
    }
    push_result_loads(&mut f, frame, header, &async_forward.result_loads);
    f.instruction(&Instruction::Call(indices.task_return));
    push_call_exit(&mut f);
    f.instruction(&Instruction::I32Const(CALLBACK_CODE_EXIT));
    f.instruction(&Instruction::End);
    f
}
