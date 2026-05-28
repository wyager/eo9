//! `only` -- restriction to a fixed allow-list of interfaces (SPEC.md "The capability
//! algebra: optional, `none`, `deny`, and `only`").
//!
//! `restrict(c, allow)`:
//! 1. every *required* residual import of `c` whose interface is outside `allow` is a
//!    compose-time error naming the offenders;
//! 2. every *optional* residual import outside `allow` is sealed as absent, by
//!    synthesizing the trivial absent provider (observationally identical to composing
//!    that API's `X.none` stub) inline -- this keeps the algebra free of a store
//!    dependency;
//! 3. exports are untouched; the result's capability imports satisfy
//!    `imports(only w $ c) ⊆ w ∩ imports(c)`.
//!
//! Allow-list matching is by interface *type*, not slot name: an entry admits both the
//! required and `-optional` flavor of its interface, at any semver-compatible version at
//! or below the entry's (an absent entry version admits every version). An entry may name
//! a single interface (`eo9:text/text`) or a whole package (`eo9:text`); a package entry
//! admits every interface of that package the consumer imports. Types-only interfaces (no
//! functions -- e.g. the `eo9:*/types` interfaces that `use` drags in) carry no authority
//! and are always admitted.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, FunctionSection, Instruction,
    MemorySection, MemoryType, Module, TypeSection, ValType,
};
use wit_parser::abi::{AbiVariant, WasmSignature};
use wit_parser::decoding::{DecodedWasm, decode};
use wit_parser::{Resolve, WorldId, WorldItem};

use crate::compose::{encode, export_all, register, slot_annotations, world_exports};
use crate::describe::{ImportMeta, OPTIONAL_SUFFIX};
use crate::error::RestrictError;
use crate::{Component, InterfaceRef, semver, slots, synth};

/// Bounds `c` to the allow-list `allow`, per the rules above.
pub fn restrict(c: &Component, allow: &[InterfaceRef]) -> Result<Component, RestrictError> {
    validate_allow_list(allow)?;

    let mut offenders = Vec::new();
    let mut to_seal = Vec::new();
    for import in &c.meta().imports {
        if import.authority_free || admitted(import, allow) {
            continue;
        }
        if import.required {
            offenders.push(describe_offender(import));
        } else {
            to_seal.push(import.clone());
        }
    }
    if !offenders.is_empty() {
        return Err(RestrictError::RequiredOutsideAllowList(offenders));
    }
    if to_seal.is_empty() {
        return Ok(c.clone());
    }
    seal_optional_imports(c, &to_seal)
}

/// Checks that every allow-list entry is an interface name with a parseable version.
fn validate_allow_list(allow: &[InterfaceRef]) -> Result<(), RestrictError> {
    for entry in allow {
        let (name, inline_version) = slots::split_extern_name(&entry.interface);
        // An entry is either a full interface name (`namespace:package/interface`) or a
        // package shorthand (`namespace:package`); the version always rides in
        // `entry.version`, never inline.
        if !name.contains(':') || !inline_version.is_empty() {
            return Err(RestrictError::InvalidAllowList(format!(
                "`{}` is not an interface or package name (expected \
                 `namespace:package/interface` or `namespace:package`)",
                entry.interface
            )));
        }
        if let Some(version) = &entry.version
            && semver::Version::parse(version).is_none()
        {
            return Err(RestrictError::InvalidAllowList(format!(
                "`{version}` is not a semver version (allow-list entry `{name}`)"
            )));
        }
    }
    Ok(())
}

/// Whether an allow-list admits an import (by interface type and the semver rule). An
/// entry is a full interface name (exact, also admitting the import's `-optional` flavor)
/// or a package shorthand with no `/interface` (admits any interface of that package).
fn admitted(import: &ImportMeta, allow: &[InterfaceRef]) -> bool {
    allow.iter().any(|entry| {
        let interface_matches = if entry.interface.contains('/') {
            let base_matches = import.interface == entry.interface;
            let optional_matches = import
                .interface
                .strip_suffix(OPTIONAL_SUFFIX)
                .is_some_and(|base| base == entry.interface);
            base_matches || optional_matches
        } else {
            interface_package(&import.interface) == entry.interface
        };
        interface_matches
            && entry
                .version
                .as_deref()
                .is_none_or(|granted| semver::satisfies(granted, &import.version))
    })
}

