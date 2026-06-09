//! Law and behavior tests for the component algebra.
//!
//! The spec's algebraic laws (SPEC.md "Composition and the `$` operator",
//! "Environments and the `&` operator", "The capability algebra") are encoded here as
//! tests over small fixture components built in-process from WIT text (no dependency on
//! the guest SDK area). "≡" is observational equality on `describe()`, with imports and
//! exports compared as sets of slots.

mod fixtures;

use std::collections::BTreeSet;

use eo9_component::{
    Component, ComponentInfo, ComponentKind, ComposeError, ConfigureError, InterfaceRef, LoadError,
    RenameError, RestrictError, compose, configure, extend, rename, restrict,
};
use fixtures::{
    clock_user, eo9_fixture, frozen_provider, kit, kit_bytes, kit_mismatched, memfs_provider,
    seeded_provider, ver_consumer, ver_provider,
};

/// The normalized form of `describe()`: kind, sorted import slots, sorted export slots,
/// and the argument signature in declaration order.
type Normalized = (
    ComponentKind,
    Vec<String>,
    Vec<String>,
    Vec<(String, String)>,
);

/// `describe()` normalized for observational comparison: imports and exports as sorted
/// slot tuples (order is not meaningful), args in declaration order.
fn normalized(c: &Component) -> Normalized {
    let info = c.describe();
    let mut imports: Vec<String> = info
        .imports
        .iter()
        .map(|i| {
            format!(
                "{}|{}|{}|{}",
                i.slot,
                i.interface,
                i.version,
                if i.required { "required" } else { "optional" }
            )
        })
        .collect();
    imports.sort();
    let mut exports: Vec<String> = info
        .exports
        .iter()
        .map(|e| format!("{}|{}|{}", e.name, e.interface, e.version))
        .collect();
    exports.sort();
    let args = info
        .args
        .iter()
        .map(|a| (a.name.clone(), a.ty.clone()))
        .collect();
    (info.kind, imports, exports, args)
}

/// Asserts observational equality of two components (same describe, up to slot order).
fn assert_equivalent(a: &Component, b: &Component) {
    assert_eq!(normalized(a), normalized(b));
}

fn import_slots(info: &ComponentInfo) -> BTreeSet<String> {
    info.imports.iter().map(|i| i.slot.clone()).collect()
}

fn export_slots(info: &ComponentInfo) -> BTreeSet<String> {
    info.exports.iter().map(|e| e.name.clone()).collect()
}

// ---------------------------------------------------------------------------
// load / save / describe (milestone 1)
// ---------------------------------------------------------------------------

#[test]
fn load_classifies_a_binary_and_extracts_main_args() {
    let app = kit("app");
    let info = app.describe();
    assert_eq!(info.kind, ComponentKind::Binary);
    assert_eq!(app.kind(), ComponentKind::Binary);
    assert!(info.exports.is_empty());
    let args: Vec<(&str, &str)> = info
        .args
        .iter()
        .map(|a| (a.name.as_str(), a.ty.as_str()))
        .collect();
    assert_eq!(args, vec![("input", "string"), ("count", "u32")]);

    let cap_a = info
        .imports
        .iter()
        .find(|i| i.slot == "fix:kit/cap-a")
        .expect("cap-a import");
    assert_eq!(cap_a.interface, "fix:kit/cap-a");
    assert_eq!(cap_a.version, "1.0.0");
    assert!(cap_a.required);
    assert!(info.imports.iter().any(|i| i.slot == "fix:kit/cap-b"));
}

#[test]
fn load_classifies_a_provider_and_extracts_configure_args() {
    let provider = kit("provider-a");
    let info = provider.describe();
    assert_eq!(info.kind, ComponentKind::Provider);
    assert!(info.imports.iter().all(|i| i.slot != "fix:kit/cap-a"));
    let export = info
        .exports
        .iter()
        .find(|e| e.name == "fix:kit/cap-a")
        .expect("cap-a export");
    assert_eq!(export.interface, "fix:kit/cap-a");
    assert_eq!(export.version, "1.0.0");
    let args: Vec<(&str, &str)> = info
        .args
        .iter()
        .map(|a| (a.name.as_str(), a.ty.as_str()))
        .collect();
    assert_eq!(args, vec![("seed", "u64")]);
}

#[test]
fn the_empty_component_is_a_provider() {
    let empty = kit("empty");
    let info = empty.describe();
    assert_eq!(info.kind, ComponentKind::Provider);
    assert!(info.imports.is_empty());
    assert!(info.exports.is_empty());
    assert!(info.args.is_empty());
}

#[test]
fn optional_imports_are_visible_in_the_import_list() {
    let app = kit("app-optional");
    let info = app.describe();
    let optional = info
        .imports
        .iter()
        .find(|i| i.slot == "fix:kit/cap-a-optional")
        .expect("cap-a-optional import");
    assert!(!optional.required);
    let required = info
        .imports
        .iter()
        .find(|i| i.slot == "fix:kit/cap-b")
        .expect("cap-b import");
    assert!(required.required);
}

