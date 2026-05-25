//! Capability suite (plan/13-tests.md milestone 1): the spec's capability claims observed
//! end-to-end — components are composed with `eo9-component` and then executed with
//! `eo9-runtime`, so sealing, `only`, deny-style providers, slot wiring, and optional
//! absence are checked through program behaviour, not just through `describe()`.

use eo9_component::{ComponentKind, InterfaceRef, RestrictError, compose, rename, restrict};
use eo9_integration::{fixtures, run};
use eo9_runtime::providers::CaptureText;
use eo9_runtime::{Outcome, Providers, SpawnError, SpawnLimits, Task};

/// SPEC "Composition and the `$` operator", Sealing: in `p $ c` a matched import is not an
/// import of the result, and no outer layer can re-satisfy it — the innermost provider
/// wins, observably.
#[test]
fn a_sealed_capability_cannot_be_regranted_by_an_outer_provider() {
    let consumer = fixtures::answer_consumer();
    let inner = fixtures::answer_provider(7);
    let outer = fixtures::answer_provider(99);

    // Before composition the consumer still needs `answer` from somewhere.
    assert!(
        consumer
            .describe()
            .imports
            .iter()
            .any(|i| i.interface == "eo9-tests:cap/answer" && i.required)
    );

    // Sealing: after `inner $ consumer` the import is gone from the composition's surface.
    let sealed = compose(&inner, &consumer).expect("inner $ consumer");
    assert_eq!(sealed.kind(), ComponentKind::Binary);
    assert!(sealed.describe().imports.is_empty());

    // An outer grant of the same capability has nothing left to satisfy: its export is
    // dropped and the surface is unchanged.
    let regranted = compose(&outer, &sealed).expect("outer $ (inner $ consumer)");
    assert!(regranted.describe().imports.is_empty());

    // And behaviourally the inner provider wins: both runs report the inner answer.
    let outcome = run::run_component(&sealed, &[], Providers::none());
    assert_eq!(run::success_value(&outcome), "7");
    let outcome = run::run_component(&regranted, &[], Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        "7",
        "the outer provider must never reach the sealed import"
    );
}

