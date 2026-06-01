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
use eo9_runtime::{NamedArg, Outcome, Providers, SpawnError, SpawnLimits, Task};

/// The full denied-link composition over an already-configured middleware, ready to
/// compile and spawn.
fn configured_stack(address: &str) -> eo9_component::Component {
    let address_literal = format!("{address:?}");
    let configured = configure(
        &guest::load_stub("net.l4.over-l2"),
        &[
            ("address", address_literal.as_str()),
            ("prefix-length", "24"),
            ("gateway", "\"192.168.7.1\""),
        ],
    )
    .expect("baking a syntactically-valid string succeeds; address validation is the provider's");
    let stack = compose(&configured, &guest::load_example("l4check"))
        .expect("configure(net.l4.over-l2, …) $ l4check");
    let stack = compose(&guest::load_stub("net.l2.deny"), &stack).expect("net.l2.deny $ …");
    let stack =
        compose(&guest::load_stub("time.monotonic-stub"), &stack).expect("time.monotonic-stub $ …");
    compose(&guest::load_stub("entropy.seeded"), &stack).expect("entropy.seeded $ …")
}

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

/// The same denied-link stack, but through the middleware's *listen* path: sockcheck's
/// first transport operation is `listen` (then `accept` would follow), so this pins that
/// the middleware's server-side surface — not just `connect`/DNS like `l4check` — turns a
/// dead link into the program's own typed failure rather than a hang or a trap.
#[test]
fn the_middlewares_listen_path_over_a_denied_link_is_refused_typed() {
    guest::ensure_components(&[
        "eo9-stub-entropy-seeded",
        "eo9-stub-time-monotonic-stub",
        "eo9-stub-net-l2-deny",
        "eo9-stub-net-l4-over-l2",
        "eo9-example-sockcheck",
    ]);

    let stack = compose(
        &guest::load_stub("net.l4.over-l2"),
        &guest::load_example("sockcheck"),
    )
    .expect("net.l4.over-l2 $ sockcheck");
    let stack = compose(&guest::load_stub("net.l2.deny"), &stack).expect("net.l2.deny $ …");
    let stack =
        compose(&guest::load_stub("time.monotonic-stub"), &stack).expect("time.monotonic-stub $ …");
    let stack = compose(&guest::load_stub("entropy.seeded"), &stack).expect("entropy.seeded $ …");

    let outcome = run::run_component(
        &stack,
        &[NamedArg::new("payload", "\"ping\"")],
        Providers::none(),
    );
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected the link layer's refusal out of the middleware's listen path: {}",
            failure.value
        ),
        other => panic!("expected the program's own typed failure, got {other:?}"),
    }
}

