//! Bridge stacking and the bridge/switch mixed matrix (plan/09 D42).
//!
//! The fan-out payoff: TWO consumers behind a stacked bridge pair both complete
//! request/reply exchanges — the exact shape the switch's MAC-NAT structurally cannot
//! provide (`vnic_stacked.rs` pins that limitation as a typed failure; this suite
//! pins the bridge clearing it). The wiring mirrors the switch's stacking test:
//!
//! ```text
//! net.l2.echo $ rename port-a eo9:net/l2 $ net.l2.bridge
//!   $ rename port-a link-a $ rename port-b link-b $ net.l2.bridge $ bridgecheck
//! ```
//!
//! The mixed matrix, both directions:
//!
//! * **switch under a bridge** (`bridge $ switch $ vnicheck`): WORKS in full — the
//!   switch's rewritten port MACs are just stations to the bridge (learned, demuxed),
//!   and the switch's own drop policy still protects its consumers from the bridge's
//!   floods. The full `vnicheck --mode echo` suite passes verbatim.
//! * **bridge under a switch** (`switch $ bridge $ bridgecheck`): runs, but the
//!   switch's unconditional source rewrite collapses every station behind the bridge
//!   onto ONE switch-port identity — replies return addressed to the switch's port
//!   MAC, which no station behind the bridge ever sourced, so the bridge floods them
//!   to every port. That is the switch doing exactly what its contract says
//!   (identity enforcement); pinned here as bridgecheck's typed check failure so the
//!   composition's behavior is recorded, not discovered.

use eo9_component::{Component, compose, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

const COMPONENTS: &[&str] = &[
    "eo9-stub-net-l2-echo",
    "eo9-stub-net-l2-bridge",
    "eo9-stub-net-l2-switch",
    "eo9-example-bridgecheck",
    "eo9-example-vnicheck",
];

/// Two stacked bridges over the echo fixture, bridgecheck on the outer ports.
fn stacked_bridges() -> Component {
    let outer = guest::load_stub("net.l2.bridge");
    let outer = rename(&outer, "port-a", "link-a").expect("rename outer port-a");
    let outer = rename(&outer, "port-b", "link-b").expect("rename outer port-b");
    let stack = compose(&outer, &guest::load_example("bridgecheck")).expect("outer $ bridgecheck");

    let inner = guest::load_stub("net.l2.bridge");
    let inner =
        rename(&inner, "port-a", "eo9:net/l2").expect("rename inner port-a onto the uplink slot");
    let stack = compose(&inner, &stack).expect("inner $ (outer $ bridgecheck)");

    compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …")
}

/// Stacking holds at the algebra level: a bridge port renames onto the default
/// `eo9:net/l2` slot and satisfies another bridge's uplink, with the unused sibling
/// port dropped — the composition is buildable and sealed end to end.
#[test]
fn a_bridge_port_satisfies_another_bridges_uplink() {
    guest::ensure_components(COMPONENTS);
    let stack = stacked_bridges();
    let info = stack.describe();
    assert!(
        info.imports
            .iter()
            .all(|need| need.interface != "eo9:net/l2"),
        "every l2 slot is sealed through both layers: {:?}",
        info.imports
    );
}

/// THE FAN-OUT PAYOFF: the full 802.1D suite — including BOTH consumers' custom-MAC
/// request/reply exchanges — runs through two stacked bridges. Compare
/// `vnic_stacked.rs`, where the same shape over stacked switches is pinned as a typed
/// failure (the MAC-NAT collapse): no rewrite means the consumers' MACs survive both
/// layers, each layer learns them, and replies demux all the way back down.
#[test]
fn fan_out_runs_through_two_stacked_bridges() {
    guest::ensure_components(COMPONENTS);
    let stack = stacked_bridges();

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
                    .contains("mac-a=02:e0:09:00:01:01 mac-b=02:e0:09:00:01:02"),
                "the outer bridge advertises its default-derived MACs: {}",
                success.value
            );
        }
        other => panic!("expected the stacked fan-out suite to pass, got {other:?}"),
    }
}

/// Switch under a bridge: the switch's uplink rides a bridge port, and the FULL
/// switch policy suite (`vnicheck --mode echo`, including unknown-unicast delivered
/// to NEITHER switch port) passes verbatim — the bridge floods, the switch's demux
/// drops, each provider keeping its own contract through the other.
#[test]
fn a_switch_under_a_bridge_keeps_the_full_switch_policy() {
    guest::ensure_components(COMPONENTS);

    let switch = guest::load_stub("net.l2.switch");
    let switch = rename(&switch, "port-a", "link-a").expect("rename switch port-a");
    let switch = rename(&switch, "port-b", "link-b").expect("rename switch port-b");
    let stack = compose(&switch, &guest::load_example("vnicheck")).expect("switch $ vnicheck");

    let bridge = guest::load_stub("net.l2.bridge");
    let bridge = rename(&bridge, "port-a", "eo9:net/l2")
        .expect("rename bridge port-a onto the switch's uplink slot");
    let stack = compose(&bridge, &stack).expect("bridge $ (switch $ vnicheck)");
    let stack = compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …");

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
                "the switch's own derived MACs and policy hold through the bridge: {}",
                success.value
            );
        }
        other => panic!("expected the full switch suite through a bridge, got {other:?}"),
    }
}

/// Bridge under a switch: composes and runs, but the switch's source rewrite
/// collapses every station behind the bridge onto one switch-port identity, so
/// replies come back addressed to a MAC nobody behind the bridge sourced and are
/// flooded — bridgecheck's first reply assertion fails, typed. This is the switch
/// enforcing its contract, recorded deliberately (the composition order that wants
/// bridge fan-out must put the bridge on top).
#[test]
fn a_bridge_under_a_switch_collapses_identities_typed() {
    guest::ensure_components(COMPONENTS);

    let bridge = guest::load_stub("net.l2.bridge");
    let bridge = rename(&bridge, "port-a", "link-a").expect("rename bridge port-a");
    let bridge = rename(&bridge, "port-b", "link-b").expect("rename bridge port-b");
    let stack =
        compose(&bridge, &guest::load_example("bridgecheck")).expect("bridge $ bridgecheck");

    let switch = guest::load_stub("net.l2.switch");
    let switch = rename(&switch, "port-a", "eo9:net/l2")
        .expect("rename switch port-a onto the bridge's uplink slot");
    let stack = compose(&switch, &stack).expect("switch $ (bridge $ bridgecheck)");
    let stack = compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …");

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", "\"learn\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Failure(failure) => {
            assert!(
                failure
                    .value
                    .contains("the reflected reply must be addressed to the custom source MAC"),
                "expected the identity-collapse check failure (the switch rewrote the \
                 source, so the reply cannot return to the custom MAC): {}",
                failure.value
            );
        }
        other => panic!(
            "expected bridgecheck's typed check failure (the switch's rewrite collapses \
             identities behind the bridge), got {other:?}"
        ),
    }
}
