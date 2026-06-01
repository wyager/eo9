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
    BlockType, CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
    GlobalType, ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module, TypeSection,
    ValType,
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

/// The WIT signature of the configuration entrypoint, as inline interface text.
const BIND_FUNC_WIT: &str = "bind: func() -> result<_, string>;";

/// Synthesizes the bind-merger component used by `$`/`&` when **both** operands carry a
/// configuration entrypoint: it imports the two entrypoints under the slot names `first`
/// and `second` (inline interfaces -- function-only interface types are structural, so
/// wac wires the operands' real `eo9:rt/configured` exports straight into them) and
/// exports one `eo9:rt/configured` whose `bind` calls `first.bind()` then
/// `second.bind()`, propagating the **first** error: a nested configuration the provider
/// rejects surfaces as the merged entrypoint's own typed error, and the second operand
/// is then not bound at all (its configuration is unreachable behind the failure).
/// Composition order is what makes nested configuration sound: the provider's
/// configuration is applied before the consumer's, because a consumer-side `configure`
/// may call through the provider's API.
pub(crate) fn bind_merger() -> Result<Vec<u8>, String> {
    let mut resolve = Resolve::default();
    resolve
        .push_source(
            "eo9-rt-configured.wit",
            &format!("package eo9:rt@0.1.0;\n\ninterface configured {{\n    {BIND_FUNC_WIT}\n}}\n"),
        )
        .map_err(|err| format!("failed to add the eo9:rt package: {err:#}"))?;
    let merger = resolve
        .push_source(
            "bind-merger.wit",
            &format!(
                "package eo9-internal:bind-merge@0.1.0;\n\n\
                 world merger {{\n\
                     import first: interface {{\n        {BIND_FUNC_WIT}\n    }}\n\
                     import second: interface {{\n        {BIND_FUNC_WIT}\n    }}\n\
                     export eo9:rt/configured@0.1.0;\n\
                 }}\n"
            ),
        )
        .map_err(|err| format!("failed to resolve the merger world: {err:#}"))?;
    let world = resolve
        .select_world(&[merger], Some("merger"))
        .map_err(|err| format!("failed to select the merger world: {err:#}"))?;

    // The core module: import both binds (lowered: each takes a retptr into this
    // module's memory and writes its `result<_, string>` there), export one bind that
    // calls them in order and returns the first error -- or the second call's result.
    //
    // Memory layout: scratch result area at 16 (12 bytes used), bump heap from 32 (the
    // canonical ABI lifts error strings into this module via `cabi_realloc`).
    const SCRATCH: i32 = 16;
    const HEAP_BASE: i32 = 32;

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    // Type 0: the lowered import signature `[retptr: i32] -> []`.
    let lowered = types.len();
    types.ty().function([ValType::I32], []);
    imports.import("first", BIND, wasm_encoder::EntityType::Function(lowered));
    imports.import("second", BIND, wasm_encoder::EntityType::Function(lowered));
    let (first_func, second_func) = (0u32, 1u32);

    // Type 1: cabi_realloc `[i32, i32, i32, i32] -> [i32]`.
    let realloc_type = types.len();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    functions.function(realloc_type);
    code.function(&realloc_body());
    let realloc_func = 2u32;

    // Type 2: the lifted export signature `[] -> [result ptr: i32]`.
    let lifted = types.len();
    types.ty().function([], [ValType::I32]);
    functions.function(lifted);
    let mut body = wasm_encoder::Function::new([]);
    // first.bind(scratch); if error (scratch[0] != 0) return scratch.
    body.instruction(&Instruction::I32Const(SCRATCH));
    body.instruction(&Instruction::Call(first_func));
    body.instruction(&Instruction::I32Const(SCRATCH));
    body.instruction(&Instruction::I32Load8U(MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }));
    body.instruction(&Instruction::If(BlockType::Empty));
    body.instruction(&Instruction::I32Const(SCRATCH));
    body.instruction(&Instruction::Return);
    body.instruction(&Instruction::End);
    // second.bind(scratch); return scratch (success or second's own error).
    body.instruction(&Instruction::I32Const(SCRATCH));
    body.instruction(&Instruction::Call(second_func));
    body.instruction(&Instruction::I32Const(SCRATCH));
    body.instruction(&Instruction::End);
    code.function(&body);
    let bind_func = 3u32;

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    // Global 0: the bump pointer for cabi_realloc.
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(HEAP_BASE),
    );

    exports.export("memory", ExportKind::Memory, 0);
    exports.export("cabi_realloc", ExportKind::Func, realloc_func);
    exports.export(
        &format!("{CONFIGURED_EXTERN}#{BIND}"),
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
    encode_component(module.finish(), &resolve, world)
}

/// A `cabi_realloc` body: a bump allocator over memory 0 (grown on demand), with the
/// bump pointer in global 0. Used by the configure binder and the bind merger -- both
/// need the canonical ABI to be able to lift strings (configure's error text) into
/// their memories. Allocations are never freed; these modules live for one component
/// instantiation and a handful of small allocations.
///
/// Locals: 0 old_ptr, 1 old_size, 2 align, 3 new_size, 4 ptr (scratch).
pub(crate) fn realloc_body() -> wasm_encoder::Function {
    let mut f = wasm_encoder::Function::new([(1, ValType::I32)]);
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
