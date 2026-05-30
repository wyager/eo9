//! Shared helpers for synthesizing small components from generated core modules.
//!
//! Two operations mint components of their own: `restrict` (the absent provider that
//! seals optional imports) and `configure` (the binder that bakes compose-time constants
//! into a provider). Both follow the same recipe -- build a tiny core module with
//! `wasm-encoder`, embed the component metadata for a world carved out of the operand's
//! own decoded `Resolve`, and wrap it with `wit-component` -- and share these helpers.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, FunctionSection, ImportSection, Instruction, Module,
    TypeSection, ValType,
};
use wit_parser::abi::{WasmSignature, WasmType};
use wit_parser::{Resolve, WorldId};

/// The function name of the configuration entrypoint (`eo9:rt/configured.bind`).
pub(crate) const BIND: &str = "bind";

/// The versioned extern name of the configuration entrypoint interface, as it appears
/// on a synthesized configured component's exports.
pub(crate) const CONFIGURED_EXTERN: &str = "eo9:rt/configured@0.1.0";

/// The core value type for a canonical-ABI wasm type (32-bit pointer flavor).
pub(crate) fn val_type(ty: &WasmType) -> ValType {
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

/// Adds the core type for a canonical-ABI signature and returns its index.
pub(crate) fn push_signature(types: &mut TypeSection, signature: &WasmSignature) -> u32 {
    let params: Vec<ValType> = signature.params.iter().map(val_type).collect();
    let results: Vec<ValType> = signature.results.iter().map(val_type).collect();
    let index = types.len();
    types.ty().function(params, results);
    index
}

/// Embeds the component metadata for `world` into the generated core module and encodes
/// it as a component.
pub(crate) fn encode_component(
    mut module: Vec<u8>,
    resolve: &Resolve,
    world: WorldId,
) -> Result<Vec<u8>, String> {
    wit_component::embed_component_metadata(
        &mut module,
        resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .map_err(|err| format!("failed to embed component metadata: {err:#}"))?;
    wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&module)
        .map_err(|err| format!("failed to encode the synthesized component: {err:#}"))?
        .encode()
        .map_err(|err| format!("failed to encode the synthesized component: {err:#}"))
}

/// Synthesizes the bind-merger component used by `$`/`&` when **both** operands carry a
/// configuration entrypoint: it imports the two entrypoints under the slot names `first`
/// and `second` (inline interfaces -- function-only interface types are structural, so
/// wac wires the operands' real `eo9:rt/configured` exports straight into them) and
/// exports one `eo9:rt/configured` whose `bind` calls `first.bind()` then
/// `second.bind()`. Composition order is what makes nested configuration sound: the
/// provider's configuration is applied before the consumer's, because a consumer-side
/// `configure` may call through the provider's API.
pub(crate) fn bind_merger() -> Result<Vec<u8>, String> {
    let mut resolve = Resolve::default();
    resolve
        .push_source(
            "eo9-rt-configured.wit",
            "package eo9:rt@0.1.0;\n\ninterface configured {\n    bind: func();\n}\n",
        )
        .map_err(|err| format!("failed to add the eo9:rt package: {err:#}"))?;
    let merger = resolve
        .push_source(
            "bind-merger.wit",
            "package eo9-internal:bind-merge@0.1.0;\n\n\
             world merger {\n\
                 import first: interface {\n        bind: func();\n    }\n\
                 import second: interface {\n        bind: func();\n    }\n\
                 export eo9:rt/configured@0.1.0;\n\
             }\n",
        )
        .map_err(|err| format!("failed to resolve the merger world: {err:#}"))?;
    let world = resolve
        .select_world(&[merger], Some("merger"))
        .map_err(|err| format!("failed to select the merger world: {err:#}"))?;

    // The core module: import both binds, export one that calls them in order.
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    let nullary = types.len();
    types.ty().function([], []);
    imports.import("first", BIND, wasm_encoder::EntityType::Function(nullary));
    imports.import("second", BIND, wasm_encoder::EntityType::Function(nullary));
    functions.function(nullary);
    let mut body = wasm_encoder::Function::new([]);
    body.instruction(&Instruction::Call(0));
    body.instruction(&Instruction::Call(1));
    body.instruction(&Instruction::End);
    code.function(&body);
    exports.export(&format!("{CONFIGURED_EXTERN}#{BIND}"), ExportKind::Func, 2);

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);
    encode_component(module.finish(), &resolve, world)
}
