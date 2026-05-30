//! `configure` -- binding a provider's compose-time configuration constants
//! (SPEC.md "Binary or provider, never both": `configure : provider × args → provider`).
//!
//! A configurable provider ships a small exported `*-config` interface whose `configure`
//! entry binds the configuration and returns the provider's root capability handle.
//! `configure(provider, args)` bakes the given constants in -- the **alias + bind**
//! construction (plan/03 D21):
//!
//! * the WAVE-encoded `args` are type-checked against `configure`'s declared parameters
//!   and lowered to canonical-ABI constants -- scalars, `char`, `string`, enums, and
//!   (nested arbitrarily) records, tuples, options, and lists of these. Strings, list
//!   bodies, and spilled parameter records are laid out at compose time in a constant
//!   arena that becomes the binder's data segment;
//! * the provider's API and types exports are re-exported **directly** -- plain aliases,
//!   no forwarding, no proxying: resources keep their identity and configured calls cost
//!   nothing extra. The config interface is sealed away (the consumer can neither
//!   observe nor re-run the configuration);
//! * a tiny *binder* component is synthesized alongside that imports only the provider's
//!   config interface and exports one extra interface -- `eo9:rt/configured`, whose
//!   parameterless `bind` invokes the provider's `configure` with the baked-in constants
//!   (idempotently; an invalid baked value traps).
//!
//! The executor contract completes the design: after instantiating any component and
//! before the first entry into it, every Eo9 executor (the usermode runtime, the kernel,
//! the browser blob) calls `bind` if the component exports `eo9:rt/configured`. That step
//! exists because nothing may call out of a component while it is being instantiated, so
//! configuration cannot apply itself; the host has to poke the one entrypoint. `$`/`&`
//! propagate the entrypoint through composition -- when both operands carry one they are
//! merged, provider first -- so the outermost component of any composition reaches every
//! nested configured provider (see `compose.rs`).
//!
//! Because the provider's API is aliased rather than forwarded, this construction works
//! for *every* provider shape -- including providers whose API interfaces own resources
//! (fs, disk, net, pci) and providers with arbitrary async functions, which the previous
//! forwarding binder had to reject.
//!
//! The result is an ordinary provider: composable, sealable, and byte-deterministic for
//! the same operands. The configured behavior end-to-end is exercised by the runtime and
//! integration suites.

use alloc::boxed::Box;
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
    Function, FunctionKind, Int, InterfaceId, Resolve, SizeAlign, Type, TypeDefKind, WorldItem,
};

use crate::compose::{
    encode as encode_graph, export_all, register, slot_annotations, wire_matching_imports,
};
use crate::describe::{CONFIG_SUFFIX, CONFIGURE, CONFIGURED_INTERFACE};
use crate::error::ConfigureError;
use crate::synth::BIND;
use crate::{Component, ComponentKind, Wiring, synth};

/// One canonical-ABI constant baked into the binder's call to `configure`.
enum FlatConst {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// A pointer into the constant arena (an arena-relative offset; rebased to an
    /// absolute address once the binder's memory layout is fixed).
    ArenaPtr(u32),
}

/// The lowered `configure` arguments: the flat constants pushed at the call site plus
/// the constant arena holding every string, list, and spilled-parameter body they
/// reference (canonical-ABI layout, little-endian, built at compose time).
struct LoweredArguments {
    /// The flat core constants, in canonical flattening order (or a single arena
    /// pointer when the parameters are passed indirectly).
    flats: Vec<FlatConst>,
    /// The constant arena: emitted verbatim as the binder's data segment.
    arena: Vec<u8>,
    /// Offsets within `arena` holding arena-relative `u32` pointers that must be
    /// rebased by the arena's absolute base address at layout time.
    relocs: Vec<u32>,
}

/// Hard cap on the constant arena (a runaway WAVE literal should fail cleanly, not OOM).
const ARENA_LIMIT: usize = 64 * 1024 * 1024;

/// Everything the binder core module is generated from.
struct BinderPlan {
    /// The config interface's extern name (import module of the sync `configure` call).
    config_extern: String,
    /// `configure`'s sync-lowered core signature.
    config_sig: WasmSignature,
    /// The baked-in arguments, in parameter order.
    constants: LoweredArguments,
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

