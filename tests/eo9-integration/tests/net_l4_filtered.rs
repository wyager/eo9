//! `net.policy-ports $ net.l4.filtered $ program` — the transport firewall as composed
//! policy ("policies are programs", SPEC: Eo9 API design; docs/design/policy-components.md).
//!
//! `net.l4.filtered` gates every endpoint operation (connect, listen, bind-udp,
//! send-to) through a composed `eo9:net/connection-policy` component;
//! `net.policy-ports` is the standard policy (a port allow-list). These tests run the
//! whole stack over `net.l4.loopback` — no real network anywhere:
//!
//! ```text
//! net.l4.loopback $ net.policy-ports --allow … $ net.l4.filtered $ sockcheck
//! ```
//!
//! and pin: a permissive policy lets the full TCP + UDP conformance check succeed; a
//! restrictive (or unconfigured) policy surfaces as the program's own typed denial; the
//! policy is pure and sealed by composition.

use eo9_component::{Component, compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

/// `net.l4.filtered $ sockcheck`, with the connection-policy and underlying-l4 imports open.
fn filtered_sockcheck() -> Component {
    guest::ensure_components(&["eo9-stub-net-l4-filtered", "eo9-example-sockcheck"]);
    compose(
        &guest::load_stub("net.l4.filtered"),
        &guest::load_example("sockcheck"),
    )
    .expect("net.l4.filtered $ sockcheck must compose")
}

/// Close `policy $ net.l4.filtered $ sockcheck` over the in-memory loopback transport.
fn closed_chain(policy: &Component) -> Component {
    guest::ensure_components(&["eo9-stub-net-l4-loopback"]);
    let chain = compose(policy, &filtered_sockcheck()).expect("policy $ filtered $ sockcheck");
    compose(&guest::load_stub("net.l4.loopback"), &chain).expect("net.l4.loopback $ …")
}

/// The standard ports policy, configured with a WAVE list of ports.
fn ports_policy(allow: &str) -> Component {
    guest::ensure_components(&["eo9-stub-net-policy-ports"]);
    configure(&guest::load_stub("net.policy-ports"), &[("allow", allow)])
        .expect("configure(net.policy-ports, --allow …) must bake")
}

fn run_sockcheck(chain: &Component) -> Outcome {
    run::run_component(
        chain,
        &[NamedArg::new("payload", "\"firewall\"")],
        Providers::none(),
    )
}

#[test]
fn sockcheck_succeeds_through_an_allowing_firewall() {
    // Every endpoint sockcheck touches, as the ports the policy will see. The loopback
    // transport's ephemeral allocation is deterministic (49152, 49153, …, fresh per
    // instance), so the full set is known in advance:
    //   0     — the `any(0)` listen and the two UDP binds (the *requested* port);
    //   1     — sockcheck's dead-port connect probe (it expects connection-refused,
    //           which only the loopback can answer — the firewall must let it through);
    //   49152 — the listener's assigned port: the duplicate-bind probe and both client
    //           connects target it;
    //   49156 — the UDP receiver's assigned port, which send-to targets.
    // (If net.l4.loopback ever changes its ephemeral sequence, update the last two.)
    let chain = closed_chain(&ports_policy("[0, 1, 49152, 49156]"));
    let outcome = run_sockcheck(&chain);
    match &outcome {
        Outcome::Success(success) => assert!(
            success.value.contains("echoed"),
            "expected sockcheck's echoed(...) success: {}",
            success.value
        ),
        other => panic!(
            "sockcheck must fully succeed (TCP echo + UDP round-trip) through an \
             allowing firewall: {other:?}"
        ),
    }
}

#[test]
fn the_firewall_denies_endpoints_outside_the_allow_list() {
    // Nothing sockcheck needs is on the list, so its very first listen is refused with
    // the layer's own `denied` — surfaced as the program's own typed failure.
    let chain = closed_chain(&ports_policy("[9999]"));
    let outcome = run_sockcheck(&chain);
    match &outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected the firewall's denial in the program's failure: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}

#[test]
fn an_unconfigured_policy_denies_everything_and_never_traps() {
    guest::ensure_components(&["eo9-stub-net-policy-ports"]);
    let chain = closed_chain(&guest::load_stub("net.policy-ports"));
    let outcome = run_sockcheck(&chain);
    match &outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected deny-all from the unconfigured policy: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, never a trap: {other:?}"),
    }
}

#[test]
fn the_policy_is_pure_and_the_composition_seals_it() {
    guest::ensure_components(&["eo9-stub-net-policy-ports", "eo9-stub-net-l4-filtered"]);

    // Purity: no capability imports (types-only uses and rt riders are not capabilities).
    let info = guest::load_stub("net.policy-ports").describe();
    assert!(
        info.imports
            .iter()
            .all(|need| need.authority_free || need.interface.starts_with("eo9:rt/")),
        "net.policy-ports must import nothing but types and rt riders: {:?}",
        info.imports
            .iter()
            .map(|n| (n.interface.clone(), n.authority_free))
            .collect::<Vec<_>>()
    );

    // Shape: the policy seals the firewall's connection-policy import; the underlying
    // l4 requirement stays visible.
    let chain = compose(&ports_policy("[80]"), &filtered_sockcheck())
        .expect("policy $ net.l4.filtered $ sockcheck");
    let residual: Vec<String> = chain
        .describe()
        .imports
        .iter()
        .map(|need| need.interface.clone())
        .collect();
    assert!(
        !residual.iter().any(|i| i == "eo9:net/connection-policy"),
        "the connection-policy import must be sealed: {residual:?}"
    );
    assert!(
        residual.iter().any(|i| i == "eo9:net/l4"),
        "the firewall's underlying l4 requirement must remain: {residual:?}"
    );
}
