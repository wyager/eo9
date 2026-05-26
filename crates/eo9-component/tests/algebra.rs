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
    eo9_fixture, kit, kit_bytes, kit_mismatched, seeded_provider, ver_consumer, ver_provider,
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
