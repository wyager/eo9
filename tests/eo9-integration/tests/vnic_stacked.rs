//! Switch-over-switch stacking (plan/09 D30: "more consumers stack switches
//! (`switch $ switch $ …`)") — green since the switch's own async-first conversion.
//!
//! The wiring: the inner switch's `port-a` renamed onto the default `eo9:net/l2`
//! slot becomes the outer switch's uplink; the echo fixture seals the inner uplink:
//!
//! ```text
//! net.l2.echo $ rename port-a eo9:net/l2 $ net.l2.switch
//!   $ rename port-a link-a $ rename port-b link-b $ net.l2.switch $ vnicheck
//! ```
//!
//! What works through a two-layer stack (the point-to-point story, `--mode through`):
//! a port-A exchange end to end — the reflect probe goes up through BOTH source
//! rewrites, the reply demuxes back down through both layers (the deterministic MAC
//! derivation makes layer N's port-a MAC equal layer N+1's, so the return path
//! composes), sibling isolation holds through the stack, broadcast fans out to both
//! outer ports, and unknown unicast reaches neither.
//!
//! What does NOT work, pinned typed below: port B's own exchange (`--mode echo`).
//! The switch's unconditional source rewrite collapses both outer ports onto the
//! inner port's single MAC on the way up — a MAC-NAT with no reverse mapping — so
//! replies can demux back to at most one outer port. Whether the switch grows a
//! per-flow reverse mapping or stacking stays point-to-point is the owner's open
//! policy question (plan/09 D37); this pin flips when it's answered.

use eo9_component::{Component, compose, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

const COMPONENTS: &[&str] = &[
    "eo9-stub-net-l2-echo",
    "eo9-stub-net-l2-switch",
    "eo9-example-vnicheck",
];

/// The two-layer stack: echo sealing the inner switch, the inner switch's `port-a`
/// feeding the outer switch's uplink, the outer switch's ports on vnicheck's links.
fn stacked_stack() -> Component {
    let outer = guest::load_stub("net.l2.switch");
    let outer = rename(&outer, "port-a", "link-a").expect("rename outer port-a");
    let outer = rename(&outer, "port-b", "link-b").expect("rename outer port-b");
    let stack = compose(&outer, &guest::load_example("vnicheck")).expect("outer $ vnicheck");

    let inner = guest::load_stub("net.l2.switch");
    let inner =
        rename(&inner, "port-a", "eo9:net/l2").expect("rename inner port-a onto the uplink slot");
    let stack = compose(&inner, &stack).expect("inner $ (outer $ vnicheck)");

    compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …")
}

/// Stacking holds at the algebra level: a port export renames onto the default
/// `eo9:net/l2` slot and one switch's port satisfies another switch's uplink, with
/// the unused sibling port dropped — the composition is buildable end to end.
#[test]
fn a_switch_port_satisfies_another_switchs_uplink() {
    guest::ensure_components(COMPONENTS);
    let stack = stacked_stack();
    let info = stack.describe();
    assert!(
        info.imports
            .iter()
            .all(|need| need.interface != "eo9:net/l2"),
        "every l2 slot is sealed through both layers: {:?}",
        info.imports
    );
}

/// The point-to-point payoff: a full port-A exchange RUNS through two stacked
/// switches — both layers' source rewrites on the way up (the upstream-seen source
/// is the inner layer's port-a MAC, which the deterministic derivation makes equal
/// to the outer port-a MAC the program checks), demux back down through both layers,
/// sibling isolation through the stack, broadcast to both outer ports, unknown
/// unicast to neither.
#[test]
fn a_port_a_exchange_runs_through_two_stacked_switches() {
    guest::ensure_components(COMPONENTS);
    let stack = stacked_stack();

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", "\"through\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Success(success) => {
            assert!(
                success.value.contains("mac-a=02:e0:09:00:00:01")
                    && success.value.contains("mac-b=02:e0:09:00:00:02"),
                "the outer ports derive base+1/+2 from the default base: {}",
                success.value
            );
        }
        other => panic!("expected the stacked point-to-point suite to pass, got {other:?}"),
    }
}

/// The fan-out limitation, pinned typed (never a trap): port B's own exchange cannot
/// return through the stack — the source rewrite collapsed it onto the inner port's
/// MAC, which demuxes back to port A's derivation, so B's reply never arrives. This
/// is the MAC-NAT/reverse-mapping policy question (plan/09 D37); flip this pin when
/// the owner answers it.
#[test]
fn the_stacked_fan_out_limitation_is_a_typed_check_failure() {
    guest::ensure_components(COMPONENTS);
    let stack = stacked_stack();

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("mode", "\"echo\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Failure(failure) => {
            assert!(
                failure
                    .value
                    .contains("link-b: the reflected unicast never arrived"),
                "expected port B's missing return path in the program's own typed \
                 failure: {}",
                failure.value
            );
        }
        other => panic!(
            "expected the program's own typed failure (the MAC-NAT fan-out limit), \
             got {other:?}"
        ),
    }
}