/// The `namespace:package` portion of an interface name (`eo9:text/text` -> `eo9:text`),
/// used to match a package-shorthand allow-list entry.
fn interface_package(interface: &str) -> &str {
    interface.split('/').next().unwrap_or(interface)
}

/// The `slot (interface@version)` text used to name an offending import.
fn describe_offender(import: &ImportMeta) -> String {
    let mut interface = import.interface.clone();
    if interface.is_empty() {
        interface = "<inline interface>".to_string();
    }
    if !import.version.is_empty() {
        interface = format!("{interface}@{}", import.version);
    }
    if import.slot == import.interface {
        interface
    } else {
        format!("{} ({interface})", import.slot)
    }
}

/// Seals the given optional imports as absent by generating one provider component that
/// exports each of their interfaces with `default()` answering `none`, and wiring it in.
fn seal_optional_imports(
    c: &Component,
    to_seal: &[ImportMeta],
) -> Result<Component, RestrictError> {
    let internal = |msg: String| RestrictError::Internal(msg);

    let decoded = decode(c.bytes())
        .map_err(|err| internal(format!("failed to re-decode the component: {err:#}")))?;
    let (mut resolve, world) = match decoded {
        DecodedWasm::Component(resolve, world) => (resolve, world),
        DecodedWasm::WitPackage(..) => {
            return Err(internal("component decoded as a WIT package".to_string()));
        }
    };

    // The interfaces to seal, deduplicated (two slots of one optional interface are both
    // sealed by the same exported instance).
    let mut interfaces: Vec<String> = Vec::new();
    for import in to_seal {
        let id = versioned_interface_id(import);
        if !interfaces.contains(&id) {
            interfaces.push(id);
        }
    }

    let sealer_bytes = build_absent_provider(&mut resolve, world, &interfaces, to_seal)
        .map_err(|err| internal(format!("failed to synthesize the absent provider: {err}")))?;

    // Wire the absent provider into the sealed slots (and only those slots), then
    // re-export everything the component exports.
    let compose_err = |err: crate::ComposeError| internal(err.to_string());
    let mut graph = wac_graph::CompositionGraph::new();
    let sealer_pkg = register(&mut graph, "absent", &sealer_bytes).map_err(compose_err)?;
    let c_pkg = register(&mut graph, "restricted", c.bytes()).map_err(compose_err)?;
    let sealer_inst = graph.instantiate(sealer_pkg);
    let c_inst = graph.instantiate(c_pkg);

    let sealer_exports = world_exports(&graph, sealer_pkg);
    for import in to_seal {
        let export_name = sealer_exports
            .iter()
            .map(|(name, _)| name)
            .find(|name| name.as_str() == versioned_interface_id(import))
            .ok_or_else(|| {
                internal(format!(
                    "the synthesized provider does not export `{}`",
                    import.interface
                ))
            })?
            .clone();
        let alias = graph
            .alias_instance_export(sealer_inst, &export_name)
            .map_err(|err| internal(err.to_string()))?;
        graph
            .set_instantiation_argument(c_inst, &import.extern_name, alias)
            .map_err(|err| internal(err.to_string()))?;
    }
    export_all(&mut graph, c_pkg, c_inst, None).map_err(compose_err)?;
    encode(&graph, &slot_annotations(&[c])).map_err(compose_err)
}

/// The versioned interface id of an import (`eo9:net/net-optional@0.1.0`).
fn versioned_interface_id(import: &ImportMeta) -> String {
    if import.version.is_empty() {
        import.interface.clone()
    } else {
        format!("{}@{}", import.interface, import.version)
    }
}

