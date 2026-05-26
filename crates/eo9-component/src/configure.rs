//! `configure` -- binding a provider's compose-time configuration constants
//! (SPEC.md "Binary or provider, never both": `configure : provider × args → provider`).
//!
//! A configurable provider ships a small exported `*-config` interface whose `configure`
//! entry binds the configuration and returns the provider's root capability handle.
//! `configure(provider, args)` bakes the given constants in:
//!
//! * the WAVE-encoded `args` are type-checked against `configure`'s declared parameters
//!   and lowered to canonical-ABI constants;
//! * a small *binder* component is synthesized that imports the config interface and
//!   calls `configure` with those constants exactly once, at instantiation time -- before
//!   any API export of the provider can be used -- trapping if `configure` reports an
//!   invalid value;
//! * provider and binder are wired together and the provider's API exports (and types)
//!   are re-exported, while the config interface is sealed away -- the consumer can
//!   neither observe nor re-run the configuration.
//!
//! The result is an ordinary provider: composable, sealable, and byte-deterministic for
//! the same operands. Like the rest of the algebra this is structure, not execution --
//! the configured behavior itself is exercised by the integration suite (area 13).

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, FunctionSection,
    GlobalSection, GlobalType, ImportSection, Instruction, MemArg, MemorySection, MemoryType,
    Module, StartSection, TypeSection, ValType,
};
use wasm_wave::value::{self, Value};
use wasm_wave::wasm::WasmValue;
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::decoding::{DecodedWasm, decode};
use wit_parser::{Function, Resolve, Type, TypeDefKind, WorldItem};

use crate::compose::{
    encode as encode_graph, export_all, register, slot_annotations, wire_matching_imports,
};
use crate::describe::{CONFIG_SUFFIX, CONFIGURE};
use crate::error::ConfigureError;
use crate::{Component, ComponentKind, synth};

/// Where the binder stores `configure`'s (indirectly returned) result.
const RET_AREA: u32 = 0;
/// Where the binder's baked-in string constants start.
const STRING_BASE: u32 = 64;

/// One canonical-ABI constant baked into the binder's call to `configure`.
enum FlatConst {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// A string constant: stored in the binder's data segment, passed as (ptr, len).
    Str(String),
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
    let config_interface = resolve.worlds[world]
        .exports
        .iter()
        .find(|(key, _)| resolve.name_world_key(key) == config_export.extern_name)
        .and_then(|(_, item)| match item {
            WorldItem::Interface { id, .. } => Some(*id),
            _ => None,
        })
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

    // Synthesize the binder and wire it in front of the provider.
    let binder = build_binder(
        &mut resolve,
        &config_export.extern_name,
        &function,
        &constants,
    )
    .map_err(internal)?;

    let compose_err =
        |err: crate::ComposeError| internal(format!("failed to assemble the wrapper: {err}"));
    let mut graph = wac_graph::CompositionGraph::new();
    let provider_pkg = register(&mut graph, "provider", provider.bytes()).map_err(compose_err)?;
    let binder_pkg = register(&mut graph, "binder", &binder).map_err(compose_err)?;
    let provider_inst = graph.instantiate(provider_pkg);
    let binder_inst = graph.instantiate(binder_pkg);

    // The binder's config (and types) imports are satisfied by the provider itself.
    wire_matching_imports(
        &mut graph,
        provider_pkg,
        provider_inst,
        binder_pkg,
        binder_inst,
    )
    .map_err(compose_err)?;
    // Re-export everything except the config interface, which is now bound and sealed.
    export_all(
        &mut graph,
        provider_pkg,
        provider_inst,
        Some(std::slice::from_ref(&config_export.slot)),
    )
    .map_err(compose_err)?;