    // `configure` is a synchronous export (it binds compile-time constants and must not
    // block), so it is sync-lowered: a plain canonical call that may itself synchronously
    // reenter another configured provider's `configure`. (It used to be async-lowered to
    // dodge the "a sync task may not block on an async export" rule; that made nested
    // configured compositions untypable -- the bug-1 trap. See plan/03 D17 + SPEC.)
    let config_sig = resolve.wasm_signature(AbiVariant::GuestImport, &function);

    // Type-check the WAVE arguments against the declared parameters and lower them to
    // canonical-ABI constants (flat values plus the constant arena). When the parameter
    // list is too wide to pass flat, the arguments are spilled to a single
    // canonically-laid-out parameter record in the arena instead.
    let constants = bind_arguments(&resolve, &function, &config_sig, args)?;

    // The provider must actually export something other than its config interface --
    // an API or types surface the configured wrapper re-exports. (A config-only
    // provider is the SPEC "export shape encodes whether configuration is required"
    // situation; configuring it would yield an empty provider.)
    let has_api_exports = exported_interfaces
        .iter()
        .any(|(extern_name, _)| *extern_name != config_export.extern_name);
    if !has_api_exports {
        return Err(internal(
            "the provider exports nothing but its config interface; there is no API \
             surface for the configuration to apply to"
                .to_string(),
        ));
    }

    let mut sizes = SizeAlign::default();
    sizes.fill(&resolve);
    let scratch_size = function
        .result
        .as_ref()
        .map(|ty| sizes.size(ty).size_wasm32() as u32)
        .unwrap_or(0);
    let plan = BinderPlan {
        config_extern: config_export.extern_name.clone(),
        config_sig,
        constants,
        scratch_size: scratch_size.next_multiple_of(8).max(16),
    };

    // Synthesize the binder (the `eo9:rt/configured` entrypoint) and wire it next to --
    // not in front of -- the provider.
    let binder =
        build_binder(&mut resolve, &plan, config_interface, &function).map_err(internal)?;

    let compose_err =
        |err: crate::ComposeError| internal(format!("failed to assemble the wrapper: {err}"));
    let mut graph = wac_graph::CompositionGraph::new();
    let provider_pkg = register(&mut graph, "provider", provider.bytes()).map_err(compose_err)?;
    let binder_pkg = register(&mut graph, "binder", &binder).map_err(compose_err)?;
    let provider_inst = graph.instantiate(provider_pkg);
    let binder_inst = graph.instantiate(binder_pkg);

    // The binder's imports (the config interface plus whatever types/API interfaces its
    // signature drags in) are satisfied by the provider where the provider exports them;
    // anything else stays a residual import, merged with the provider's own residuals.
    wire_matching_imports(
        &mut graph,
        provider_pkg,
        provider_inst,
        binder_pkg,
        binder_inst,
        &[],
    )
    .map_err(compose_err)?;

    // The wrapper exports the binder's `eo9:rt/configured` entrypoint plus everything
    // the provider exports -- API interfaces aliased directly (the binder is not in the
    // call path) -- except the config interface, which is sealed away.
    export_all(&mut graph, binder_pkg, binder_inst, None).map_err(compose_err)?;
    let skip_slots: Vec<String> = vec![config_export.slot.clone()];
    export_all(&mut graph, provider_pkg, provider_inst, Some(&skip_slots)).map_err(compose_err)?;

    let arg_labels: Vec<String> = args
        .iter()
        .map(|(name, value)| format!("{}={}", name.as_ref(), value.as_ref()))
        .collect();
    encode_graph(&graph, &slot_annotations(&[provider]))
        .map(|component| {
            component.with_wiring(Wiring::Configure {
                args: arg_labels,
                body: Box::new(provider.wiring().clone()),
            })
        })
        .map_err(compose_err)
}