#[test]
fn named_slots_are_reported_by_slot_name() {
    let tool = kit("named-slots");
    let info = tool.describe();
    let left = info
        .imports
        .iter()
        .find(|i| i.slot == "left")
        .expect("left slot");
    assert_eq!(left.interface, "fix:kit/cap-a");
    assert_eq!(left.version, "1.0.0");
    let right = info
        .imports
        .iter()
        .find(|i| i.slot == "right")
        .expect("right slot");
    assert_eq!(right.interface, "fix:kit/cap-a");
}

#[test]
fn load_rejects_garbage_and_core_modules() {
    assert!(matches!(
        Component::load(b"definitely not wasm".to_vec()),
        Err(LoadError::InvalidComponent(_))
    ));
    // A valid (empty) core module is not a component.
    let empty_core_module = b"\0asm\x01\0\0\0".to_vec();
    assert!(matches!(
        Component::load(empty_core_module),
        Err(LoadError::InvalidComponent(_))
    ));
}

#[test]
fn load_rejects_modules_that_are_both_binary_and_provider() {
    let err = Component::load(kit_bytes("both-kinds")).unwrap_err();
    assert!(matches!(err, LoadError::NotAnEo9Module(_)), "{err}");
}

#[test]
fn load_rejects_unexpected_function_exports() {
    let err = Component::load(kit_bytes("odd-exports")).unwrap_err();
    assert!(matches!(err, LoadError::NotAnEo9Module(_)), "{err}");
}

#[test]
fn save_round_trips_byte_for_byte() {
    let app = kit("app");
    let saved = app.save();
    let reloaded = Component::load(saved.clone()).unwrap();
    assert_eq!(reloaded.save(), saved);
    assert_equivalent(&app, &reloaded);
}

#[test]
fn describes_components_against_the_real_eo9_wit() {
    let hello = eo9_fixture("hello");
    let info = hello.describe();
    assert_eq!(info.kind, ComponentKind::Binary);
    let text = info
        .imports
        .iter()
        .find(|i| i.slot == "eo9:text/text")
        .expect("text import");
    assert!(text.required);
    assert_eq!(text.version, "0.1.0");
    let entropy = info
        .imports
        .iter()
        .find(|i| i.slot == "eo9:entropy/entropy-optional")
        .expect("entropy-optional import");
    assert!(!entropy.required);
    let args: Vec<(&str, &str)> = info
        .args
        .iter()
        .map(|a| (a.name.as_str(), a.ty.as_str()))
        .collect();
    assert_eq!(args, vec![("greeting", "string")]);

    let mock = eo9_fixture("text-mock");
    assert_eq!(mock.kind(), ComponentKind::Provider);
    assert!(export_slots(&mock.describe()).contains("eo9:text/text"));
}

// ---------------------------------------------------------------------------
// compose ($) -- milestone 2
// ---------------------------------------------------------------------------

#[test]
fn compose_seals_matched_imports() {
    let result = compose(&kit("provider-a"), &kit("app")).unwrap();
    let info = result.describe();
    // Sealing: the matched import is gone and cannot be re-satisfied from outside.
    assert!(!import_slots(&info).contains("fix:kit/cap-a"));
    // Unmatched imports remain residuals.
    assert!(import_slots(&info).contains("fix:kit/cap-b"));
    assert_eq!(info.kind, ComponentKind::Binary);
}

#[test]
fn compose_requires_a_provider_on_the_left() {
    let err = compose(&kit("app"), &kit("app")).unwrap_err();
    assert_eq!(err, ComposeError::NotAProvider);
}

#[test]
fn compose_satisfies_the_residual_formula() {
    // imports(p $ c) = imports(p) ∪ (imports(c) ∖ exports(p))
    let p = kit("provider-b-from-a");
    let c = kit("app");
    let result = compose(&p, &c).unwrap();

    let p_info = p.describe();
    let c_info = c.describe();
    let expected: BTreeSet<String> = import_slots(&p_info)
        .into_iter()
        .chain(
            import_slots(&c_info)
                .into_iter()
                .filter(|slot| !export_slots(&p_info).contains(slot)),
        )
        .collect();
    assert_eq!(import_slots(&result.describe()), expected);
}

#[test]
fn compose_preserves_kind_and_drops_unconsumed_provider_exports() {
    // exports(p $ c) = exports(c): provider exports the consumer does not import are
    // dropped, and the result is whatever the consumer is.
    let provider = kit("provider-ab");
    let binary = kit("app-a");
    let composed = compose(&provider, &binary).unwrap();
    assert_eq!(composed.kind(), ComponentKind::Binary);
    assert_eq!(
        export_slots(&composed.describe()),
        export_slots(&binary.describe())
    );

    // Provider into provider yields a provider with the consumer's exports.
    let middleware = kit("provider-b-from-a");
    let layered = compose(&kit("provider-a"), &middleware).unwrap();
    assert_eq!(layered.kind(), ComponentKind::Provider);
    assert_eq!(
        export_slots(&layered.describe()),
        export_slots(&middleware.describe())
    );
    assert!(!import_slots(&layered.describe()).contains("fix:kit/cap-a"));
}

#[test]
fn the_empty_provider_is_the_identity_for_compose() {
    let app = kit("app");
    let composed = compose(&kit("empty"), &app).unwrap();
    assert_equivalent(&composed, &app);
}

#[test]
fn compose_reports_type_mismatches() {
    // A provider exporting a structurally different interface under the same slot name.
    let err = compose(&kit_mismatched(), &kit("app")).unwrap_err();
    assert!(matches!(err, ComposeError::TypeMismatch(_)), "{err:?}");
}