    encode_graph(&graph, &slot_annotations(&[provider])).map_err(compose_err)
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

/// Synthesizes the binder component: it imports the provider's config interface and
/// calls `configure` with the baked-in constants from its start function (i.e. exactly
/// once, at instantiation, before any export of the wrapper can be called), trapping if
/// `configure` reports an error.
fn build_binder(
    resolve: &mut Resolve,
    config_extern_name: &str,
    function: &Function,
    constants: &[FlatConst],
) -> Result<Vec<u8>, String> {
    // A world importing exactly the config interface; wit-parser elaborates the
    // transitive types import for us.
    let wit = format!(
        "package eo9-internal:configure@0.1.0;\n\nworld binder {{\n    import {config_extern_name};\n}}\n"
    );
    let package = resolve
        .push_source("configure-binder.wit", &wit)
        .map_err(|err| format!("failed to resolve the binder world: {err:#}"))?;
    let world = resolve
        .select_world(&[package], Some("binder"))
        .map_err(|err| format!("failed to select the binder world: {err:#}"))?;

    let signature = resolve.wasm_signature(AbiVariant::GuestImport, function);
    let module = synthesize_binder_module(config_extern_name, &signature, constants);
    synth::encode_component(module, resolve, world)
}

/// Builds the binder's core module: one imported function (`configure`, lowered
/// synchronously), a bump allocator for result lifting, and a start function that makes
/// the call with the baked-in constants and traps on a configuration error.
fn synthesize_binder_module(
    config_extern_name: &str,
    signature: &WasmSignature,
    constants: &[FlatConst],
) -> Vec<u8> {
    // Lay out the baked-in string constants in the data segment.
    let mut string_data = Vec::new();
    let mut string_offsets = Vec::new();
    for constant in constants {
        if let FlatConst::Str(text) = constant {
            string_offsets.push(STRING_BASE + string_data.len() as u32);
            string_data.extend_from_slice(text.as_bytes());
        }
    }
    let heap_base = (STRING_BASE + string_data.len() as u32 + 15) & !15;

    let mut types = TypeSection::new();
    let configure_type = synth::push_signature(&mut types, signature);
    let realloc_type = types.len();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    let start_type = types.len();
    types.ty().function([], []);

    let mut imports = ImportSection::new();
    imports.import(
        config_extern_name,
        CONFIGURE,
        wasm_encoder::EntityType::Function(configure_type),
    );
    let configure_func = 0u32;
    let realloc_func = 1u32;
    let start_func = 2u32;

    let mut functions = FunctionSection::new();
    functions.function(realloc_type);
    functions.function(start_type);

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(heap_base as i32),
    );

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("cabi_realloc", ExportKind::Func, realloc_func);

    let mut code = CodeSection::new();
    code.function(&realloc_body());
    code.function(&start_body(
        signature,
        constants,
        &string_offsets,
        configure_func,
    ));

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&globals);
    module.section(&exports);
    module.section(&StartSection {
        function_index: start_func,
    });
    module.section(&code);
    if !string_data.is_empty() {
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(STRING_BASE as i32), string_data);
        module.section(&data);
    }
    module.finish()
}

/// The binder's `cabi_realloc`: a bump allocator over the exported memory (grown on
/// demand) used by the canonical ABI to lift `configure`'s error string, if any. Old
/// allocations are never revisited -- the binder makes a single call.
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

/// The binder's start function: call `configure` with the baked-in constants and trap if
/// it reports an error (so an invalid configuration fails before the consumer ever runs).
fn start_body(
    signature: &WasmSignature,
    constants: &[FlatConst],
    string_offsets: &[u32],
    configure_func: u32,
) -> wasm_encoder::Function {
    let mut f = wasm_encoder::Function::new([]);
    let mut next_string = 0usize;
    for constant in constants {
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
                f.instruction(&Instruction::I32Const(string_offsets[next_string] as i32));
                f.instruction(&Instruction::I32Const(text.len() as i32));
                next_string += 1;
            }
        }
    }
    if signature.retptr {
        f.instruction(&Instruction::I32Const(RET_AREA as i32));
    }
    f.instruction(&Instruction::Call(configure_func));

    if signature.retptr {
        // The result landed in memory; its first byte is the `result` discriminant.
        f.instruction(&Instruction::I32Const(RET_AREA as i32));
        f.instruction(&Instruction::I32Load8U(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Unreachable);
        f.instruction(&Instruction::End);
    } else if signature.results.as_slice() == [wit_parser::abi::WasmType::I32] {
        // A single directly-returned i32 is the `result` discriminant.
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Unreachable);
        f.instruction(&Instruction::End);
    } else {
        for _ in &signature.results {
            f.instruction(&Instruction::Drop);
        }
    }
    f.instruction(&Instruction::End);
    f
}