/// Checks the supplied arguments against `configure`'s parameters and lowers them to
/// their canonical-ABI constants, in parameter order: flat core values when the
/// parameter list passes flat, or a single pointer to a canonically-laid-out parameter
/// record in the constant arena when it is passed indirectly.
fn bind_arguments<N, V>(
    resolve: &Resolve,
    function: &Function,
    config_sig: &WasmSignature,
    args: &[(N, V)],
) -> Result<LoweredArguments, ConfigureError>
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

    // Parse every parameter's WAVE text against its declared type first, so type errors
    // surface per argument before any lowering happens.
    let mut values = Vec::new();
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
        values.push(parse_argument(resolve, &param.name, &param.ty, text)?);
    }

    let mut sizes = SizeAlign::default();
    sizes.fill(resolve);
    let mut lowerer = Lowerer {
        resolve,
        sizes,
        arena: Vec::new(),
        relocs: Vec::new(),
    };

    let mut flats = Vec::new();
    if config_sig.indirect_params {
        // The parameters are passed indirectly: lay them out as one canonical parameter
        // record in the arena and pass its address as the single flat value.
        let param_types: Vec<Type> = function.params.iter().map(|p| p.ty).collect();
        let offsets = lowerer.sizes.field_offsets(param_types.iter());
        let info = lowerer.sizes.record(param_types.iter());
        let base = lowerer
            .reserve(
                info.size.size_wasm32(),
                info.align.align_wasm32(),
                "the spilled parameter record",
            )
            .map_err(ConfigureError::Internal)?;
        let offsets: Vec<u32> = offsets
            .iter()
            .map(|(offset, _)| offset.size_wasm32() as u32)
            .collect();
        for ((param, value), offset) in function.params.iter().zip(&values).zip(offsets) {
            lowerer
                .store(value, &param.ty, base + offset)
                .map_err(|message| ConfigureError::InvalidArgument {
                    name: param.name.clone(),
                    message,
                })?;
        }
        flats.push(FlatConst::ArenaPtr(base));
    } else {
        for (param, value) in function.params.iter().zip(&values) {
            lowerer
                .lower_flat(value, &param.ty, &mut flats)
                .map_err(|message| ConfigureError::InvalidArgument {
                    name: param.name.clone(),
                    message,
                })?;
        }
        // The flat constants must line up one-for-one with `configure`'s lowered core
        // parameters (minus the appended return pointer, pushed by the gate itself).
        let expected = config_sig.params.len() - usize::from(config_sig.retptr);
        if flats.len() != expected {
            return Err(ConfigureError::Internal(format!(
                "lowered {} flat constants for `configure` but its core signature takes \
                 {expected}; this is a bug in compose-time configuration",
                flats.len()
            )));
        }
    }

    Ok(LoweredArguments {
        flats,
        arena: lowerer.arena,
        relocs: lowerer.relocs,
    })
}

/// Parses one WAVE value against its declared WIT type (following aliases), checking
/// first that the type is something compose-time configuration can bake.
fn parse_argument(
    resolve: &Resolve,
    name: &str,
    ty: &Type,
    text: &str,
) -> Result<Value, ConfigureError> {
    let ty = resolve_alias(resolve, ty);
    if let Err(what) = ensure_bakeable(resolve, &ty) {
        // The message text lives in the Display impl; the variant carries the pieces so
        // callers can match on the refusal instead of substring-searching it.
        return Err(ConfigureError::UnbakeableType {
            name: name.to_string(),
            kind: what.to_string(),
        });
    }
    let wave_type = wave_type(resolve, &ty).ok_or_else(|| {
        ConfigureError::Internal(format!(
            "parameter `{name}`: failed to derive a WAVE type for compose-time baking"
        ))
    })?;
    wasm_wave::from_str(&wave_type, text).map_err(|err| ConfigureError::InvalidArgument {
        name: name.to_string(),
        message: format!(
            "does not parse as `{}`: {err}",
            crate::describe::type_text(resolve, &ty)
        ),
    })
}

/// Follows type aliases down to the underlying type.
fn resolve_alias(resolve: &Resolve, ty: &Type) -> Type {
    let mut ty = *ty;
    while let Type::Id(id) = ty {
        match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => ty = *inner,
            _ => break,
        }
    }
    ty
}

