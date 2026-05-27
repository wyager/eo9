//! Shared helpers for synthesizing small components from generated core modules.
//!
//! Two operations mint components of their own: `restrict` (the absent provider that
//! seals optional imports) and `configure` (the binder that bakes compose-time constants
//! into a provider). Both follow the same recipe -- build a tiny core module with
//! `wasm-encoder`, embed the component metadata for a world carved out of the operand's
//! own decoded `Resolve`, and wrap it with `wit-component` -- and share these helpers.

use alloc::string::String;
use alloc::vec::Vec;

use wasm_encoder::{TypeSection, ValType};
use wit_parser::abi::{WasmSignature, WasmType};
use wit_parser::{Resolve, WorldId};

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