#[test]
fn compose_matches_versions_by_the_semver_rule() {
    // Same major, newer minor: satisfied and sealed.
    let sealed = compose(&ver_provider("1.2.0"), &ver_consumer("1.0.0")).unwrap();
    assert!(!import_slots(&sealed.describe()).contains("fix:ver/api"));

    // Older minor does not satisfy a newer requirement: the import stays residual.
    let unsealed = compose(&ver_provider("1.0.0"), &ver_consumer("1.2.0")).unwrap();
    assert!(import_slots(&unsealed.describe()).contains("fix:ver/api"));

    // A different major never unifies.
    let unsealed = compose(&ver_provider("2.0.0"), &ver_consumer("1.0.0")).unwrap();
    assert!(import_slots(&unsealed.describe()).contains("fix:ver/api"));
}

#[test]
fn compose_matches_by_slot_name_and_rename_retargets_slots() {
    let tool = kit("named-slots");
    let provider = kit("provider-a");

    // A default-slot export does not satisfy a differently-named slot of the same type.
    let untouched = compose(&provider, &tool).unwrap();
    assert!(import_slots(&untouched.describe()).contains("left"));
    assert!(import_slots(&untouched.describe()).contains("right"));

    // Renaming the provider's export onto the slot is exactly `with p as left`.
    let as_left = rename(&provider, "fix:kit/cap-a", "left").unwrap();
    let bound = compose(&as_left, &tool).unwrap();
    assert!(!import_slots(&bound.describe()).contains("left"));
    assert!(import_slots(&bound.describe()).contains("right"));
}

#[test]
fn dropping_is_just_composition_with_a_none_provider() {
    let none_a = kit("provider-none-a");
    let app_optional = kit("app-optional");
    let app = kit("app");

    // Sealing an optional import with X.none leaves no residual for outer layers.
    let dropped = compose(&none_a, &app_optional).unwrap();
    assert!(!import_slots(&dropped.describe()).contains("fix:kit/cap-a-optional"));

    // An outer grant cannot undo an inner drop: p $ X.none $ c ≡ X.none $ c when p
    // provides only X.
    let outer_grant = compose(&kit("provider-a"), &dropped).unwrap();
    assert_equivalent(&outer_grant, &dropped);

    // X.none $ c ≡ c when c never imports X (the drop is a no-op).
    let noop = compose(&none_a, &app).unwrap();
    assert_equivalent(&noop, &app);
}

#[test]
fn compose_is_deterministic() {
    let once = compose(&kit("provider-a"), &kit("app")).unwrap();
    let twice = compose(&kit("provider-a"), &kit("app")).unwrap();
    assert_eq!(once.save(), twice.save());
}

#[test]
fn compose_works_against_the_real_eo9_wit() {
    let composed = compose(&eo9_fixture("text-mock"), &eo9_fixture("hello")).unwrap();
    let info = composed.describe();
    assert_eq!(info.kind, ComponentKind::Binary);
    assert!(!import_slots(&info).contains("eo9:text/text"));
    assert!(import_slots(&info).contains("eo9:entropy/entropy-optional"));
}

// ---------------------------------------------------------------------------
// extend (&) -- milestone 3
// ---------------------------------------------------------------------------

#[test]
fn extend_requires_providers_on_both_sides() {
    assert_eq!(
        extend(&kit("app"), &kit("provider-a")).unwrap_err(),
        ComposeError::NotAProvider
    );
    assert_eq!(
        extend(&kit("provider-a"), &kit("app")).unwrap_err(),
        ComposeError::NotAProvider
    );
}

#[test]
fn extend_wires_imports_and_takes_the_right_biased_export_union() {
    let x = kit("provider-ab");
    let y = kit("provider-b-from-a");
    let env = extend(&x, &y).unwrap();
    let info = env.describe();
    assert_eq!(info.kind, ComponentKind::Provider);
    // exports(x & y) = exports(y) ∪ (exports(x) ∖ exports(y))
    let expected: BTreeSet<String> = export_slots(&y.describe())
        .into_iter()
        .chain(export_slots(&x.describe()))
        .collect();
    assert_eq!(export_slots(&info), expected);
    // imports(x & y) = imports(x) ∪ (imports(y) ∖ exports(x)): y's cap-a need is wired
    // from x and sealed.
    assert!(!import_slots(&info).contains("fix:kit/cap-a"));
}

#[test]
fn extend_is_associative() {
    let x = kit("provider-a");
    let y = kit("provider-b-from-a");
    let z = kit("provider-c-from-b");
    let left = extend(&extend(&x, &y).unwrap(), &z).unwrap();
    let right = extend(&x, &extend(&y, &z).unwrap()).unwrap();
    assert_equivalent(&left, &right);
}

#[test]
fn the_empty_provider_is_the_identity_for_extend() {
    let p = kit("provider-b-from-a");
    let left = extend(&kit("empty"), &p).unwrap();
    let right = extend(&p, &kit("empty")).unwrap();
    assert_equivalent(&left, &p);
    assert_equivalent(&right, &p);
}

