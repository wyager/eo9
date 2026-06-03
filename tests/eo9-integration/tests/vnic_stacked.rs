//! Switch-over-switch stacking (plan/09 D30: "more consumers stack switches
//! (`switch $ switch $ …`)") — the retest after the async-first conversion.
//!
//! The wiring: the inner switch's `port-a` renamed onto the default `eo9:net/l2`
//! slot becomes the outer switch's uplink; the echo fixture seals the inner uplink:
//!
//! ```text
//! net.l2.echo $ rename port-a eo9:net/l2 $ net.l2.switch
//!   $ rename port-a link-a $ rename port-b link-b $ net.l2.switch $ vnicheck
//! ```
//!
//! Status, pinned (2026-06-02): the stack composes, compiles, spawns, and **runs to a
//! typed program outcome** — one layer further than the pre-conversion pin (the
//! middleware's typed `io` failure happened at the *vnicheck-over-switch* edge before
//! honest awaits; that edge now awaits and works). The residual wall is the switch's
//! OWN `eager()` single-poll of its uplink (the D31 conversion covered `net.virtio`
//! and `net.l4.over-l2`, not `net.l2.switch`): over a leaf upstream (echo, a driver)
//! the eager poll completes, which is why `vnic_switch.rs` passes — but an inner
//! *switch* is a nested-guest-caller whose exports suspend, so the outer switch's
//! single-poll of `list-interfaces` reports its typed suspension error. The fix is
//! the established pattern (delete `eager()`, await the uplink); recorded in plan/09.
//!
//! Note for whoever does that conversion: awaits alone will not make THIS test's
//! full vnicheck echo suite pass. The switch's unconditional source rewrite collapses
//! both outer ports onto the inner port's single MAC on the way up (a MAC-NAT with no
//! reverse mapping), so the echo's replies can demux back to at most one outer port —
//! the stacked fan-out story needs a policy decision, not just async plumbing.

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

/// The behavioral pin: the stacked composition RUNS — no spawn refusal, no trap —
/// and fails in the program's own typed vocabulary with the outer switch's
/// suspension report from its first uplink operation. This is the residual wall
/// (the switch's own `eager()` poll), not the pre-conversion middleware wall;
/// when `net.l2.switch` awaits its uplink honestly, this test's expectation is
/// the part to revisit (see the module docs for the MAC-rewrite caveat).
#[test]
fn stacked_switches_run_to_the_switchs_own_typed_suspension_error() {
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
                    .contains("list-interfaces: the upstream l2 provider suspended"),
                "expected the outer switch's eager-poll suspension report in the \
                 program's own typed failure: {}",
                failure.value
            );
        }
        other => panic!(
            "expected the program's own typed failure (the residual eager() wall), \
             got {other:?}"
        ),
    }
}