/// The middleware's `eo9:net/l4-over-l2-config` (address, prefix length, gateway) bakes
/// through `configure(…)` under the alias + bind construction (plan/03 D21): `eo9:net/l4`
/// owning its own resources no longer matters, because the configured wrapper re-exports
/// the API by direct alias and only the configuration call goes through the synthesized
/// `eo9:rt/configured.bind` entrypoint. This test is the acceptance check for that design:
/// the configured middleware composes, and the configured chain over `net.l2.deny` still
/// surfaces the link layer's refusal as the program's own typed failure -- proving the
/// configuration baked (and was applied at spawn) without breaking the chain.
#[test]
fn configuring_the_middleware_bakes_and_the_configured_chain_still_runs() {
    guest::ensure_components(&[
        "eo9-stub-entropy-seeded",
        "eo9-stub-time-monotonic-stub",
        "eo9-stub-net-l2-deny",
        "eo9-stub-net-l4-over-l2",
        "eo9-example-l4check",
    ]);

    let configured = configure(
        &guest::load_stub("net.l4.over-l2"),
        &[
            ("address", "\"192.168.7.2\""),
            ("prefix-length", "24"),
            ("gateway", "\"192.168.7.1\""),
        ],
    )
    .expect("configure(net.l4.over-l2, address/prefix/gateway) must bake under alias + bind");

    // The configured provider carries the bind entrypoint and still exports l4.
    let info = configured.describe();
    let exports: Vec<&str> = info.exports.iter().map(|e| e.interface.as_str()).collect();
    assert!(exports.contains(&"eo9:net/l4"), "{exports:?}");
    assert!(exports.contains(&"eo9:rt/configured"), "{exports:?}");
    assert!(
        !exports.contains(&"eo9:net/l4-over-l2-config"),
        "the config interface must be sealed away: {exports:?}"
    );

    // The configured chain composes and runs: deny at l2 still surfaces as the program's
    // own typed failure (the configured addressing does not change that), never a trap.
    let stack = compose(&configured, &guest::load_example("l4check"))
        .expect("configure(net.l4.over-l2, …) $ l4check");
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

/// A malformed configured address is a **typed pre-run refusal**, never a trap (user
/// study 08 finding F1; plan/03 D23's error channel). Baking succeeds -- "not-an-ip" is
/// a perfectly valid WIT string -- and the provider's own validation rejects it when the
/// executor applies the configuration: `eo9:rt/configured.bind` returns the provider's
/// error, the spawn is refused with `SpawnError::ConfigurationRefused`, and the program
/// is never entered.
#[test]
fn a_malformed_configure_address_is_a_typed_refusal_not_a_trap() {
    guest::ensure_components(&[
        "eo9-stub-entropy-seeded",
        "eo9-stub-time-monotonic-stub",
        "eo9-stub-net-l2-deny",
        "eo9-stub-net-l4-over-l2",
        "eo9-example-l4check",
    ]);

    let stack = configured_stack("not-an-ip");
    let image = run::compile_component(&stack);
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none())
        .expect_err("a malformed configured address must refuse the spawn");
    match err {
        SpawnError::ConfigurationRefused(reason) => {
            assert!(
                reason.contains("not a dotted-quad IPv4 address"),
                "the refusal must carry the provider's own validation message: {reason}"
            );
            assert!(
                reason.contains("not-an-ip"),
                "the refusal must name the rejected value: {reason}"
            );
        }
        other => panic!("expected ConfigurationRefused, got: {other}"),
    }
}

/// The same malformed configuration, but reached through a **merged** bind entrypoint:
/// the outer compose's provider (`entropy.seeded`, valid config) and the inner
/// configured middleware (bad address) both carry `eo9:rt/configured`, so the executor
/// calls one merged `bind` -- provider first (succeeds), then the inner one (refuses).
/// The merger must propagate that error as its own, so the spawn is still the typed
/// refusal, never a trap.
#[test]
fn a_merged_bind_propagates_the_nested_configure_refusal() {
    guest::ensure_components(&[
        "eo9-stub-entropy-seeded",
        "eo9-stub-time-monotonic-stub",
        "eo9-stub-net-l2-deny",
        "eo9-stub-net-l4-over-l2",
        "eo9-example-l4check",
    ]);

    // Inner: the bad-address middleware over l4check (carries the middleware's bind).
    let bad = configure(
        &guest::load_stub("net.l4.over-l2"),
        &[
            ("address", "\"999.0.0.1\""),
            ("prefix-length", "24"),
            ("gateway", "\"192.168.7.1\""),
        ],
    )
    .expect("baking succeeds; validation is the provider's");
    let inner = compose(&bad, &guest::load_example("l4check")).expect("bad-l4 $ l4check");
    let inner = compose(&guest::load_stub("net.l2.deny"), &inner).expect("net.l2.deny $ …");
    let inner =
        compose(&guest::load_stub("time.monotonic-stub"), &inner).expect("time.monotonic-stub $ …");

    // Outer: a *configured* entropy provider over the inner stack -- both operands carry
    // a bind entrypoint, so this compose synthesizes the bind merger.
    let seeded = configure(&guest::load_stub("entropy.seeded"), &[("seed", "42")])
        .expect("configure(entropy.seeded, seed)");
    let stack = compose(&seeded, &inner).expect("configured-entropy $ …");

    let image = run::compile_component(&stack);
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none())
        .expect_err("the nested configure refusal must refuse the spawn through the merger");
    match err {
        SpawnError::ConfigurationRefused(reason) => assert!(
            reason.contains("not a dotted-quad IPv4 address"),
            "the merger must propagate the nested provider's own message: {reason}"
        ),
        other => panic!("expected ConfigurationRefused, got: {other}"),
    }
}
