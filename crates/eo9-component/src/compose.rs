//! `$` (compose) and `&` (extend): wiring one component's exports into another's imports.
//!
//! Both operators are implemented on top of `wac-graph`: the operands are registered as
//! packages, instantiated, wired argument-by-argument (matched by slot name and the
//! semver rule), and the graph is encoded back to a component. Unsatisfied imports become
//! the imports of the result (wac merges identically-named residuals), which is exactly
//! the residual formula from SPEC.md "Composition and the `$` operator".

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use wac_graph::types::{ItemKind, Package};
use wac_graph::{CompositionGraph, EncodeOptions, InstantiationArgumentError, NodeId, PackageId};

use crate::describe::{CONFIG_SUFFIX, OPTIONAL_SUFFIX};
use crate::error::ComposeError;
use crate::{Component, ComponentKind, Wiring, externs, slots};

/// A non-fatal observation made while composing (SPEC.md "Composition and the `$`
/// operator": unmatched provider exports are dropped, and the shell is expected to warn
/// when a provider contributed nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeWarning {
    /// None of the provider's exports matched an import of the consumer: the entire
    /// provider is dropped from the composition (a dead layer). The slot names of the
    /// dropped exports are listed for the message.
    ProviderExportsUnused { exports: Vec<String> },
}

impl core::fmt::Display for ComposeWarning {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ProviderExportsUnused { exports } => write!(
                f,
                "the provider's exports ({}) match nothing the consumer imports; the layer has \
                 no effect and is dropped",
                exports.join(", ")
            ),
        }
    }
}

/// `$` -- composition: satisfy `consumer`'s imports from `provider`'s matching exports.
///
/// Per SPEC.md "Composition and the `$` operator":
/// * matching is by slot name (with the semver rule on versions); matched imports are
///   *sealed* -- they are not imports of the result;
/// * `imports(p $ c) = imports(p) ∪ (imports(c) ∖ exports(p))`;
/// * `exports(p $ c) = exports(c)` -- the provider's unconsumed exports are dropped, and
///   the result's kind is the consumer's kind (kind preservation / layering).
///
/// The left operand must be a provider; the right operand may be a binary or a provider.
pub fn compose(provider: &Component, consumer: &Component) -> Result<Component, ComposeError> {
    compose_checked(provider, consumer).map(|(component, _)| component)
}

/// [`compose`], also reporting non-fatal observations (see [`ComposeWarning`]) so a
/// shell or other front end can relay them to the user.
pub fn compose_checked(
    provider: &Component,
    consumer: &Component,
) -> Result<(Component, Vec<ComposeWarning>), ComposeError> {
    if provider.kind() != ComponentKind::Provider {
        return Err(ComposeError::NotAProvider);
    }

    let mut graph = CompositionGraph::new();
    let p_pkg = register(&mut graph, "provider", provider.bytes())?;
    let c_pkg = register(&mut graph, "consumer", consumer.bytes())?;
    let p_inst = graph.instantiate(p_pkg);
    let c_inst = graph.instantiate(c_pkg);

    let skips = split_identity_skips(provider, consumer);
    let sealed = wire_matching_imports(&mut graph, p_pkg, p_inst, c_pkg, c_inst, &skips)?;
    let mut warnings = Vec::new();
    if sealed == 0 {
        // Nothing the provider exports is consumed. If that is because the provider
        // only offers its configuration interface for an API the consumer actually
        // needs (the SPEC "export shape encodes whether configuration is required"
        // rule), refuse with the configure hint; otherwise it is the spec's
        // dead-layer case, which composes fine but deserves a warning.
        if let Some(slot) = unconfigured_provider_for(provider, consumer) {
            return Err(ComposeError::TypeMismatch(format!(
                "`{slot}` exports only its configuration interface for an API the consumer \
                 requires -- apply `configure(\u{2026})` to bind its arguments and produce a \
                 composable provider (an unconfigured provider has no API to compose)"
            )));
        }
        warnings.push(ComposeWarning::ProviderExportsUnused {
            exports: provider
                .meta()
                .exports
                .iter()
                .map(|e| e.slot.clone())
                .collect(),
        });
    }
    export_all(&mut graph, c_pkg, c_inst, None)?;

    let component = encode(&graph, &slot_annotations(&[provider, consumer]))?;
    let component = component.with_wiring(Wiring::Compose {
        provider: Box::new(provider.wiring().clone()),
        consumer: Box::new(consumer.wiring().clone()),
        satisfied: satisfied_interfaces(provider, consumer),
    });
    Ok((component, warnings))
}