/// Generates the "absent provider" component: it exports each interface in `interfaces`
/// with every function returning the all-zero flat representation -- for the mechanically
/// derived `-optional` flavors this is `default() -> none`, i.e. exactly the behavior of
/// that API's `X.none` stub.
fn build_absent_provider(
    resolve: &mut Resolve,
    consumer_world: WorldId,
    interfaces: &[String],
    to_seal: &[ImportMeta],
) -> Result<Vec<u8>, String> {
    // Only the mechanically derived `-optional` shape (functions returning `option<...>`)
    // can be sealed as absent; anything else has no meaningful "absent" behavior.
    for import in to_seal {
        check_sealable(resolve, consumer_world, import)?;
    }

    // A world exporting exactly the interfaces to seal. Pushing WIT text (rather than
    // hand-assembling a World) lets wit-parser elaborate the transitive `use`
    // dependencies into imports for us.
    let mut wit = String::from("package eo9-internal:absent@0.1.0;\n\nworld absent {\n");
    for interface in interfaces {
        wit.push_str(&format!("    export {interface};\n"));
    }
    wit.push_str("}\n");
    let package = resolve
        .push_source("absent-provider.wit", &wit)
        .map_err(|err| format!("failed to resolve the absent-provider world: {err:#}"))?;
    let world = resolve
        .select_world(&[package], Some("absent"))
        .map_err(|err| format!("failed to select the absent-provider world: {err:#}"))?;

    // The core module implementing it: one function per exported interface function,
    // returning zeroes (plus a zero-filled exported memory for indirect returns).
    let module = synthesize_zero_module(resolve, world)?;
    synth::encode_component(module, resolve, world)
}

/// Checks that an optional import has the mechanically derived `-optional` shape:
/// every function returns an `option<...>` (in practice, exactly `default`).
fn check_sealable(
    resolve: &Resolve,
    consumer_world: WorldId,
    import: &ImportMeta,
) -> Result<(), String> {
    let world = &resolve.worlds[consumer_world];
    let item = world
        .imports
        .iter()
        .find(|(key, _)| resolve.name_world_key(key) == import.extern_name)
        .map(|(_, item)| item)
        .ok_or_else(|| format!("import `{}` not found after decoding", import.extern_name))?;
    let WorldItem::Interface { id, .. } = item else {
        return Err(format!(
            "import `{}` is not an interface",
            import.extern_name
        ));
    };
    for (name, function) in &resolve.interfaces[*id].functions {
        let returns_option = matches!(
            function.result,
            Some(wit_parser::Type::Id(id))
                if matches!(resolve.types[id].kind, wit_parser::TypeDefKind::Option(_))
        );
        if !function.params.is_empty() || !returns_option {
            return Err(format!(
                "optional import `{}` has function `{name}` that is not a nullary \
                 option-returning accessor; cannot seal it as absent",
                import.extern_name
            ));
        }
    }
    Ok(())
}

/// Builds a core module exporting, for every function exported by `world`, a stub that
/// returns zeroes -- which lifts to `none` for option-returning accessors. A zero-filled
/// memory is exported for functions whose results are returned indirectly.
fn synthesize_zero_module(resolve: &Resolve, world: WorldId) -> Result<Vec<u8>, String> {
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    exports.export("memory", ExportKind::Memory, 0);

    let mut func_index = 0u32;
    let world = &resolve.worlds[world];
    for (key, item) in &world.exports {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let interface_name = resolve.name_world_key(key);
        for (func_name, function) in &resolve.interfaces[*id].functions {
            let signature = resolve.wasm_signature(AbiVariant::GuestExport, function);
            let type_index = synth::push_signature(&mut types, &signature);
            functions.function(type_index);
            exports.export(
                &format!("{interface_name}#{func_name}"),
                ExportKind::Func,
                func_index,
            );
            code.function(&zero_returning_body(&signature));
            func_index += 1;
        }
    }

    let mut module = Module::new();
    module.section(&types);
    module.section(&functions);
    module.section(&memories);
    module.section(&exports);
    module.section(&code);
    // Keep the (zero) contents of the return area explicit.
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), [0u8; 16]);
    module.section(&data);
    Ok(module.finish())
}

/// A function body returning a zero for every result (a zero pointer for indirect
/// returns points at zero-filled memory, which lifts to `none`).
fn zero_returning_body(signature: &WasmSignature) -> wasm_encoder::Function {
    let mut body = wasm_encoder::Function::new([]);
    for result in &signature.results {
        match synth::val_type(result) {
            ValType::I32 => body.instruction(&Instruction::I32Const(0)),
            ValType::I64 => body.instruction(&Instruction::I64Const(0)),
            ValType::F32 => body.instruction(&Instruction::F32Const(0.0f32.into())),
            ValType::F64 => body.instruction(&Instruction::F64Const(0.0f64.into())),
            other => unreachable!("unexpected core type {other:?} in a canonical ABI signature"),
        };
    }
    body.instruction(&Instruction::End);
    body
}
