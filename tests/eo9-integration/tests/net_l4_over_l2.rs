//! The TCP/IP middleware (`net.l4.over-l2`) exercised through the algebra with no real
//! network anywhere: composed over `net.l2.deny`, the link layer's refusal must surface
//! through the whole stack — middleware, then the l4-speaking program — as the
//! program's own typed failure, never a trap and never a hang. The clock and entropy
//! the middleware needs are ordinary stub providers in the same composition, so the
//! whole chain is plain `$` over shipped components:
//!
//! ```text
//! entropy.seeded $ time.monotonic-stub $ net.l2.deny $ net.l4.over-l2 $ l4check
//! ```

use eo9_component::{compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::{Outcome, Providers};

#[test]
fn deny_at_l2_surfaces_through_the_middleware_as_the_programs_own_failure() {
    guest::ensure_components(&[
        "eo9-stub-entropy-seeded",
        "eo9-stub-time-monotonic-stub",
        "eo9-stub-net-l2-deny",
        "eo9-stub-net-l4-over-l2",
        "eo9-example-l4check",
    ]);

    // Right-associative: each provider seals the imports of everything to its right.
    let stack = compose(
        &guest::load_stub("net.l4.over-l2"),
        &guest::load_example("l4check"),
    )
    .expect("net.l4.over-l2 $ l4check");
    let stack = compose(&guest::load_stub("net.l2.deny"), &stack).expect("net.l2.deny $ …");
    let stack =
        compose(&guest::load_stub("time.monotonic-stub"), &stack).expect("time.monotonic-stub $ …");
    let stack = compose(&guest::load_stub("entropy.seeded"), &stack).expect("entropy.seeded $ …");

    let outcome = run::run_component(&stack, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected the link layer's refusal in the program's own failure value: {}",
            failure.value
        ),
        other => panic!("expected the program's own typed failure, got {other:?}"),
    }
}

/// The middleware now ships an `eo9:net/l4-over-l2-config` entry (address, prefix length,
/// gateway), but actually *baking* that configuration through `configure(…)` is blocked on
/// the parked compose-time-configuration design for resource-owning API providers
/// (plan/03 D13): `eo9:net/l4` declares its own resources, and the binder refuses such
/// providers with a typed error today. This test pins that refusal — the configure attempt
/// must fail with the documented message, never trap, and the unconfigured default form
/// (the test above) must keep working. When the binder learns resource-owning providers,
/// this test fails and gets upgraded to a behavioural one.
#[test]
fn configuring_the_middleware_is_refused_typed_until_the_binder_learns_resource_apis() {
    guest::ensure_components(&["eo9-stub-net-l4-over-l2"]);

    let result = configure(
        &guest::load_stub("net.l4.over-l2"),
        &[
            ("address", "\"192.168.7.2\""),
            ("prefix-length", "24"),
            ("gateway", "\"192.168.7.1\""),
        ],
    );
    let message = format!("{:?}", result.expect_err("configure must be refused, not baked"));
    assert!(
        message.contains("defines its own resources"),
        "expected the documented resource-owning-provider refusal, got: {message}"
    );
}