/// The interfaces a provider's exports satisfy among a consumer's imports (by interface
/// type, matching the `-optional` flavor too) -- recorded in the compose wiring so a tree
/// shows what an interposed layer actually contributes.
fn satisfied_interfaces(provider: &Component, consumer: &Component) -> Vec<String> {
    let mut satisfied = Vec::new();
    for export in &provider.meta().exports {
        if export.interface.is_empty() {
            continue;
        }
        let matched = consumer.meta().imports.iter().any(|import| {
            import.interface == export.interface
                || import.interface.strip_suffix(OPTIONAL_SUFFIX) == Some(export.interface.as_str())
        });
        if matched && !satisfied.contains(&export.interface) {
            satisfied.push(export.interface.clone());
        }
    }
    satisfied
}

/// If the provider's only relevant offer for something the consumer *requires* is a
/// `*-config` interface (it exports `X-config` but not `X`, and the consumer requires
/// `X`), the slot name of that required API -- the "needs `configure`" situation.
fn unconfigured_provider_for(provider: &Component, consumer: &Component) -> Option<String> {
    let provider_exports = &provider.meta().exports;
    for import in &consumer.meta().imports {
        if !import.required || import.interface.is_empty() {
            continue;
        }
        let exports_api = provider_exports
            .iter()
            .any(|e| e.interface == import.interface);
        let config_interface = format!("{}{}", import.interface, CONFIG_SUFFIX);
        let exports_config = provider_exports
            .iter()
            .any(|e| e.interface == config_interface);
        if exports_config && !exports_api {
            return Some(import.slot.clone());
        }
    }
    None
}

/// `&` -- environment extension: `base` extended and, where they overlap, overridden by
/// `layer`.
///
/// Per SPEC.md "Environments and the `&` operator":
/// * every import of `layer` matched by an export of `base` is satisfied by `base` (and
///   sealed, exactly as with `$`);
/// * `exports(x & y) = exports(y) ∪ (exports(x) ∖ exports(y))` -- the right-biased union;
/// * `imports(x & y) = imports(x) ∪ (imports(y) ∖ exports(x))`.
///
/// Both operands must be providers (binaries do not participate in `&`), and the result
/// is a provider.
pub fn extend(base: &Component, layer: &Component) -> Result<Component, ComposeError> {
    if base.kind() != ComponentKind::Provider || layer.kind() != ComponentKind::Provider {
        return Err(ComposeError::NotAProvider);
    }

    let mut graph = CompositionGraph::new();
    let x_pkg = register(&mut graph, "base", base.bytes())?;
    let y_pkg = register(&mut graph, "layer", layer.bytes())?;
    let x_inst = graph.instantiate(x_pkg);
    let y_inst = graph.instantiate(y_pkg);

    wire_matching_imports(
        &mut graph,
        x_pkg,
        x_inst,
        y_pkg,
        y_inst,
        &split_identity_skips(base, layer),
    )?;

    // Exports: everything from the layer, plus whatever the base exports that the layer
    // does not shadow (shadowing is keyed by slot name).
    export_all(&mut graph, y_pkg, y_inst, None)?;
    let shadowed: Vec<String> = world_exports(&graph, y_pkg)
        .into_iter()
        .map(|(name, _)| slots::slot_name(&name).to_string())
        .collect();
    export_all(&mut graph, x_pkg, x_inst, Some(&shadowed))?;

    // Shadowing is only meaningful where the base also exported that slot; report just the
    // base exports the layer actually overrode.
    let base_slots: Vec<String> = base.meta().exports.iter().map(|e| e.slot.clone()).collect();
    let overridden: Vec<String> = shadowed
        .into_iter()
        .filter(|slot| base_slots.contains(slot))
        .collect();
    let component = encode(&graph, &slot_annotations(&[base, layer]))?;
    Ok(component.with_wiring(Wiring::Extend {
        base: Box::new(base.wiring().clone()),
        layer: Box::new(layer.wiring().clone()),
        shadowed: overridden,
    }))
}

