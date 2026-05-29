//! The layered net API exercised through the algebra: `net.l4.loopback $ sockcheck`
//! runs a real TCP both-ways echo and a UDP round-trip entirely inside the in-memory
//! transport stub (no lower layers anywhere), and `net.l4.deny $ sockcheck` shows the
//! same program failing in the layer's own vocabulary instead. Both compositions are
//! plain `$` over real shipped components — no configuration, no host-side providers
//! beyond the implicit buffer support.

use eo9_component::compose;
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

#[test]
fn loopback_l4_round_trips_tcp_and_udp_through_the_algebra() {
    guest::ensure_components(&["eo9-stub-net-l4-loopback", "eo9-example-sockcheck"]);
    let program = compose(
        &guest::load_stub("net.l4.loopback"),
        &guest::load_example("sockcheck"),
    )
    .expect("net.l4.loopback $ sockcheck");

    let outcome = run::run_component(
        &program,
        &[NamedArg::new("payload", "\"ping pong\"")],
        Providers::none(),
    );
    // Two TCP legs plus one UDP leg, nine payload bytes each.
    assert_eq!(run::success_value(&outcome), "echoed(27)");
}

#[test]
fn deny_l4_refuses_in_the_layers_own_vocabulary() {
    guest::ensure_components(&["eo9-stub-net-l4-deny", "eo9-example-sockcheck"]);
    let program = compose(
        &guest::load_stub("net.l4.deny"),
        &guest::load_example("sockcheck"),
    )
    .expect("net.l4.deny $ sockcheck");

    let outcome = run::run_component(
        &program,
        &[NamedArg::new("payload", "\"ping\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.contains("Denied"),
            "expected the l4 deny error inside the program's failure value: {}",
            failure.value
        ),
        other => panic!("expected the program's own failure, got {other:?}"),
    }
}
