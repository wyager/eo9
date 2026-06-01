//! `pci.admit-* $ pci.filtered $ driver` — the policy-component attenuator
//! ("policies are programs", SPEC: Eo9 API design; docs/design/policy-components.md).
//!
//! `pci.filtered` no longer carries its own allow-list configuration: *which devices*
//! is decided by a composed `eo9:pci/admit-policy` component. The standard policies are
//! `pci.admit-address` (fixed bus addresses — the original behavior) and
//! `pci.admit-vendor` (vendor:device identity — stable across boot configs, study 09).
//! These tests pin:
//!
//! * the composition shape (policy seals the middleware's admit-policy import; the
//!   underlying pci requirement and the program's own residuals stay visible),
//! * compound configuration of both standard policies (lists of records bake),
//! * the never-trap rules (an unconfigured policy composes and runs as deny-all),
//! * end-to-end behavior over `pci.deny` in usermode (no host pci provider needed),
//! * purity visibility: the standard policies import nothing but types, and an *impure*
//!   policy's illegitimate imports stay visible as residuals of the composition.

use eo9_component::{Component, compose, configure};
use eo9_integration::{fixtures, guest, run};
use eo9_runtime::{Outcome, Providers};

/// `pci.filtered $ lspci`, with the admit-policy and underlying-pci imports still open.
fn filtered_lspci() -> Component {
    guest::ensure_components(&["eo9-stub-pci-filtered", "eo9-example-lspci"]);
    compose(
        &guest::load_stub("pci.filtered"),
        &guest::load_example("lspci"),
    )
    .expect("pci.filtered $ lspci must compose")
}

/// Seal `chain`'s remaining pci + text imports with `pci.deny` and `text.null`, so the
/// whole composition runs in usermode against no host providers at all.
fn close_over_deny(chain: &Component) -> Component {
    guest::ensure_components(&["eo9-stub-pci-deny", "eo9-stub-text-null"]);
    let sealed = compose(&guest::load_stub("pci.deny"), chain).expect("pci.deny $ …");
    compose(&guest::load_stub("text.null"), &sealed).expect("text.null $ …")
}

/// The interfaces a composition still needs, by name.
fn residual_interfaces(component: &Component) -> Vec<String> {
    component
        .describe()
        .imports
        .iter()
        .map(|need| need.interface.clone())
        .collect()
}

#[test]
fn the_address_policy_seals_the_middleware_and_the_chain_composes() {
    guest::ensure_components(&["eo9-stub-pci-admit-address"]);

    // Configure the standard address policy: a list of records bakes (compound config).
    let policy = configure(
        &guest::load_stub("pci.admit-address"),
        &[("allow", "[{segment: 0, bus: 0, device: 1, function: 0}]")],
    )
    .expect("configure(pci.admit-address, --allow [{…}]) must bake");

    // The configured policy: admit-policy re-exported, config sealed, bind rider carried.
    let info = policy.describe();
    let exports: Vec<&str> = info.exports.iter().map(|e| e.interface.as_str()).collect();
    assert!(
        exports.contains(&"eo9:pci/admit-policy"),
        "the policy interface must be exported: {exports:?}"
    );
    assert!(
        exports.contains(&"eo9:rt/configured"),
        "the bind entrypoint must be carried: {exports:?}"
    );
    assert!(
        !exports.iter().any(|e| e.ends_with("address-admit-config")),
        "the config interface must be sealed away: {exports:?}"
    );

    // policy $ (pci.filtered $ lspci): the middleware's admit-policy import is sealed;
    // its underlying pci requirement and lspci's text remain visible.
    let chain = compose(&policy, &filtered_lspci()).expect("policy $ pci.filtered $ lspci");
    let residual = residual_interfaces(&chain);
    assert!(
        !residual.iter().any(|i| i == "eo9:pci/admit-policy"),
        "the admit-policy import must be sealed by the composed policy: {residual:?}"
    );
    assert!(
        residual.iter().any(|i| i == "eo9:pci/pci"),
        "the attenuator's own underlying pci requirement must remain: {residual:?}"
    );
    assert!(
        residual.iter().any(|i| i == "eo9:text/text"),
        "lspci's text requirement must remain: {residual:?}"
    );
}