/// Registers component bytes with the graph under `name`.
///
/// The `implements` name annotations (see [`externs`]) are stripped first: the wiring
/// machinery predates that encoding, and the annotations are re-attached to the
/// composition's own externs by [`encode`].
pub(crate) fn register(
    graph: &mut CompositionGraph,
    name: &str,
    bytes: &[u8],
) -> Result<PackageId, ComposeError> {
    let stripped = externs::strip_implements(bytes)
        .map_err(|err| ComposeError::Internal(format!("failed to prepare `{name}`: {err}")))?;
    let package = Package::from_bytes(name, None, stripped, graph.types_mut())
        .map_err(|err| ComposeError::Internal(format!("failed to register `{name}`: {err:#}")))?;
    graph
        .register_package(package)
        .map_err(|err| ComposeError::Internal(format!("failed to register `{name}`: {err}")))
}

/// The `implements` annotations carried by the operands' plain-named slots
/// (extern name -> versioned interface id), so [`encode`] can restore them on the
/// composition's residual imports and re-exported exports.
pub(crate) fn slot_annotations(operands: &[&Component]) -> BTreeMap<String, String> {
    let mut annotations = BTreeMap::new();
    let mut conflicting = Vec::new();
    let mut record = |extern_name: &str, interface: &str, version: &str| {
        if slots::is_interface_style(extern_name) || interface.is_empty() {
            return;
        }
        let id = if version.is_empty() {
            interface.to_string()
        } else {
            format!("{interface}@{version}")
        };
        match annotations.get(extern_name) {
            Some(existing) if *existing != id => conflicting.push(extern_name.to_string()),
            _ => {
                annotations.insert(extern_name.to_string(), id);
            }
        }
    };
    for component in operands {
        for import in &component.meta().imports {
            record(&import.extern_name, &import.interface, &import.version);
        }
        for export in &component.meta().exports {
            record(&export.extern_name, &export.interface, &export.version);
        }
    }
    for name in conflicting {
        annotations.remove(&name);
    }
    annotations
}

/// The (extern name, item kind) pairs of a registered package's imports.
pub(crate) fn world_imports(graph: &CompositionGraph, pkg: PackageId) -> Vec<(String, ItemKind)> {
    graph.types()[graph[pkg].ty()]
        .imports
        .iter()
        .map(|(name, kind)| (name.clone(), *kind))
        .collect()
}

/// The (extern name, item kind) pairs of a registered package's exports.
pub(crate) fn world_exports(graph: &CompositionGraph, pkg: PackageId) -> Vec<(String, ItemKind)> {
    graph.types()[graph[pkg].ty()]
        .exports
        .iter()
        .map(|(name, kind)| (name.clone(), *kind))
        .collect()
}

/// The extern names of `consumer` imports that must NOT be wired from `provider`: a
/// types-only (authority-free) import whose package's authority interface the consumer
/// also imports but `provider` does not export. Wiring just the types in that situation
/// splits the package's nominal resource identity between two implementers -- the
/// authority interface keeps expecting the types of whoever eventually provides it --
/// and the encoded composition fails validation (the `X.none $ consumer` shape from the
/// PL user study). Leaving the types import residual keeps the drop law intact: the
/// provider contributes nothing, and whoever provides the authority later brings its
/// own types instance.
fn split_identity_skips(provider: &Component, consumer: &Component) -> Vec<String> {
    let provider_exports = &provider.meta().exports;
    let imports = &consumer.meta().imports;
    let package_of = |interface: &str| interface.split('/').next().unwrap_or("").to_string();

    let mut skips = Vec::new();
    for import in imports {
        if !import.authority_free || import.interface.is_empty() {
            continue;
        }
        let package = package_of(&import.interface);
        let unsatisfied_authority = imports.iter().any(|other| {
            !other.authority_free
                && package_of(&other.interface) == package
                && !provider_exports
                    .iter()
                    .any(|e| e.interface == other.interface)
        });
        if unsatisfied_authority {
            skips.push(import.extern_name.clone());
        }
    }
    skips
}