#[test]
fn extend_satisfies_the_action_law() {
    // (x & y) $ c ≡ x $ y $ c
    let x = kit("provider-a");
    let y = kit("provider-b-from-a");
    let c = kit("app");
    let bundled = compose(&extend(&x, &y).unwrap(), &c).unwrap();
    let chained = compose(&x, &compose(&y, &c).unwrap()).unwrap();
    assert_equivalent(&bundled, &chained);
    // And the environment actually seals both needs.
    assert!(!import_slots(&bundled.describe()).contains("fix:kit/cap-a"));
    assert!(!import_slots(&bundled.describe()).contains("fix:kit/cap-b"));
}

#[test]
fn extend_is_deterministic() {
    let once = extend(&kit("provider-a"), &kit("provider-b-from-a")).unwrap();
    let twice = extend(&kit("provider-a"), &kit("provider-b-from-a")).unwrap();
    assert_eq!(once.save(), twice.save());
}

// ---------------------------------------------------------------------------
// rename -- milestone 3
// ---------------------------------------------------------------------------

#[test]
fn rename_relabels_an_import_slot() {
    let app = kit("app");
    let renamed = rename(&app, "fix:kit/cap-a", "primary-cap").unwrap();
    let info = renamed.describe();
    let slot = info
        .imports
        .iter()
        .find(|i| i.slot == "primary-cap")
        .expect("renamed slot");
    assert_eq!(slot.interface, "fix:kit/cap-a");
    assert_eq!(slot.version, "1.0.0");
    assert!(slot.required);
    assert!(!import_slots(&info).contains("fix:kit/cap-a"));
    // Everything else is untouched.
    assert!(import_slots(&info).contains("fix:kit/cap-b"));
    assert_eq!(info.kind, ComponentKind::Binary);
}

#[test]
fn rename_relabels_an_export_slot() {
    let provider = kit("provider-a");
    let renamed = rename(&provider, "fix:kit/cap-a", "my-cap").unwrap();
    let info = renamed.describe();
    let slot = info
        .exports
        .iter()
        .find(|e| e.name == "my-cap")
        .expect("renamed export");
    assert_eq!(slot.interface, "fix:kit/cap-a");
    assert!(!export_slots(&info).contains("fix:kit/cap-a"));
}

#[test]
fn rename_round_trips() {
    let app = kit("app");
    let there = rename(&app, "fix:kit/cap-a", "primary-cap").unwrap();
    let back = rename(&there, "primary-cap", "fix:kit/cap-a").unwrap();
    assert_equivalent(&back, &app);

    let provider = kit("provider-a");
    let there = rename(&provider, "fix:kit/cap-a", "my-cap").unwrap();
    let back = rename(&there, "my-cap", "fix:kit/cap-a").unwrap();
    assert_equivalent(&back, &provider);
}

#[test]
fn rename_rejects_missing_slots_and_collisions() {
    assert!(matches!(
        rename(&kit("app"), "no-such-slot", "whatever"),
        Err(RenameError::NoSuchSlot(_))
    ));
    assert!(matches!(
        rename(&kit("named-slots"), "left", "right"),
        Err(RenameError::SlotCollision(_))
    ));
    // A default-style target must name the slot's own interface.
    assert!(matches!(
        rename(&kit("app"), "fix:kit/cap-a", "fix:kit/cap-b"),
        Err(RenameError::SlotCollision(_))
    ));
}

#[test]
fn rename_is_deterministic() {
    let once = rename(&kit("app"), "fix:kit/cap-a", "primary-cap").unwrap();
    let twice = rename(&kit("app"), "fix:kit/cap-a", "primary-cap").unwrap();
    assert_eq!(once.save(), twice.save());
}

// ---------------------------------------------------------------------------
// restrict (only) -- milestone 3
// ---------------------------------------------------------------------------

fn allow(names: &[&str]) -> Vec<InterfaceRef> {
    names.iter().map(|n| InterfaceRef::any(*n)).collect()
}

#[test]
fn restrict_passes_components_within_the_allow_list() {
    let app = kit("app");
    let bounded = restrict(&app, &allow(&["fix:kit/cap-a", "fix:kit/cap-b"])).unwrap();
    assert_equivalent(&bounded, &app);
}

#[test]
fn restrict_rejects_required_imports_outside_the_allow_list() {
    let err = restrict(&kit("app"), &allow(&["fix:kit/cap-a"])).unwrap_err();
    let RestrictError::RequiredOutsideAllowList(offenders) = err else {
        panic!("expected RequiredOutsideAllowList, got {err:?}");
    };
    assert_eq!(offenders, vec!["fix:kit/cap-b@1.0.0".to_string()]);
}

#[test]
fn restrict_seals_optional_imports_outside_the_allow_list() {
    let app = kit("app-optional");
    let bounded = restrict(&app, &allow(&["fix:kit/cap-b"])).unwrap();
    let info = bounded.describe();
    assert!(!import_slots(&info).contains("fix:kit/cap-a-optional"));
    assert!(import_slots(&info).contains("fix:kit/cap-b"));
    assert_eq!(info.kind, ComponentKind::Binary);
    // Sealing is observationally the same as composing the API's none stub.
    let via_none = compose(&kit("provider-none-a"), &app).unwrap();
    assert_equivalent(&bounded, &via_none);
}