/// Checks that a parameter type is bakeable at compose time: scalars, `char`, `string`,
/// enums, records, tuples, options, and lists of these, in any nesting. Anything that
/// carries authority (handles, resources) or needs discriminant-dependent flattening
/// beyond `option` (variants, results, flags) is rejected.
/// The `Err` is the short name of the offending kind ("variant", "resource handle", ...),
/// which [`ConfigureError::UnbakeableType`] carries so callers can match on it.
fn ensure_bakeable(resolve: &Resolve, ty: &Type) -> Result<(), &'static str> {
    let ty = resolve_alias(resolve, ty);
    match ty {
        Type::Bool
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::S8
        | Type::S16
        | Type::S32
        | Type::S64
        | Type::F32
        | Type::F64
        | Type::Char
        | Type::String => Ok(()),
        Type::ErrorContext => Err("error-context"),
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Enum(_) => Ok(()),
            TypeDefKind::List(element) => ensure_bakeable(resolve, element),
            TypeDefKind::Option(payload) => ensure_bakeable(resolve, payload),
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    ensure_bakeable(resolve, &field.ty)?;
                }
                Ok(())
            }
            TypeDefKind::Tuple(tuple) => {
                for ty in &tuple.types {
                    ensure_bakeable(resolve, ty)?;
                }
                Ok(())
            }
            kind @ (TypeDefKind::Variant(_)
            | TypeDefKind::Result(_)
            | TypeDefKind::Flags(_)
            | TypeDefKind::Handle(_)
            | TypeDefKind::Resource
            | TypeDefKind::Future(_)
            | TypeDefKind::Stream(_)
            | TypeDefKind::Map(..)
            | TypeDefKind::FixedLengthList(..)) => Err(kind_name(kind)),
            TypeDefKind::Type(_) => unreachable!("aliases are resolved above"),
            TypeDefKind::Unknown => Err("unresolved type"),
        },
    }
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
        Type::Id(id) => value::resolve_wit_type(resolve, *id).ok()?,
        _ => return None,
    })
}

/// Lowers parsed WAVE values to canonical-ABI constants: flat core values for the call
/// site plus the constant arena holding string bytes, list elements, and spilled
/// parameter records (with relocations for every arena-internal pointer).
struct Lowerer<'a> {
    resolve: &'a Resolve,
    sizes: SizeAlign,
    arena: Vec<u8>,
    relocs: Vec<u32>,
}