/// Wires every interface import of `to` that is matched -- by slot name and the semver
/// rule -- by an interface export of `from`, except those listed (by extern name) in
/// `skip`. Returns how many imports were sealed.
pub(crate) fn wire_matching_imports(
    graph: &mut CompositionGraph,
    from_pkg: PackageId,
    from_inst: NodeId,
    to_pkg: PackageId,
    to_inst: NodeId,
    skip: &[String],
) -> Result<usize, ComposeError> {
    let exports = world_exports(graph, from_pkg);
    let imports = world_imports(graph, to_pkg);

    let mut sealed = 0;
    for (import_name, import_kind) in &imports {
        // Capability slots are interfaces; other import kinds are never wired here.
        if !matches!(import_kind, ItemKind::Instance(_)) {
            continue;
        }
        if skip.iter().any(|name| name == import_name) {
            continue;
        }
        let matched = exports.iter().find(|(export_name, export_kind)| {
            matches!(export_kind, ItemKind::Instance(_))
                && slots::export_matches_import(export_name, import_name)
        });
        let Some((export_name, _)) = matched else {
            continue;
        };

        let alias = graph
            .alias_instance_export(from_inst, export_name)
            .map_err(|err| ComposeError::Internal(err.to_string()))?;
        graph
            .set_instantiation_argument(to_inst, import_name, alias)
            .map_err(|err| match err {
                InstantiationArgumentError::ArgumentTypeMismatch { name, source } => {
                    ComposeError::TypeMismatch(format!("slot `{name}`: {source:#}"))
                }
                other => ComposeError::Internal(other.to_string()),
            })?;
        sealed += 1;
    }
    Ok(sealed)
}

/// Re-exports every export of `pkg`'s instantiation under its original name, skipping
/// exports whose slot name appears in `skip_slots` (used by `extend` for shadowing).
pub(crate) fn export_all(
    graph: &mut CompositionGraph,
    pkg: PackageId,
    inst: NodeId,
    skip_slots: Option<&[String]>,
) -> Result<(), ComposeError> {
    for (name, _) in world_exports(graph, pkg) {
        if let Some(skip) = skip_slots
            && skip.iter().any(|slot| slot == slots::slot_name(&name))
        {
            continue;
        }
        let alias = graph
            .alias_instance_export(inst, &name)
            .map_err(|err| ComposeError::Internal(err.to_string()))?;
        graph
            .export(alias, &name)
            .map_err(|err| ComposeError::Internal(err.to_string()))?;
    }
    Ok(())
}

/// Encodes the graph, restores the operands' slot annotations, and re-loads the result
/// as a validated `Component`.
///
/// Determinism note: the graph is constructed in a fixed order from its operands' own
/// import/export order and encoded without any processor/producer metadata, so the same
/// operands always produce byte-identical results.
pub(crate) fn encode(
    graph: &CompositionGraph,
    annotations: &BTreeMap<String, String>,
) -> Result<Component, ComposeError> {
    let bytes = graph
        .encode(EncodeOptions {
            define_components: true,
            validate: true,
            processor: None,
        })
        .map_err(|err| ComposeError::Internal(format!("{err:#}")))?;
    let bytes = externs::attach_implements(&bytes, annotations)
        .map_err(|err| ComposeError::Internal(format!("failed to restore slot names: {err}")))?;
    Component::load(bytes)
        .map_err(|err| ComposeError::Internal(format!("composition produced {err}")))
}