#[test]
fn the_vendor_policy_composes_the_same_shape() {
    guest::ensure_components(&["eo9-stub-pci-admit-vendor"]);

    // virtio-net's identity: 0x1af4:0x1000 (decimal 6900:4096).
    let policy = configure(
        &guest::load_stub("pci.admit-vendor"),
        &[("allow", "[{vendor-id: 6900, device-id: 4096}]")],
    )
    .expect("configure(pci.admit-vendor, --allow [{…}]) must bake");

    let chain = compose(&policy, &filtered_lspci()).expect("policy $ pci.filtered $ lspci");
    let residual = residual_interfaces(&chain);
    assert!(
        !residual.iter().any(|i| i == "eo9:pci/admit-policy"),
        "the admit-policy import must be sealed: {residual:?}"
    );
    assert!(
        residual.iter().any(|i| i == "eo9:pci/pci"),
        "the underlying pci requirement must remain: {residual:?}"
    );
}

#[test]
fn the_full_chain_runs_in_usermode_over_pci_deny_and_reports_denied() {
    guest::ensure_components(&["eo9-stub-pci-admit-address"]);

    // A configured policy admitting one address; the underlying provider denies
    // enumeration outright, and that refusal must surface as lspci's own failure.
    let policy = configure(
        &guest::load_stub("pci.admit-address"),
        &[("allow", "[{segment: 0, bus: 0, device: 1, function: 0}]")],
    )
    .expect("configure(pci.admit-address, …)");
    let chain = compose(&policy, &filtered_lspci()).expect("policy $ pci.filtered $ lspci");
    let closed = close_over_deny(&chain);

    let outcome = run::run_component(&closed, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected lspci's own `denied` failure through the policy + middleware chain: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}

#[test]
fn an_unconfigured_policy_composes_and_runs_as_deny_all_never_trapping() {
    guest::ensure_components(&["eo9-stub-pci-admit-address"]);

    // No configure at all: the policy's documented default is "admit nothing".
    // Composition must succeed and the chain must still run (option-C never-trap rule).
    let chain = compose(&guest::load_stub("pci.admit-address"), &filtered_lspci())
        .expect("an unconfigured admit policy must still compose");
    let closed = close_over_deny(&chain);

    let outcome = run::run_component(&closed, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected a typed denied failure (deny-all policy over a denying provider): {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, never a trap: {other:?}"),
    }
}

#[test]
fn the_standard_policies_are_pure_their_imports_carry_no_authority() {
    guest::ensure_components(&["eo9-stub-pci-admit-address", "eo9-stub-pci-admit-vendor"]);

    for stub in ["pci.admit-address", "pci.admit-vendor"] {
        let info = guest::load_stub(stub).describe();
        // Purity: every import is either a types-only use (authority-free) or an
        // `eo9:rt/*` runtime-contract rider (the SDK's panic-report sink — not a
        // capability: it grants nothing and allow-lists never name it; see SPEC).
        // No *capability* import is permitted.
        assert!(
            info.imports
                .iter()
                .all(|need| need.authority_free || need.interface.starts_with("eo9:rt/")),
            "{stub} must import nothing but types and rt riders (purity): {:?}",
            info.imports
                .iter()
                .map(|n| (n.interface.clone(), n.authority_free))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn an_impure_policys_imports_stay_visible_as_residuals() {
    // A hand-built admit policy that (illegitimately) wants entropy. Composing it into
    // the chain must leave the entropy requirement visible in the result's imports —
    // an impure policy can never hide what it needs.
    let impure = fixtures::impure_admit_policy();
    let info = impure.describe();
    assert!(
        info.imports
            .iter()
            .any(|need| need.interface == "eo9:entropy/entropy" && !need.authority_free),
        "the fixture must carry a real (authority-bearing) entropy import: {:?}",
        info.imports
            .iter()
            .map(|n| (n.interface.clone(), n.authority_free))
            .collect::<Vec<_>>()
    );

    let chain = compose(&impure, &filtered_lspci()).expect("impure policy $ pci.filtered $ lspci");
    let residual = residual_interfaces(&chain);
    assert!(
        residual.iter().any(|i| i == "eo9:entropy/entropy"),
        "the impure policy's entropy requirement must stay visible as a residual: {residual:?}"
    );

    // And the pure chain (for contrast) has no entropy residual.
    guest::ensure_components(&["eo9-stub-pci-admit-address"]);
    let pure_chain = compose(&guest::load_stub("pci.admit-address"), &filtered_lspci())
        .expect("pure policy $ pci.filtered $ lspci");
    let pure_residual = residual_interfaces(&pure_chain);
    assert!(
        !pure_residual.iter().any(|i| i == "eo9:entropy/entropy"),
        "the pure policy must add no entropy requirement: {pure_residual:?}"
    );
}