#[test]
fn restrict_admits_both_flavors_of_an_allowed_interface() {
    // An entry admits the `-optional` flavor of its interface too.
    let app = kit("app-optional");
    let bounded = restrict(&app, &allow(&["fix:kit/cap-a", "fix:kit/cap-b"])).unwrap();
    assert_equivalent(&bounded, &app);
}

#[test]
fn restrict_is_idempotent_and_restrictions_intersect() {
    let app = kit("app-optional");
    let wide = allow(&["fix:kit/cap-a", "fix:kit/cap-b"]);
    let narrow = allow(&["fix:kit/cap-b"]);

    // only w is idempotent.
    let once = restrict(&app, &narrow).unwrap();
    let twice = restrict(&once, &narrow).unwrap();
    assert_equivalent(&once, &twice);

    // only v $ only w $ c ≡ only (v ∩ w) $ c.
    let nested = restrict(&restrict(&app, &wide).unwrap(), &narrow).unwrap();
    let intersection = restrict(&app, &narrow).unwrap();
    assert_equivalent(&nested, &intersection);
}

#[test]
fn restrict_respects_allow_list_versions() {
    let consumer = ver_consumer("1.2.0");
    // A version-pinned entry admits imports it could satisfy per the semver rule...
    let ok = restrict(
        &consumer,
        &[InterfaceRef {
            interface: "fix:ver/api".to_string(),
            version: Some("1.3.0".to_string()),
        }],
    );
    assert!(ok.is_ok());
    // ... and rejects imports newer than it.
    let err = restrict(
        &consumer,
        &[InterfaceRef {
            interface: "fix:ver/api".to_string(),
            version: Some("1.0.0".to_string()),
        }],
    )
    .unwrap_err();
    assert!(matches!(err, RestrictError::RequiredOutsideAllowList(_)));
}

#[test]
fn restrict_rejects_malformed_allow_lists() {
    let err = restrict(&kit("app"), &allow(&["not-an-interface"])).unwrap_err();
    assert!(matches!(err, RestrictError::InvalidAllowList(_)));
    let err = restrict(
        &kit("app"),
        &[InterfaceRef {
            interface: "fix:kit/cap-a".to_string(),
            version: Some("not.a.version".to_string()),
        }],
    )
    .unwrap_err();
    assert!(matches!(err, RestrictError::InvalidAllowList(_)));
}

#[test]
fn restrict_works_against_the_real_eo9_wit() {
    let hello = eo9_fixture("hello");

    // The entropy grant is optional: restricting it away seals it as absent.
    let no_entropy = restrict(&hello, &allow(&["eo9:text/text"])).unwrap();
    assert!(!import_slots(&no_entropy.describe()).contains("eo9:entropy/entropy-optional"));
    assert!(import_slots(&no_entropy.describe()).contains("eo9:text/text"));

    // The text requirement is hard: an empty allow-list is a compose-time error.
    let err = restrict(&hello, &[]).unwrap_err();
    let RestrictError::RequiredOutsideAllowList(offenders) = err else {
        panic!("expected RequiredOutsideAllowList");
    };
    assert_eq!(offenders, vec!["eo9:text/text@0.1.0".to_string()]);
}

#[test]
fn restrict_is_deterministic() {
    let app = kit("app-optional");
    let once = restrict(&app, &allow(&["fix:kit/cap-b"])).unwrap();
    let twice = restrict(&app, &allow(&["fix:kit/cap-b"])).unwrap();
    assert_eq!(once.save(), twice.save());
}

// ---------------------------------------------------------------------------
// configure -- binding a provider's compose-time constants
// ---------------------------------------------------------------------------

#[test]
fn describe_reports_a_providers_config_arguments() {
    let seeded = seeded_provider();
    let info = seeded.describe();
    assert_eq!(info.kind, ComponentKind::Provider);
    assert!(export_slots(&info).contains("eo9:entropy/seeded-config"));
    let args: Vec<(&str, &str)> = info
        .args
        .iter()
        .map(|a| (a.name.as_str(), a.ty.as_str()))
        .collect();
    assert_eq!(args, vec![("seed", "u64")]);
}

#[test]
fn configure_bakes_args_and_seals_the_config_interface() {
    let seeded = seeded_provider();
    let configured = configure(&seeded, &[("seed", "42")]).unwrap();
    let info = configured.describe();

    // Still an ordinary provider, but the config surface is gone and there is nothing
    // left to bind.
    assert_eq!(info.kind, ComponentKind::Provider);
    let exports = export_slots(&info);
    assert!(exports.contains("eo9:entropy/entropy"));
    assert!(exports.contains("eo9:entropy/types"));
    assert!(!exports.contains("eo9:entropy/seeded-config"));
    assert!(info.args.is_empty());

    // It composes like any provider: the consumer's entropy need is sealed and the
    // config interface never reaches it.
    let consumer = eo9_fixture("entropy-user");
    let bound = compose(&configured, &consumer).unwrap();
    assert_eq!(bound.kind(), ComponentKind::Binary);
    assert!(!import_slots(&bound.describe()).contains("eo9:entropy/entropy"));
    assert!(!import_slots(&bound.describe()).contains("eo9:entropy/types"));
}

#[test]
fn configure_requires_a_provider() {
    let err = configure(&eo9_fixture("hello"), &[("seed", "1")]).unwrap_err();
    assert_eq!(err, ConfigureError::NotAProvider);
}

