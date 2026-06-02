//! `net.l2.switch` — the virtual-NIC switch (the single-owner-NIC sharing story:
//! docs/design/executor-model.md §6, plan/09).
//!
//! The switch imports ONE upstream `eo9:net/l2` and exports two virtual NICs as the
//! named ports `port-a` / `port-b`. These tests verify the component's shape and the
//! whole observable switching policy behaviorally, with the `net.l2.echo` fixture as
//! the upstream and `vnicheck` driving both ports:
//!
//! ```text
//! net.l2.echo $ rename port-a link-a $ rename port-b link-b $ net.l2.switch
//!   $ vnicheck
//! ```
//!
//! One `$` instantiates the switch once and wires that single instance to both of the
//! consumer side's named slots — the sharing is real, not two switches.

use eo9_component::{Component, compose, configure, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers, SpawnError, SpawnLimits, Task};

const COMPONENTS: &[&str] = &[
    "eo9-stub-net-l2-echo",
    "eo9-stub-net-l2-switch",
    "eo9-example-vnicheck",
];

/// The full echo-backed two-port composition: the switch's ports renamed onto
/// vnicheck's `link-a` / `link-b` slots, the echo fixture sealing the uplink.
fn switched_stack(switch: &Component) -> Component {
    let switch = rename(switch, "port-a", "link-a").expect("rename port-a to link-a");
    let switch = rename(&switch, "port-b", "link-b").expect("rename port-b to link-b");
    let stack = compose(&switch, &guest::load_example("vnicheck")).expect("switch $ vnicheck");
    compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …")
}

/// The switch component has the documented surface: one default-named uplink import,
/// two named same-interface port exports (each its own slot), and the config entry.
#[test]
fn switch_component_exposes_two_ports_and_one_uplink() {
    guest::ensure_components(&["eo9-stub-net-l2-switch"]);
    let switch = guest::load_stub("net.l2.switch");
    let info = switch.describe();

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

    // Renaming a port is the wiring building block (`rename port-a link-a $ …`).
    rename(&switch, "port-a", "link-a").expect("the port-a export should be renameable");
}

/// The behavioral contract, end to end over the echo fixture: per-port
/// locally-administered MACs (the documented default derivation), source rewrite on
/// send, own-unicast demux with sibling isolation both ways, broadcast delivery to
/// both ports, and unknown-unicast dropped — all asserted inside `vnicheck`, whose
/// typed success carries the two MACs.
#[test]
fn switching_policy_verified_over_two_ports() {
    guest::ensure_components(COMPONENTS);
    let stack = switched_stack(&guest::load_stub("net.l2.switch"));

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", "\"echo\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Success(success) => {
            assert!(
                success
                    .value
                    .contains("mac-a=02:e0:09:00:00:01 mac-b=02:e0:09:00:00:02"),
                "the ports must carry the documented default-derived MACs: {}",
                success.value
            );
        }
        other => panic!("expected vnicheck to verify the switching policy, got {other:?}"),
    }
}

/// A configured MAC base derives the port MACs deterministically (base+1, base+2 in
/// the last octet) — the compound-config bake plus the bind entrypoint, end to end.
#[test]
fn a_configured_mac_base_derives_the_port_macs() {
    guest::ensure_components(COMPONENTS);
    let configured = configure(
        &guest::load_stub("net.l2.switch"),
        &[("mac-base", "\"06:00:00:00:00:10\"")],
    )
    .expect("baking a syntactically-valid string succeeds");
    let stack = switched_stack(&configured);

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", "\"echo\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Success(success) => {
            assert!(
                success
                    .value
                    .contains("mac-a=06:00:00:00:00:11 mac-b=06:00:00:00:00:12"),
                "the configured base must derive the port MACs: {}",
                success.value
            );
        }
        other => panic!("expected the configured switch to verify, got {other:?}"),
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
        let configured = configure(&guest::load_stub("net.l2.switch"), &[("mac-base", base)])
            .expect("baking a syntactically-valid string succeeds");
        let stack = switched_stack(&configured);
        let image = run::compile_component(&stack);
        let err = Task::spawn(
            &image,
            &[NamedArg::new("mode", "\"echo\"")],
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
