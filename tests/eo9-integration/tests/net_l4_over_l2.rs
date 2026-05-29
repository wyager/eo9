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

use eo9_component::compose;
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
