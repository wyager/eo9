//! Harness self-checks: every fixture builds, validates, and classifies as the module
//! kind it claims to be. Failures here mean the harness (not the crates under test) needs
//! attention, so the real suites can assume well-formed fixtures.

use eo9_component::ComponentKind;
use eo9_integration::fixtures;

#[test]
fn consumer_fixtures_are_binaries() {
    for (name, component) in [
        ("answer-consumer", fixtures::answer_consumer()),
        ("two-answers", fixtures::two_answers_consumer()),
        ("optional-consumer", fixtures::optional_consumer()),
        ("storage-consumer", fixtures::storage_consumer()),
        ("text-writer", fixtures::text_writer()),
        ("det", fixtures::det_guest()),
    ] {
        assert_eq!(component.kind(), ComponentKind::Binary, "{name}");
        assert!(
            component.describe().exports.is_empty(),
            "{name} must export no interfaces"
        );
    }
}

#[test]
fn provider_fixtures_are_providers() {
    for (name, component) in [
        ("answer-provider", fixtures::answer_provider(7)),
        (
            "optional-provider (present)",
            fixtures::optional_provider_present(5),
        ),
        (
            "optional-provider (absent)",
            fixtures::optional_provider_absent(),
        ),
        ("store-deny", fixtures::store_deny_provider()),
        ("store-ok", fixtures::store_ok_provider(11)),
        ("text-sink", fixtures::text_sink_provider()),
    ] {
        assert_eq!(component.kind(), ComponentKind::Provider, "{name}");
        assert!(
            component.describe().imports.is_empty(),
            "{name} must have no residual imports"
        );
        assert!(
            !component.describe().exports.is_empty(),
            "{name} must export at least one interface"
        );
    }
}

#[test]
fn the_sleeper_wat_is_valid_component_text() {
    // Compiled directly by the runtime (WAT in, image out); just check it parses here.
    wat::parse_str(fixtures::sleeper_wat()).expect("sleeper fixture must be valid WAT");
}
