//! Compose-time diagnostics and artifact soundness for the shapes the PL user study
//! exercised (docs/user-studies/05-pl-researcher.md):
//!
//! * the no-op-drop law — a provider whose exports match nothing composes cleanly (and
//!   is reported as a dead layer rather than silently ignored);
//! * the export-shape rule — a provider that offers only its configuration interface
//!   for an API the consumer requires is refused with a `configure(…)` hint;
//! * `rename` of a residual import — the renamed component keeps its slot identity for
//!   `describe`, and its executable form compiles instead of failing to parse.

use eo9_component::{ComposeWarning, Component, compose, compose_checked, rename};
use eo9_integration::fixtures::build_component;
use eo9_integration::{guest, run};

/// `fs.none $ cat`: the provider's optional-flavor export matches nothing `cat`
/// imports, so per the drop law the composition succeeds, the consumer's own fs import
/// stays residual, and the dead layer is reported as a warning (study finding 2; the
/// encode/validation failure this shape used to produce is the regression being
/// guarded).
#[test]
fn unmatched_provider_exports_compose_cleanly_and_warn() {
    guest::ensure_components(&["eo9-stub-fs-none", "eo9-coreutil-cat"]);
    let fs_none = guest::load_stub("fs.none");
    let cat = guest::load_component("eo9-coreutil-cat");

    let (composed, warnings) =
        compose_checked(&fs_none, &cat).expect("fs.none $ cat must compose (no-op drop law)");

    // The consumer is unchanged: its required fs import is still a residual import.
    let info = composed.describe();
    assert!(
        info.imports
            .iter()
            .any(|i| i.interface == "eo9:fs/fs" && i.required),
        "cat's required fs import must remain residual: {:?}",
        info.imports
    );
    // And the dead layer is called out.
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, ComposeWarning::ProviderExportsUnused { .. })),
        "expected a dead-layer warning, got {warnings:?}"
    );
}

/// A fully-shadowed provider layer (an outer entropy seed behind an inner one) is the
/// spec's "exports match nothing" example: it must compose and warn.
#[test]
fn fully_shadowed_layer_warns() {
    guest::ensure_components(&["eo9-stub-entropy-seeded", "eo9-coreutil-rng"]);
    let seeded = guest::load_stub("entropy.seeded");
    let rng = guest::load_component("eo9-coreutil-rng");

    let inner = compose(&seeded, &rng).expect("entropy.seeded $ rng");
    let (_, warnings) = compose_checked(&guest::load_stub("entropy.seeded"), &inner)
        .expect("a dead outer entropy layer still composes");
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w, ComposeWarning::ProviderExportsUnused { .. })),
        "the dead outer seed must be reported: {warnings:?}"
    );
}

/// A provider that exports only its configuration interface, composed onto a consumer
/// that requires the API, is refused with the configure hint (SPEC "export shape
/// encodes whether configuration is required").
#[test]
fn config_only_provider_is_refused_with_a_configure_hint() {
    const PROVIDER_WIT: &str = r#"
package eo9-tests:cap@0.1.0;

interface answer-config {
    configure: func(value: u32) -> result<_, string>;
}

world answer-config-only {
    export answer-config;
}
"#;
    const PROVIDER_CORE: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "eo9-tests:cap/answer-config@0.1.0#configure") (param i32) (result i32)
    (i32.store8 (i32.const 32) (i32.const 0))
    (i32.const 32)))
"#;
    let config_only = build_component(PROVIDER_WIT, &[], "answer-config-only", PROVIDER_CORE);
    let consumer = eo9_integration::fixtures::answer_consumer();

    let err = compose(&config_only, &consumer)
        .expect_err("an unconfigured config-only provider must be refused");
    let message = err.to_string();
    assert!(
        message.contains("configure"),
        "the refusal must point at configure(…): {message}"
    );
}

/// `rename eo9:time/time wallclock $ hello`: the renamed slot keeps its interface
/// identity for `describe`, and the executable form of the artifact compiles (study
/// finding 3: this used to fail wasm parsing inside the runtime's compile).
#[test]
fn renamed_residual_import_still_compiles() {
    guest::ensure_components(&["eo9-example-hello"]);
    let hello = guest::load_example("hello");
    let renamed = rename(&hello, "eo9:time/time", "wallclock").expect("rename to a plain slot");

    let info = renamed.describe();
    let slot = info
        .imports
        .iter()
        .find(|i| i.slot == "wallclock")
        .expect("the renamed slot is present");
    assert_eq!(slot.interface, "eo9:time/time");

    // The executable form must be acceptable to the runtime's compiler; the artifact
    // itself stays annotated so the algebra keeps the slot identity above.
    let executable = Component::load(renamed.executable_bytes())
        .expect("the executable form is still a valid component");
    let _image = run::compile_component(&executable);
}
