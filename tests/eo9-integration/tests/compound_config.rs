//! Compose-time baking of compound configuration values, observed end to end (plan/03):
//! `configure(provider, …)` with a list, a list of records carrying strings, a string,
//! an option, and an enum, composed with a consumer and run — the provider's `configure`
//! must observe exactly the values the invoker baked in, and the same arguments must
//! produce byte-identical artifacts.

use eo9_component::{compose, configure};
use eo9_integration::fixtures::{compound_checksum, compound_consumer, compound_provider};
use eo9_integration::run;
use eo9_runtime::Providers;

/// `configure(provider, full args) $ consumer`, run to its outcome.
fn run_configured(args: &[(&str, &str)]) -> String {
    let configured =
        configure(&compound_provider(), args).expect("compound configure should succeed");
    let program = compose(&configured, &compound_consumer()).expect("configured $ consumer");
    assert!(
        program.describe().imports.is_empty(),
        "the composed program should be fully closed"
    );
    let outcome = run::run_component(&program, &[], Providers::none());
    run::success_value(&outcome).to_string()
}

#[test]
fn compound_configuration_reaches_the_provider() {
    let observed = run_configured(&[
        ("thresholds", "[1, 2, 3]"),
        (
            "probes",
            "[{offset: 4, label: \"alpha\"}, {offset: 9, label: \"beta\"}]",
        ),
        ("title", "\"compound\""),
        ("scale", "some(5)"),
        ("mode", "careful"),
    ]);
    let expected = compound_checksum(
        &[1, 2, 3],
        &[(4, "alpha"), (9, "beta")],
        "compound",
        Some(5),
        1,
    );
    assert_eq!(
        observed,
        expected.to_string(),
        "the provider must observe exactly the baked compound values"
    );
}

#[test]
fn empty_lists_and_absent_options_bake_correctly() {
    let observed = run_configured(&[
        ("thresholds", "[]"),
        ("probes", "[]"),
        ("title", "\"\""),
        ("scale", "none"),
        ("mode", "fast"),
    ]);
    let expected = compound_checksum(&[], &[], "", None, 0);
    assert_eq!(observed, expected.to_string());
}

#[test]
fn different_compound_arguments_produce_different_configurations() {
    let first = run_configured(&[
        ("thresholds", "[1, 2, 3]"),
        ("probes", "[{offset: 4, label: \"alpha\"}]"),
        ("title", "\"one\""),
        ("scale", "some(5)"),
        ("mode", "fast"),
    ]);
    let second = run_configured(&[
        ("thresholds", "[3, 2, 1]"),
        ("probes", "[{offset: 4, label: \"alpha\"}]"),
        ("title", "\"one\""),
        ("scale", "some(5)"),
        ("mode", "fast"),
    ]);
    assert_ne!(
        first, second,
        "element order is part of the configuration and must change the observed value"
    );
}

#[test]
fn compound_configuration_is_byte_deterministic() {
    let args: &[(&str, &str)] = &[
        ("thresholds", "[1, 2, 3]"),
        ("probes", "[{offset: 4, label: \"alpha\"}]"),
        ("title", "\"deterministic\""),
        ("scale", "none"),
        ("mode", "careful"),
    ];
    let once = configure(&compound_provider(), args).unwrap();
    let twice = configure(&compound_provider(), args).unwrap();
    assert_eq!(
        once.save(),
        twice.save(),
        "the same compound arguments must produce byte-identical artifacts"
    );
}