impl Lowerer<'_> {
    /// Reserves `size` bytes at the next `align`-aligned arena offset, zero-filled.
    fn reserve(&mut self, size: usize, align: usize, what: &str) -> Result<u32, String> {
        let align = align.max(1);
        let padded = self.arena.len().next_multiple_of(align);
        let end = padded + size;
        if end > ARENA_LIMIT {
            return Err(format!(
                "{what} does not fit in the compose-time constant arena ({size} bytes \
                 requested, {ARENA_LIMIT} byte limit)"
            ));
        }
        self.arena.resize(end, 0);
        Ok(padded as u32)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) {
        let offset = offset as usize;
        self.arena[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Writes an arena-relative pointer at `offset` and records the relocation.
    fn write_rel_ptr(&mut self, offset: u32, target: u32) {
        self.write(offset, &target.to_le_bytes());
        self.relocs.push(offset);
    }

    /// Appends a string's bytes to the arena, returning (relative pointer, length).
    fn append_string(&mut self, text: &str) -> Result<(u32, u32), String> {
        let offset = self.reserve(text.len(), 1, "a string constant")?;
        self.write(offset, text.as_bytes());
        Ok((offset, text.len() as u32))
    }

    /// Appends a list's elements to the arena in canonical layout, returning
    /// (relative pointer, element count).
    fn append_list(&mut self, value: &Value, element: &Type) -> Result<(u32, u32), String> {
        let elements: Vec<_> = value.unwrap_list().collect();
        let size = self.sizes.size(element).size_wasm32();
        let align = self.sizes.align(element).align_wasm32();
        let base = self.reserve(size * elements.len(), align, "a list constant")?;
        for (index, element_value) in elements.iter().enumerate() {
            self.store(
                element_value.as_ref(),
                element,
                base + (index * size) as u32,
            )?;
        }
        Ok((base, elements.len() as u32))
    }

    /// Looks up a record field's value by name (the parse is typed, so a missing field
    /// can only mean an internal mismatch).
    fn record_field(value: &Value, name: &str) -> Result<Value, String> {
        for (field_name, field_value) in value.unwrap_record() {
            if field_name == name {
                return Ok(field_value.into_owned());
            }
        }
        Err(format!("record value is missing field `{name}`"))
    }

    /// Lowers one value to its flat core constants (the canonical flattening used when
    /// the parameter list is passed by value).
    fn lower_flat(
        &mut self,
        value: &Value,
        ty: &Type,
        flats: &mut Vec<FlatConst>,
    ) -> Result<(), String> {
        let resolve = self.resolve;
        let ty = resolve_alias(resolve, ty);
        match ty {
            Type::Bool => flats.push(FlatConst::I32(i32::from(value.unwrap_bool()))),
            Type::U8 => flats.push(FlatConst::I32(i32::from(value.unwrap_u8()))),
            Type::U16 => flats.push(FlatConst::I32(i32::from(value.unwrap_u16()))),
            Type::U32 => flats.push(FlatConst::I32(value.unwrap_u32() as i32)),
            Type::S8 => flats.push(FlatConst::I32(i32::from(value.unwrap_s8()))),
            Type::S16 => flats.push(FlatConst::I32(i32::from(value.unwrap_s16()))),
            Type::S32 => flats.push(FlatConst::I32(value.unwrap_s32())),
            Type::U64 => flats.push(FlatConst::I64(value.unwrap_u64() as i64)),
            Type::S64 => flats.push(FlatConst::I64(value.unwrap_s64())),
            Type::F32 => flats.push(FlatConst::F32(value.unwrap_f32())),
            Type::F64 => flats.push(FlatConst::F64(value.unwrap_f64())),
            Type::Char => flats.push(FlatConst::I32(value.unwrap_char() as i32)),
            Type::String => {
                let (ptr, len) = self.append_string(&value.unwrap_string())?;
                flats.push(FlatConst::ArenaPtr(ptr));
                flats.push(FlatConst::I32(len as i32));
            }
            Type::ErrorContext => return Err("error-context values are not bakeable".into()),
            Type::Id(id) => match &resolve.types[id].kind {
                TypeDefKind::Enum(e) => {
                    flats.push(FlatConst::I32(enum_discriminant(e, value)? as i32));
                }
                TypeDefKind::List(element) => {
                    let (ptr, len) = self.append_list(value, element)?;
                    flats.push(FlatConst::ArenaPtr(ptr));
                    flats.push(FlatConst::I32(len as i32));
                }
                TypeDefKind::Record(record) => {
                    for field in &record.fields {
                        let field_value = Self::record_field(value, &field.name)?;
                        self.lower_flat(&field_value, &field.ty, flats)?;
                    }
                }
                TypeDefKind::Tuple(tuple) => {
                    let values: Vec<_> = value.unwrap_tuple().collect();
                    if values.len() != tuple.types.len() {
                        return Err("tuple value arity mismatch".into());
                    }
                    for (item, item_ty) in values.iter().zip(&tuple.types) {
                        self.lower_flat(item.as_ref(), item_ty, flats)?;
                    }
                }
                TypeDefKind::Option(payload) => match value.unwrap_option() {
                    Some(inner) => {
                        flats.push(FlatConst::I32(1));
                        self.lower_flat(inner.as_ref(), payload, flats)?;
                    }
                    None => {
                        flats.push(FlatConst::I32(0));
                        self.push_zero_flats(payload, flats);
                    }
                },
                other => {
                    return Err(format!(
                        "values of this kind ({}) are not bakeable",
                        kind_name(other)
                    ));
                }
            },
        }
        Ok(())
    }

    /// Pushes a zero constant for every flat core value of `ty` (the payload slots of an
    /// absent `option`, which the canonical ABI still passes).
    fn push_zero_flats(&self, ty: &Type, flats: &mut Vec<FlatConst>) {
        let mut storage = [WasmType::I32; 32];
        let mut flat = FlatTypes::new(&mut storage);
        self.resolve.push_flat(ty, &mut flat);
        for wasm_type in flat.to_vec() {
            flats.push(match wasm_type {
                WasmType::I64 | WasmType::PointerOrI64 => FlatConst::I64(0),
                WasmType::F32 => FlatConst::F32(0.0),
                WasmType::F64 => FlatConst::F64(0.0),
                _ => FlatConst::I32(0),
            });
        }
    }

    /// Stores one value at `offset` in the arena using its canonical memory layout (the
    /// region must already be reserved; strings and lists it contains are appended).
    fn store(&mut self, value: &Value, ty: &Type, offset: u32) -> Result<(), String> {
        let resolve = self.resolve;
        let ty = resolve_alias(resolve, ty);
        match ty {
            Type::Bool => self.write(offset, &[u8::from(value.unwrap_bool())]),
            Type::U8 => self.write(offset, &value.unwrap_u8().to_le_bytes()),
            Type::S8 => self.write(offset, &value.unwrap_s8().to_le_bytes()),
            Type::U16 => self.write(offset, &value.unwrap_u16().to_le_bytes()),
            Type::S16 => self.write(offset, &value.unwrap_s16().to_le_bytes()),
            Type::U32 => self.write(offset, &value.unwrap_u32().to_le_bytes()),
            Type::S32 => self.write(offset, &value.unwrap_s32().to_le_bytes()),
            Type::U64 => self.write(offset, &value.unwrap_u64().to_le_bytes()),
            Type::S64 => self.write(offset, &value.unwrap_s64().to_le_bytes()),
            Type::F32 => self.write(offset, &value.unwrap_f32().to_le_bytes()),
            Type::F64 => self.write(offset, &value.unwrap_f64().to_le_bytes()),
            Type::Char => self.write(offset, &(value.unwrap_char() as u32).to_le_bytes()),
            Type::String => {
                let (ptr, len) = self.append_string(&value.unwrap_string())?;
                self.write_rel_ptr(offset, ptr);
                self.write(offset + 4, &len.to_le_bytes());
            }
            Type::ErrorContext => return Err("error-context values are not bakeable".into()),
            Type::Id(id) => match &resolve.types[id].kind {
                TypeDefKind::Enum(e) => {
                    let discriminant = enum_discriminant(e, value)?;
                    self.store_discriminant(offset, e.tag(), discriminant);
                }
                TypeDefKind::List(element) => {
                    let (ptr, len) = self.append_list(value, element)?;
                    self.write_rel_ptr(offset, ptr);
                    self.write(offset + 4, &len.to_le_bytes());
                }
                TypeDefKind::Record(record) => {
                    let field_types: Vec<Type> = record.fields.iter().map(|f| f.ty).collect();
                    let offsets = self.sizes.field_offsets(field_types.iter());
                    let offsets: Vec<u32> = offsets
                        .iter()
                        .map(|(off, _)| off.size_wasm32() as u32)
                        .collect();
                    for (field, field_offset) in record.fields.iter().zip(offsets) {
                        let field_value = Self::record_field(value, &field.name)?;
                        self.store(&field_value, &field.ty, offset + field_offset)?;
                    }
                }
                TypeDefKind::Tuple(tuple) => {
                    let values: Vec<_> = value.unwrap_tuple().collect();
                    if values.len() != tuple.types.len() {
                        return Err("tuple value arity mismatch".into());
                    }
                    let offsets = self.sizes.field_offsets(tuple.types.iter());
                    let offsets: Vec<u32> = offsets
                        .iter()
                        .map(|(off, _)| off.size_wasm32() as u32)
                        .collect();
                    for ((item, item_ty), item_offset) in
                        values.iter().zip(&tuple.types).zip(offsets)
                    {
                        self.store(item.as_ref(), item_ty, offset + item_offset)?;
                    }
                }
                TypeDefKind::Option(payload) => {
                    let payload_offset = self
                        .sizes
                        .payload_offset(Int::U8, [None, Some(payload)])
                        .size_wasm32() as u32;
                    match value.unwrap_option() {
                        Some(inner) => {
                            self.write(offset, &[1]);
                            self.store(inner.as_ref(), payload, offset + payload_offset)?;
                        }
                        None => self.write(offset, &[0]),
                    }
                }
                other => {
                    return Err(format!(
                        "values of this kind ({}) are not bakeable",
                        kind_name(other)
                    ));
                }
            },
        }
        Ok(())
    }

    /// Stores an enum discriminant in its canonical tag width.
    fn store_discriminant(&mut self, offset: u32, tag: Int, discriminant: u32) {
        match tag {
            Int::U8 => self.write(offset, &[discriminant as u8]),
            Int::U16 => self.write(offset, &(discriminant as u16).to_le_bytes()),
            Int::U32 | Int::U64 => self.write(offset, &discriminant.to_le_bytes()),
        }
    }
}

/// The case index of an enum value.
fn enum_discriminant(e: &wit_parser::Enum, value: &Value) -> Result<u32, String> {
    let case = value.unwrap_enum();
    e.cases
        .iter()
        .position(|c| c.name == case)
        .map(|index| index as u32)
        .ok_or_else(|| format!("`{case}` is not a case of the enum"))
}

/// A short human name for a type kind (for "not bakeable" messages).
fn kind_name(kind: &TypeDefKind) -> &'static str {
    match kind {
        TypeDefKind::Variant(_) => "variant",
        TypeDefKind::Result(_) => "result",
        TypeDefKind::Flags(_) => "flags",
        TypeDefKind::Handle(_) | TypeDefKind::Resource => "resource handle",
        TypeDefKind::Future(_) => "future",
        TypeDefKind::Stream(_) => "stream",
        TypeDefKind::Map(..) => "map",
        TypeDefKind::FixedLengthList(..) => "fixed-length list",
        _ => "unsupported type",
    }
}

