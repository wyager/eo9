//! `net.l2.bridge` — the 802.1D learning bridge (plan/09 D42: the trusting sibling of
//! `net.l2.switch`; the provider CHOICE — rewrite-and-isolate vs. trust-and-bridge —
//! is the composer's capability decision).
//!
//! These tests verify the component's shape and the whole observable bridging policy
//! behaviorally, with the `net.l2.echo` fixture as the upstream and `bridgecheck`
//! driving both ports:
//!
//! ```text
//! net.l2.echo $ rename port-a link-a $ rename port-b link-b $ net.l2.bridge
//!   $ bridgecheck
//! ```
//!
//! The policy suite (`--mode learn`) covers: NO source rewrite (custom consumer MACs
//! reach the upstream verbatim), flooding before learning and one-way unicast after,
//! local port-to-port delivery the upstream never sees, broadcast to every other
//! port, unknown-unicast FLOODING (the deliberate opposite of the switch's drop
//! policy — `vnicheck` pins the switch delivering the same probe to NEITHER port),
//! and MAC migration. The table modes pin the bounded learning table's
//! least-recently-learned eviction in both directions.

use eo9_component::{compose, configure, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers, SpawnError, SpawnLimits, Task};

const COMPONENTS: &[&str] = &[
    "eo9-stub-net-l2-echo",
    "eo9-stub-net-l2-bridge",
    "eo9-example-bridgecheck",
];

/// The full echo-backed two-port composition: the bridge's ports renamed onto
/// bridgecheck's `link-a` / `link-b` slots, the echo fixture sealing the uplink.
fn bridged_stack(bridge: &eo9_component::Component) -> eo9_component::Component {
    let bridge = rename(bridge, "port-a", "link-a").expect("rename port-a to link-a");
    let bridge = rename(&bridge, "port-b", "link-b").expect("rename port-b to link-b");
    let stack =
        compose(&bridge, &guest::load_example("bridgecheck")).expect("bridge $ bridgecheck");
    compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …")
}

/// Run one bridgecheck mode over the default-configured bridge and return the
/// verified payload, panicking with the failure otherwise.
fn run_mode(mode: &str) -> String {
    guest::ensure_components(COMPONENTS);
    let stack = bridged_stack(&guest::load_stub("net.l2.bridge"));
    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", format!("\"{mode}\""))],
        Providers::none(),
    );
    match outcome {
        Outcome::Success(success) => success.value,
        other => panic!("expected bridgecheck --mode {mode} to verify, got {other:?}"),
    }
}

/// The bridge component has the documented surface: one default-named uplink import,
/// two named same-interface port exports, and the config entry — the same wiring
/// shape as the switch, so compositions swap one provider for the other freely.
#[test]
fn bridge_component_exposes_two_ports_and_one_uplink() {
    guest::ensure_components(&["eo9-stub-net-l2-bridge"]);
    let bridge = guest::load_stub("net.l2.bridge");
    let info = bridge.describe();

    let uplinks: Vec<_> = info
        .imports
        .iter()
        .filter(|need| need.interface == "eo9:net/l2")
        .collect();
    assert_eq!(
        uplinks.len(),
        1,
        "exactly one uplink l2 import is expected: {uplinks:?}"
    );
    assert_eq!(
        uplinks[0].slot, "eo9:net/l2",
        "the uplink is the default-named slot"
    );
    assert!(uplinks[0].required, "the uplink is a real capability ask");

    let port_slots: Vec<(&str, &str)> = info
        .exports
        .iter()
        .map(|export| (export.name.as_str(), export.interface.as_str()))
        .collect();
    assert!(
        port_slots.contains(&("port-a", "eo9:net/l2")),
        "missing the named port-a l2 export: {port_slots:?}"
    );
    assert!(
        port_slots.contains(&("port-b", "eo9:net/l2")),
        "missing the named port-b l2 export: {port_slots:?}"
    );

    rename(&bridge, "port-a", "link-a").expect("the port-a export should be renameable");
}

/// The full 802.1D policy suite, end to end over the echo fixture — every assertion
/// lives inside `bridgecheck` (no rewrite, flood-then-learn, local delivery,
/// broadcast and unknown-unicast flooding, migration); its typed success carries the
/// advertised default-derived MACs.
#[test]
fn the_bridging_policy_verified_over_two_ports() {
    let value = run_mode("learn");
    assert!(
        value.contains("mac-a=02:e0:09:00:01:01 mac-b=02:e0:09:00:01:02"),
        "the ports must advertise the documented default-derived MACs \
         (distinct from the switch's defaults): {value}",
    );
}

/// The 65th distinct source MAC evicts the least-recently-learned entry — observable
/// because the probe addressed to the evicted MAC floods to the upstream.
#[test]
fn the_65th_mac_evicts_the_least_recently_learned_entry() {
    let value = run_mode("evict");
    assert!(
        value.contains("evicted"),
        "the eviction probe must reach the upstream: {value}"
    );
}

/// The control: 64 distinct sources fit, the oldest entry survives, and the probe
/// addressed to it stays local (the upstream never sees it).
#[test]
fn a_full_table_retains_the_oldest_entry_under_lru() {
    let value = run_mode("keep");
    assert!(
        value.contains("retained"),
        "the retained target's probe must stay local: {value}"
    );
}

/// A configured MAC base derives the ADVERTISED port MACs deterministically (base+1,
/// base+2 in the last octet) — the compound-config bake plus the bind entrypoint.
#[test]
fn a_configured_mac_base_derives_the_advertised_macs() {
    guest::ensure_components(COMPONENTS);
    let configured = configure(
        &guest::load_stub("net.l2.bridge"),
        &[("mac-base", "\"06:00:00:00:00:20\"")],
    )
    .expect("baking a syntactically-valid string succeeds");
    let stack = bridged_stack(&configured);

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", "\"learn\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Success(success) => {
            assert!(
                success
                    .value
                    .contains("mac-a=06:00:00:00:00:21 mac-b=06:00:00:00:00:22"),
                "the configured base must derive the advertised MACs: {}",
                success.value
            );
        }
        other => panic!("expected the configured bridge to verify, got {other:?}"),
    }
}

/// A malformed (or non-locally-administered) MAC base is a typed pre-run refusal
/// carrying the provider's own message — never a trap (the configure contract).
#[test]
fn a_bad_mac_base_is_a_typed_refusal_not_a_trap() {
    guest::ensure_components(COMPONENTS);
    for (base, expected) in [
        ("\"not-a-mac\"", "not a colon-separated MAC address"),
        ("\"00:11:22:33:44:55\"", "locally administered"),
    ] {
        let configured = configure(&guest::load_stub("net.l2.bridge"), &[("mac-base", base)])
            .expect("baking a syntactically-valid string succeeds");
        let stack = bridged_stack(&configured);
        let image = run::compile_component(&stack);
        let err = Task::spawn(
            &image,
            &[NamedArg::new("mode", "\"learn\"")],
            SpawnLimits::default(),
            Providers::none(),
        )
        .expect_err("a bad MAC base must refuse the spawn");
        match err {
            SpawnError::ConfigurationRefused(reason) => assert!(
                reason.contains(expected),
                "the refusal must carry the provider's own message ({expected}): {reason}"
            ),
            other => panic!("expected ConfigurationRefused, got: {other}"),
        }
    }
}
