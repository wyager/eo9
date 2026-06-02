//! Two whole TCP/IP stacks over one switched link — the shared-link payoff of the
//! single-owner-NIC design (docs/design/executor-model.md §6, plan/09): one
//! `net.l2.switch` owns the upstream link; each of two `net.l4.over-l2` middlewares
//! rides its own virtual NIC; `vnic4check` completes an independent UDP round-trip on
//! each.
//!
//! ```text
//! entropy.seeded $ time.monotonic-stub $ net.l2.echo
//!   $ rename port-a link-a $ rename port-b link-b $ net.l2.switch
//!   $ (net.l4.over-l2 as left, riding link-a)
//!   $ (net.l4.over-l2 as right, riding link-b, configured 10.0.2.16)
//!   $ vnic4check
//! ```
//!
//! **Platform limit, pinned below:** this composition is well-typed, composes, and
//! compiles, but cannot *run* today. The middleware drives its l2 import eagerly
//! (single-poll, the layered-provider convention), and the switch — itself a guest
//! whose exports make nested guest-to-guest calls to the upstream — does not complete
//! eagerly under the CM-async callback ABI: the middleware's poll sees the suspension
//! and reports its typed `io` error. This is the same nested-forwarding limit user
//! study 09 reproduced with `pci.filtered $ disk.virtio $ fs.eofs` (GAPS, the
//! suspended-subtask caveat) — a second, minimal reproduction. Guest middleware over
//! the switch works the moment that platform limit lifts; the `#[ignore]`d test below
//! is the acceptance run for that day. What works *today* over the switch: consumers
//! that import `l2` directly and await properly (`vnicheck` — see vnic_switch.rs, and
//! the metal `arp` mode), because a properly-awaiting caller absorbs the suspension.

use eo9_component::{Component, compose, configure, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

const COMPONENTS: &[&str] = &[
    "eo9-stub-entropy-seeded",
    "eo9-stub-time-monotonic-stub",
    "eo9-stub-net-l2-echo",
    "eo9-stub-net-l2-switch",
    "eo9-stub-net-l4-over-l2",
    "eo9-example-vnic4check",
];

/// One transport stack for one named slot: the middleware's l4 export renamed onto the
/// program's slot (`left`/`right`), its l2 import renamed onto the switch port's wire
/// name (`link-a`/`link-b`).
fn transport(slot: &str, link: &str, configured_address: Option<&str>) -> Component {
    let stack = if let Some(address) = configured_address {
        configure(
            &guest::load_stub("net.l4.over-l2"),
            &[
                ("address", format!("{address:?}").as_str()),
                ("prefix-length", "24"),
                ("gateway", "\"10.0.2.2\""),
            ],
        )
        .expect("baking a syntactically-valid address succeeds")
    } else {
        guest::load_stub("net.l4.over-l2")
    };
    let stack = rename(&stack, "eo9:net/l4", slot).expect("rename the l4 export onto the slot");
    rename(&stack, "eo9:net/l2", link).expect("rename the l2 import onto the wire")
}

/// The full two-stack composition over the echo fixture.
fn two_stack_composition() -> Component {
    let inner = compose(
        &transport("right", "link-b", Some("10.0.2.16")),
        &guest::load_example("vnic4check"),
    )
    .expect("right transport $ vnic4check");
    let inner = compose(&transport("left", "link-a", None), &inner).expect("left transport $ …");

    let switch = rename(&guest::load_stub("net.l2.switch"), "port-a", "link-a")
        .expect("rename port-a to link-a");
    let switch = rename(&switch, "port-b", "link-b").expect("rename port-b to link-b");
    let stack = compose(&switch, &inner).expect("switch $ …");
    let stack = compose(&guest::load_stub("net.l2.echo"), &stack).expect("net.l2.echo $ …");
    let clock = configure(
        &guest::load_stub("time.monotonic-stub"),
        &[("start-ns", "0"), ("step-ns", "1000000")],
    )
    .expect("baking the stub clock's numbers succeeds");
    let stack = compose(&clock, &stack).expect("time.monotonic-stub $ …");
    compose(&guest::load_stub("entropy.seeded"), &stack).expect("entropy.seeded $ …")
}

fn run_two_stacks() -> Outcome {
    run::run_component(
        &two_stack_composition(),
        &[
            NamedArg::new("peer", "\"10.0.2.2\""),
            NamedArg::new("peer-port", "7777"),
            NamedArg::new("mode", "\"echo\""),
        ],
        Providers::none(),
    )
}

/// The composition composes and compiles, and the platform limit surfaces as the
/// middleware's *typed* error — never a trap, never a hang. (Replace this test with
/// the `#[ignore]`d one below when the nested-forwarding limit lifts.)
#[test]
fn guest_middleware_over_the_switch_hits_the_nested_forwarding_limit_typed() {
    guest::ensure_components(COMPONENTS);
    match run_two_stacks() {
        Outcome::Failure(failure) => {
            assert!(
                failure.value.contains("suspended"),
                "expected the middleware's typed suspended-provider error: {}",
                failure.value
            );
        }
        Outcome::Success(success) => panic!(
            "the nested-forwarding limit appears to have lifted (success: {}) — \
             un-ignore `two_transport_stacks_share_one_link_through_the_switch` and \
             delete this pin",
            success.value
        ),
        other => panic!("expected the middleware's typed failure, got {other:?}"),
    }
}

/// The acceptance run for the day the platform's nested guest-to-guest forwarding
/// limit lifts (GAPS, suspended-subtask): both transport stacks complete independent
/// UDP round-trips through one switched link.
#[test]
#[ignore = "blocked on the nested guest-to-guest forwarding limit (GAPS: suspended-subtask; study 09)"]
fn two_transport_stacks_share_one_link_through_the_switch() {
    guest::ensure_components(COMPONENTS);
    match run_two_stacks() {
        Outcome::Success(success) => {
            assert!(
                success.value.contains("left=echoed") && success.value.contains("right=echoed"),
                "both stacks must complete their round-trip: {}",
                success.value
            );
        }
        other => panic!("expected both stacks to round-trip through the switch, got {other:?}"),
    }
}