/// Synthesizes the binder component: it imports the provider's config interface and
/// exports the `eo9:rt/configured` entrypoint whose `bind` applies the baked constants.
fn build_binder(
    resolve: &mut Resolve,
    plan: &BinderPlan,
    config_interface: InterfaceId,
    config_function: &Function,
) -> Result<Vec<u8>, String> {
    let configured_extern =
        ensure_configured_interface(resolve, config_interface, config_function)?;

    // A world importing the config interface (wit-parser elaborates its transitive
    // types/API imports for us) and exporting the configured entrypoint.
    let mut wit = String::from("package eo9-internal:configure@0.1.0;\n\nworld binder {\n");
    wit.push_str(&format!("    import {};\n", plan.config_extern));
    wit.push_str(&format!("    export {configured_extern};\n"));
    wit.push_str("}\n");
    let package = resolve
        .push_source("configure-binder.wit", &wit)
        .map_err(|err| format!("failed to resolve the binder world: {err:#}"))?;
    let world = resolve
        .select_world(&[package], Some("binder"))
        .map_err(|err| format!("failed to select the binder world: {err:#}"))?;

    let module = synthesize_binder_module(plan, &configured_extern);
    synth::encode_component(module, resolve, world)
}

/// Ensures `resolve` carries the `eo9:rt` package's `configured` interface (the bind
/// entrypoint every configured component exports -- see `wit/rt/rt.wit`), adding the
/// package or the interface when the provider's own decoded resolve lacks them, and
/// returns its extern name (e.g. `eo9:rt/configured@0.1.0`).
///
/// Every SDK-built provider already references `eo9:rt` (the diagnostics import), so the
/// usual case is adding one interface to an existing package. The interface and its
/// `bind` function are built by cloning the provider's own config interface/function as
/// skeletons -- that way no `Span`/`Docs`/`Stability` values need constructing by hand,
/// and the result is structurally identical to the canonical definition (function-only
/// interfaces have no nominal identity, so "same shape" is "same type").
fn ensure_configured_interface(
    resolve: &mut Resolve,
    config_interface: InterfaceId,
    config_function: &Function,
) -> Result<String, String> {
    const CONFIGURED: &str = "configured";

    let rt_pkg = resolve
        .package_names
        .iter()
        .find(|(name, _)| name.namespace == "eo9" && name.name == "rt")
        .map(|(_, id)| *id);
    let rt_pkg = match rt_pkg {
        Some(pkg) => pkg,
        None => {
            // The provider references no `eo9:rt` at all (hand-built fixtures); add the
            // canonical package fresh. Pushing text keeps wit-parser's own invariants.
            resolve
                .push_source(
                    "eo9-rt-configured.wit",
                    "package eo9:rt@0.1.0;\n\ninterface configured {\n    bind: func();\n}\n",
                )
                .map_err(|err| format!("failed to add the eo9:rt package: {err:#}"))?
        }
    };

    let version = resolve.packages[rt_pkg]
        .name
        .version
        .as_ref()
        .map(|v| format!("@{v}"))
        .unwrap_or_default();
    let extern_name = format!("{CONFIGURED_INTERFACE}{version}");

    if resolve.packages[rt_pkg].interfaces.contains_key(CONFIGURED) {
        return Ok(extern_name);
    }

    // The provider's resolve has eo9:rt (diagnostics) but not `configured`: add it
    // programmatically, cloning the config interface/function as field skeletons.
    let mut bind_function = config_function.clone();
    bind_function.name = BIND.to_string();
    bind_function.kind = FunctionKind::Freestanding;
    bind_function.params = Vec::new();
    bind_function.result = None;

    let mut configured_iface = resolve.interfaces[config_interface].clone();
    configured_iface.name = Some(CONFIGURED.to_string());
    configured_iface.types.clear();
    configured_iface.functions.clear();
    configured_iface
        .functions
        .insert(BIND.to_string(), bind_function);
    configured_iface.package = Some(rt_pkg);
    configured_iface.clone_of = None;

    let configured_id = resolve.interfaces.alloc(configured_iface);
    resolve.packages[rt_pkg]
        .interfaces
        .insert(CONFIGURED.to_string(), configured_id);
    Ok(extern_name)
}