#[test]
fn configure_requires_a_config_interface() {
    // A provider without a config interface has nothing to bind ...
    let err = configure(&kit("provider-ab"), &[] as &[(&str, &str)]).unwrap_err();
    assert_eq!(err, ConfigureError::NoConfigInterface);

    // ... and an already-configured provider errors the same way (no double-configure).
    let configured = configure(&seeded_provider(), &[("seed", "42")]).unwrap();
    let err = configure(&configured, &[("seed", "42")]).unwrap_err();
    assert_eq!(err, ConfigureError::NoConfigInterface);
}

#[test]
fn configure_rejects_unknown_missing_and_ill_typed_arguments() {
    let seeded = seeded_provider();

    let err = configure(&seeded, &[("seed", "1"), ("extra", "2")]).unwrap_err();
    assert_eq!(err, ConfigureError::UnknownArgument("extra".to_string()));

    let err = configure(&seeded, &[] as &[(&str, &str)]).unwrap_err();
    assert_eq!(err, ConfigureError::MissingArgument("seed".to_string()));

    let err = configure(&seeded, &[("seed", "\"not a number\"")]).unwrap_err();
    assert!(
        matches!(&err, ConfigureError::InvalidArgument { name, .. } if name == "seed"),
        "{err:?}"
    );

    let err = configure(&seeded, &[("seed", "1"), ("seed", "2")]).unwrap_err();
    assert!(
        matches!(&err, ConfigureError::InvalidArgument { name, .. } if name == "seed"),
        "{err:?}"
    );
}

#[test]
fn configure_is_deterministic() {
    let once = configure(&seeded_provider(), &[("seed", "7")]).unwrap();
    let twice = configure(&seeded_provider(), &[("seed", "7")]).unwrap();
    assert_eq!(once.save(), twice.save());
}

// ---------------------------------------------------------------------------
// configure -- compound argument values (lists, records, options, tuples)
// ---------------------------------------------------------------------------

/// A full compound argument set for the `provider-d` fixture.
const COMPOUND_ARGS: &[(&str, &str)] = &[
    ("thresholds", "[1, 2, 3]"),
    (
        "probes",
        "[{offset: 4, label: \"alpha\"}, {offset: 9, label: \"beta\"}]",
    ),
    ("title", "\"compound\""),
    ("scale", "some(5)"),
    ("mode", "careful"),
    ("pair", "(7, true)"),
];

#[test]
fn configure_bakes_compound_arguments_and_seals_the_config_interface() {
    let provider = kit("provider-d");
    let info = provider.describe();
    assert!(export_slots(&info).contains("fix:kit/cap-d-config"));

    let configured = configure(&provider, COMPOUND_ARGS).unwrap();
    let info = configured.describe();
    assert_eq!(info.kind, ComponentKind::Provider);
    let exports = export_slots(&info);
    assert!(exports.contains("fix:kit/cap-b"));
    assert!(!exports.contains("fix:kit/cap-d-config"));
    assert!(info.args.is_empty());

    // The encoded result is a valid component and composes like any provider.
    Component::load(configured.bytes().to_vec()).expect("configured provider revalidates");
    let bound = compose(&configured, &kit("app")).unwrap();
    assert_eq!(bound.kind(), ComponentKind::Binary);
    assert!(!import_slots(&bound.describe()).contains("fix:kit/cap-b"));
}

#[test]
fn configure_with_compound_arguments_is_deterministic() {
    let once = configure(&kit("provider-d"), COMPOUND_ARGS).unwrap();
    let twice = configure(&kit("provider-d"), COMPOUND_ARGS).unwrap();
    assert_eq!(once.save(), twice.save());
}

#[test]
fn configure_accepts_empty_lists_and_absent_options() {
    let configured = configure(
        &kit("provider-d"),
        &[
            ("thresholds", "[]"),
            ("probes", "[]"),
            ("title", "\"\""),
            ("scale", "none"),
            ("mode", "fast"),
            ("pair", "(0, false)"),
        ],
    )
    .unwrap();
    Component::load(configured.bytes().to_vec()).expect("configured provider revalidates");
}

#[test]
fn configure_spills_wide_parameter_lists_to_memory() {
    let args: Vec<(String, String)> = "abcdefghijklm"
        .chars()
        .enumerate()
        .map(|(index, name)| (name.to_string(), (index as u64 * 1000 + 1).to_string()))
        .chain([
            ("text".to_string(), "\"spilled\"".to_string()),
            ("nums".to_string(), "[10, 20, 30]".to_string()),
        ])
        .collect();
    let args: Vec<(&str, &str)> = args.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();

    let once = configure(&kit("provider-f"), &args).unwrap();
    let twice = configure(&kit("provider-f"), &args).unwrap();
    assert_eq!(once.save(), twice.save());
    Component::load(once.bytes().to_vec()).expect("configured provider revalidates");

    let info = once.describe();
    assert!(!export_slots(&info).contains("fix:kit/cap-f-config"));
    assert!(export_slots(&info).contains("fix:kit/cap-b"));
}