/// The ambient half of the sealing law: once an import is sealed by composition, the
/// runtime's root providers (the ambient context) cannot see or serve it either.
#[test]
fn a_sealed_capability_cannot_be_regranted_by_the_ambient_context() {
    let writer = fixtures::text_writer();
    let image = run::compile_component(&writer);

    // Unsealed, the import is served by the ambient root text provider...
    let ambient = CaptureText::new();
    let outcome = run::run_image(
        &image,
        &[],
        Providers {
            text: Some(Box::new(ambient.clone())),
            ..Providers::none()
        },
    );
    assert_eq!(run::success_value(&outcome), "42");
    assert_eq!(ambient.stdout(), fixtures::TEXT_WRITER_OUTPUT);

    // ...and without any grant the loader rule rejects the spawn before anything runs.
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap_err();
    match err {
        SpawnError::Internal(message) => {
            assert!(
                message.contains("eo9:text"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected the loader rule to reject the spawn, got {other:?}"),
    }

    // Composing a text provider seals the import: the program no longer needs any
    // ambient grant at all...
    let sealed = compose(&fixtures::text_sink_provider(), &writer).expect("text-sink $ writer");
    assert!(sealed.describe().imports.is_empty());
    let sealed_image = run::compile_component(&sealed);
    let outcome = run::run_image(&sealed_image, &[], Providers::none());
    assert_eq!(run::success_value(&outcome), "42");

    // ...and an ambient grant offered from outside can no longer reach the program.
    let ambient = CaptureText::new();
    let outcome = run::run_image(
        &sealed_image,
        &[],
        Providers {
            text: Some(Box::new(ambient.clone())),
            ..Providers::none()
        },
    );
    assert_eq!(run::success_value(&outcome), "42");
    assert_eq!(
        ambient.stdout(),
        "",
        "output must go to the sealed-in provider, never to the ambient one"
    );
}

/// SPEC "Restriction: `only`", rule 1: a required residual import outside the allow-list
/// is a compose-time error naming the offender — nothing is instantiated or run.
#[test]
fn only_rejects_required_imports_outside_the_allow_list_before_run() {
    let consumer = fixtures::answer_consumer();
    let err = restrict(&consumer, &[InterfaceRef::any("eo9-tests:cap/store")]).unwrap_err();
    match err {
        RestrictError::RequiredOutsideAllowList(offenders) => {
            assert!(
                offenders.iter().any(|o| o.contains("eo9-tests:cap/answer")),
                "the error must name the offending import: {offenders:?}"
            );
        }
        other => panic!("expected a required-outside-allow-list error, got {other:?}"),
    }
}

/// SPEC "Restriction: `only`", position matters: a capability satisfied *inside* the gate
/// is fine — the gate bounds what may still cross it, and a fully sealed program passes
/// even an empty allow-list (pure compute).
#[test]
fn only_admits_capabilities_satisfied_inside_the_gate() {
    let inside = compose(&fixtures::answer_provider(7), &fixtures::answer_consumer())
        .expect("answer-provider $ answer-consumer");
    let gated = restrict(&inside, &[]).expect("only [] $ (provider $ consumer)");
    assert!(gated.describe().imports.is_empty());

    let outcome = run::run_component(&gated, &[], Providers::none());
    assert_eq!(run::success_value(&outcome), "7");
}

/// SPEC "Restriction: `only`", rule 2: optional residual imports outside the allow-list
/// are sealed as absent, and the program observes that absence through the `-optional`
/// import's own type.
#[test]
fn only_seals_optional_residuals_and_the_program_observes_absence() {
    let consumer = fixtures::optional_consumer();
    assert!(
        consumer
            .describe()
            .imports
            .iter()
            .any(|i| i.interface == "eo9-tests:cap/answer-optional" && !i.required)
    );

    let gated = restrict(&consumer, &[]).expect("only [] $ optional-consumer");
    assert_eq!(gated.kind(), ComponentKind::Binary);
    assert!(
        gated.describe().imports.is_empty(),
        "optional residuals outside the gate must be sealed"
    );

    let outcome = run::run_component(&gated, &[], Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        fixtures::OPTIONAL_ABSENT_SENTINEL.to_string(),
        "the program must observe absence, not trap or fail"
    );
}

/// SPEC "The capability algebra": for optional imports, never granted ≡ explicitly
/// composed with the `none` stub — and a present provider is observed through the very
/// same import.
#[test]
fn absence_and_presence_are_both_observable_through_an_optional_import() {
    let consumer = fixtures::optional_consumer();

    // Composing the `none`-style provider is the same observation as `only`'s sealing.
    let absent = compose(&fixtures::optional_provider_absent(), &consumer)
        .expect("answer-optional.none $ optional-consumer");
    assert!(absent.describe().imports.is_empty());
    let outcome = run::run_component(&absent, &[], Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        fixtures::OPTIONAL_ABSENT_SENTINEL.to_string()
    );

    // A present optional capability is observed through the same import.
    let present = compose(&fixtures::optional_provider_present(5), &consumer)
        .expect("answer-optional(5) $ optional-consumer");
    let outcome = run::run_component(&present, &[], Providers::none());
    assert_eq!(run::success_value(&outcome), "5");
}

/// SPEC "Dropping: `X.none`, `X.deny`, and friends": a deny-style provider answers every
/// operation with the API's own error cases, in-band; the program turns that into its own
/// failure vocabulary — no trap, no exit code.
#[test]
fn a_deny_style_provider_fails_in_band_in_the_programs_own_error_vocabulary() {
    let consumer = fixtures::storage_consumer();

    // With a working provider the program succeeds with the provider's value.
    let working = compose(&fixtures::store_ok_provider(11), &consumer).expect("store $ consumer");
    let outcome = run::run_component(&working, &[], Providers::none());
    assert_eq!(run::success_value(&outcome), "11");

    // With the deny provider every fetch fails with the store API's `denied` case and the
    // program reports it through its *own* failure variant.
    let denied =
        compose(&fixtures::store_deny_provider(), &consumer).expect("store.deny $ consumer");
    let outcome = run::run_component(&denied, &[], Providers::none());
    assert_eq!(run::failure_value(&outcome), "storage-denied");
    match &outcome {
        Outcome::Failure(_) => {}
        other => panic!("denial must be an in-band program failure, got {other:?}"),
    }
}

/// SPEC "Capability slots, `rename`, and `with`": `with seven as left, nine as right $ c`
/// — relabel each provider's export slot and compose, so two slots of one interface are
/// served by different providers.
#[test]
fn rename_wires_two_slots_of_one_interface_to_different_providers() {
    let consumer = fixtures::two_answers_consumer();
    let info = consumer.describe();
    let slots: Vec<&str> = info.imports.iter().map(|i| i.slot.as_str()).collect();
    assert!(
        slots.contains(&"left") && slots.contains(&"right"),
        "expected the two named slots, got {slots:?}"
    );

    // A provider under its default slot name matches neither named slot: nothing is wired.
    let unwired = compose(&fixtures::answer_provider(7), &consumer).expect("unwired compose");
    assert_eq!(unwired.describe().imports.len(), info.imports.len());

    // `with seven as left, nine as right $ two-answers` — rename, then compose.
    let left = rename(
        &fixtures::answer_provider(7),
        "eo9-tests:cap/answer",
        "left",
    )
    .expect("rename seven to left");
    let right = rename(
        &fixtures::answer_provider(9),
        "eo9-tests:cap/answer",
        "right",
    )
    .expect("rename nine to right");
    let wired =
        compose(&right, &compose(&left, &consumer).expect("wire left")).expect("wire right");
    assert!(wired.describe().imports.is_empty());

    let outcome = run::run_component(&wired, &[], Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        "709",
        "left and right must be served by different providers (7 * 100 + 9)"
    );
}