/// The binder's memory layout: a fixed scratch area for indirect results starting at 16,
/// then the baked-in constant arena (strings, lists, spilled parameters), then the bump
/// heap.
struct Layout {
    scratch: u32,
    /// The absolute base address of the constant arena (where the data segment lands).
    arena_base: u32,
    /// The arena bytes with every arena-relative pointer rebased to its absolute address.
    arena: Vec<u8>,
    heap_base: u32,
}

fn layout(plan: &BinderPlan) -> Layout {
    let scratch = 16u32;
    let arena_base = scratch + plan.scratch_size;
    let mut arena = plan.constants.arena.clone();
    for &reloc in &plan.constants.relocs {
        let reloc = reloc as usize;
        let relative = u32::from_le_bytes([
            arena[reloc],
            arena[reloc + 1],
            arena[reloc + 2],
            arena[reloc + 3],
        ]);
        arena[reloc..reloc + 4].copy_from_slice(&(arena_base + relative).to_le_bytes());
    }
    let heap_base = (arena_base + arena.len() as u32).next_multiple_of(16);
    Layout {
        scratch,
        arena_base,
        arena,
        heap_base,
    }
}

/// Builds the binder's core module: the sync-lowered `configure` import, the baked-in
/// constant arena as a data segment, a bump allocator (`cabi_realloc`) for canonical-ABI
/// result lifting, and one exported function -- `bind` -- that calls `configure` with
/// the constants exactly once (later calls are no-ops).
fn synthesize_binder_module(plan: &BinderPlan, configured_extern: &str) -> Vec<u8> {
    let layout = layout(plan);

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    // Import: the sync-lowered `configure` (function index 0).
    let configure_type = synth::push_signature(&mut types, &plan.config_sig);
    imports.import(
        &plan.config_extern,
        CONFIGURE,
        wasm_encoder::EntityType::Function(configure_type),
    );
    let configure_func = 0u32;

    // Defined functions: cabi_realloc (index 1) -- the canonical ABI needs it to lift
    // `configure`'s result (the error string of a rejected value) into the binder --
    // and bind (index 2).
    let realloc_type = types.len();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    functions.function(realloc_type);
    code.function(&realloc_body());
    let realloc_func = 1u32;

    let bind_type = types.len();
    types.ty().function([], []);
    functions.function(bind_type);
    code.function(&bind_body(plan, &layout, configure_func));
    let bind_func = 2u32;

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    // Globals: 0 = bump pointer (cabi_realloc), 1 = "configure has run" flag.
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
    exports.export(
        &format!("{configured_extern}#{BIND}"),
        ExportKind::Func,
        bind_func,
    );

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&globals);
    module.section(&exports);
    module.section(&code);
    if !layout.arena.is_empty() {
        let mut data = DataSection::new();
        data.active(
            0,
            &ConstExpr::i32_const(layout.arena_base as i32),
            layout.arena.iter().copied(),
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

/// The `bind` entrypoint: call `configure` (sync-lowered) with the baked-in constants,
/// require that it returned success, and mark the configuration applied. Idempotent --
/// the first call wins, later calls return immediately. An error from `configure` (an
/// invalid baked value) traps, so a misconfigured composition fails before any of its
/// observable behavior runs. Because `configure` is synchronous it may itself reenter
/// another configured provider's `configure` (the executor binds providers before the
/// consumers layered on top of them -- see `compose.rs`).
fn bind_body(plan: &BinderPlan, layout: &Layout, configure_func: u32) -> wasm_encoder::Function {
    let mut f = wasm_encoder::Function::new([]);

    // Idempotence: if configuration has already been applied, do nothing.
    f.instruction(&Instruction::GlobalGet(1));
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    for constant in &plan.constants.flats {
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
            FlatConst::ArenaPtr(offset) => {
                f.instruction(&Instruction::I32Const((layout.arena_base + offset) as i32));
            }
        }
    }
    if plan.config_sig.retptr {
        f.instruction(&Instruction::I32Const(layout.scratch as i32));
    }
    f.instruction(&Instruction::Call(configure_func));

    // Sync-lowered call: any results small enough to be returned by value (rather than
    // through the retptr) arrive on the stack; drop them -- the binder only needs the
    // side effect of `configure` having bound the provider's state. All standard configs
    // return `result<x-impl, string>`, which is wide enough to use the retptr, so this
    // loop is empty for them and the discriminant is read from `scratch` below.
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