#[test]
fn configure_rejects_unbakeable_parameter_types_with_a_typed_refusal() {
    let err = configure(&kit("provider-e"), &[("t", "plain")]).unwrap_err();
    // The refusal is a typed variant carrying the parameter and the offending kind, so
    // callers can match on it instead of substring-searching a message ...
    match &err {
        ConfigureError::UnbakeableType { name, kind } => {
            assert_eq!(name, "t");
            assert_eq!(kind, "variant");
        }
        other => panic!("expected the typed unbakeable-type refusal, got {other:?}"),
    }
    // ... and its rendered message still says what is and is not supported.
    let message = format!("{err}");
    assert!(
        message.contains("cannot bake") && message.contains("variant"),
        "unexpected message: {message}"
    );
}

#[test]
fn configure_rejects_malformed_compound_values() {
    // An unterminated list literal.
    let mut args: Vec<(&str, &str)> = COMPOUND_ARGS.to_vec();
    args[0] = ("thresholds", "[1, 2");
    let err = configure(&kit("provider-d"), &args).unwrap_err();
    assert!(
        matches!(&err, ConfigureError::InvalidArgument { name, .. } if name == "thresholds"),
        "{err:?}"
    );

    // A record value with a misnamed field.
    let mut args: Vec<(&str, &str)> = COMPOUND_ARGS.to_vec();
    args[1] = ("probes", "[{offset: 4, wrong: \"alpha\"}]");
    let err = configure(&kit("provider-d"), &args).unwrap_err();
    assert!(
        matches!(&err, ConfigureError::InvalidArgument { name, .. } if name == "probes"),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// configure -- providers with async API functions (the time.frozen shape)
// ---------------------------------------------------------------------------

#[test]
fn configure_binds_an_async_api_provider_and_seals_its_config_interface() {
    let frozen = frozen_provider();

    // The fixture mirrors the real time.frozen stub: an async `sleep` in the API, an
    // async `configure` in the config interface, two declared parameters.
    let info = frozen.describe();
    assert_eq!(info.kind, ComponentKind::Provider);
    let args: Vec<(&str, &str)> = info
        .args
        .iter()
        .map(|a| (a.name.as_str(), a.ty.as_str()))
        .collect();
    assert_eq!(args, vec![("now-seconds", "s64"), ("monotonic-ns", "u64")]);

    let configured = configure(
        &frozen,
        &[("now-seconds", "1111"), ("monotonic-ns", "2222")],
    )
    .unwrap();
    let info = configured.describe();

    // Still an ordinary provider; the config surface is gone, the API (including the
    // async `sleep`) and the types interface remain.
    assert_eq!(info.kind, ComponentKind::Provider);
    let exports = export_slots(&info);
    assert!(exports.contains("eo9:time/time"));
    assert!(exports.contains("eo9:time/types"));
    assert!(!exports.contains("eo9:time/frozen-config"));
    assert!(info.args.is_empty());

    // It composes like any provider: the consumer's time need is sealed and the config
    // interface never reaches it.
    let bound = compose(&configured, &clock_user()).unwrap();
    assert_eq!(bound.kind(), ComponentKind::Binary);
    assert!(!import_slots(&bound.describe()).contains("eo9:time/time"));
    assert!(!import_slots(&bound.describe()).contains("eo9:time/frozen-config"));
}

#[test]
fn configure_of_an_async_api_provider_is_deterministic() {
    let bind = || {
        configure(
            &frozen_provider(),
            &[("now-seconds", "1111"), ("monotonic-ns", "2222")],
        )
        .unwrap()
    };
    assert_eq!(bind().save(), bind().save());
}

#[test]
fn configure_binds_resource_owning_providers() {
    // fs.memfs's API interface defines its own resources (`file`, `immutable-handle`).
    // Under the alias + bind construction (plan/03 D21) that no longer matters: the API
    // is re-exported by direct alias (resources keep their identity, nothing is
    // proxied), and only the configuration call itself goes through the synthesized
    // `eo9:rt/configured.bind` entrypoint.
    let configured = configure(&memfs_provider(), &[] as &[(&str, &str)]).unwrap();
    let info = configured.describe();

    assert_eq!(info.kind, ComponentKind::Provider);
    let exports = export_slots(&info);
    assert!(exports.contains("eo9:fs/fs"), "{exports:?}");
    assert!(!exports.contains("eo9:fs/memfs-config"), "{exports:?}");
    assert!(
        exports.contains("eo9:rt/configured"),
        "the configured provider must carry the bind entrypoint: {exports:?}"
    );
}

#[test]
fn configured_components_carry_the_bind_entrypoint_through_composition() {
    // The rider must survive `$` (and stay invisible to kind classification): an
    // executor only ever sees the outermost component, so `configure(p) $ c` has to
    // re-export p's entrypoint or the baked arguments could never be applied.
    let configured = configure(&seeded_provider(), &[("seed", "42")]).unwrap();
    assert!(
        export_slots(&configured.describe()).contains("eo9:rt/configured"),
        "{:?}",
        export_slots(&configured.describe())
    );

    let bound = compose(&configured, &eo9_fixture("entropy-user")).unwrap();
    assert_eq!(bound.kind(), ComponentKind::Binary);
    assert!(
        export_slots(&bound.describe()).contains("eo9:rt/configured"),
        "the rider must propagate through compose: {:?}",
        export_slots(&bound.describe())
    );

    // Both operands configured: `&` merges the two entrypoints into one.
    let frozen = configure(
        &frozen_provider(),
        &[("now-seconds", "1111"), ("monotonic-ns", "2222")],
    )
    .unwrap();
    let env = eo9_component::extend(&configured, &frozen).unwrap();
    let riders = env
        .describe()
        .exports
        .iter()
        .filter(|e| e.interface == "eo9:rt/configured")
        .count();
    assert_eq!(riders, 1, "exactly one merged entrypoint");
}

// ---------------------------------------------------------------------------
// Component manuals: the wac-nesting pin (docs/design/component-manuals.md, workaround 2)
// ---------------------------------------------------------------------------

/// Unsigned LEB128 (the only encoding the section frames use).
fn leb(mut value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out
}

/// An encoded custom section (id 0) with the given name and data.
fn custom_section(name: &str, data: &[u8]) -> Vec<u8> {
    let mut payload = leb(name.len() as u32);
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(data);
    let mut out = vec![0u8];
    out.extend(leb(payload.len() as u32));
    out.extend(payload);
    out
}

/// Append an OUTER custom section to a component (custom sections are legal anywhere
/// after the preamble; the end is the cheapest spot).
fn append_custom_section(mut bytes: Vec<u8>, name: &str, data: &[u8]) -> Vec<u8> {
    bytes.extend(custom_section(name, data));
    bytes
}

/// Inject a custom section into a component's FIRST depth-1 core module — the canonical
/// place the guest SDK's `#[link_section]` static lands. Walks the outer sections,
/// re-frames the module section around the grown module, leaves everything else as is.
fn inject_core_module_custom(bytes: Vec<u8>, name: &str, data: &[u8]) -> Vec<u8> {
    fn read_leb(bytes: &[u8], pos: &mut usize) -> u32 {
        let mut result = 0u32;
        let mut shift = 0;
        loop {
            let byte = bytes[*pos];
            *pos += 1;
            result |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return result;
            }
            shift += 7;
        }
    }
    assert_eq!(&bytes[0..4], b"\0asm", "a wasm container");
    assert_eq!(bytes[6], 1, "a component (layer 1)");
    let mut out = bytes[..8].to_vec();
    let mut pos = 8usize;
    let mut injected = false;
    while pos < bytes.len() {
        let id = bytes[pos];
        pos += 1;
        let mut size_pos = pos;
        let size = read_leb(&bytes, &mut size_pos) as usize;
        let payload = &bytes[size_pos..size_pos + size];
        if id == 1 && !injected {
            // The first core module: append the custom section inside it and re-frame.
            let mut module = payload.to_vec();
            module.extend(custom_section(name, data));
            out.push(1);
            out.extend(leb(module.len() as u32));
            out.extend(module);
            injected = true;
        } else {
            out.extend_from_slice(&bytes[pos - 1..size_pos + size]);
        }
        pos = size_pos + size;
    }
    assert!(injected, "the component carries a core module");
    out
}

#[test]
fn wac_nesting_buries_operand_manuals_below_the_man_scanners_reach() {
    // The fused-artifact rule (component-manuals design, section 1): a saved
    // composition has no top-level manual — `man` falls back to `describe`, which is
    // honest, because a composition's behavior is the algebra's, not one part's prose.
    // That rests on a wac-graph behavior: operands nest as components, putting their
    // core modules at depth 2, below the scanner's outer + depth-1 reach. This test
    // pins it so a wac upgrade cannot silently change `man`'s fallback.
    use eosh_core::manual::extract_manual;

    const PROVIDER_MANUAL: &str = "eo9-manual 1\nname: text-mock\nsynopsis: a text provider\nend\n";
    const CONSUMER_MANUAL: &str = "eo9-manual 1\nname: hello\nsynopsis: say hello\nend\n";

    // Operands carrying manuals in the canonical place (a custom section of their core
    // module, where the guest SDK's link_section static lands)…
    let provider_bytes = inject_core_module_custom(
        eo9_fixture("text-mock").save(),
        "eo9-manual",
        PROVIDER_MANUAL.as_bytes(),
    );
    let consumer_bytes = inject_core_module_custom(
        eo9_fixture("hello").save(),
        "eo9-manual",
        CONSUMER_MANUAL.as_bytes(),
    );
    // …are found there before composition (this is `man <operand>` working).
    assert_eq!(
        extract_manual(&provider_bytes),
        Some(PROVIDER_MANUAL.as_bytes())
    );
    assert_eq!(
        extract_manual(&consumer_bytes),
        Some(CONSUMER_MANUAL.as_bytes())
    );

    let provider = Component::load(provider_bytes).expect("the operand still loads");
    let consumer = Component::load(consumer_bytes).expect("the operand still loads");
    let fused = compose(&provider, &consumer).expect("composes");
    assert_eq!(
        extract_manual(&fused.save()),
        None,
        "a fused artifact must answer `man` with the describe fallback, never one \
         operand's manual — wac's nesting (or the scanner's depth rule) changed"
    );

    // The outer-custom variant (the design's contingency location) is buried the same way.
    let outer = append_custom_section(
        eo9_fixture("text-mock").save(),
        "eo9-manual",
        b"outer manual",
    );
    assert_eq!(extract_manual(&outer), Some(&b"outer manual"[..]));
    let outer = Component::load(outer).expect("an appended custom section still loads");
    let fused = compose(&outer, &eo9_fixture("hello")).expect("composes");
    assert_eq!(extract_manual(&fused.save()), None);
}
