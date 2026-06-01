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
use eo9_runtime::{NamedArg, Outcome, Providers, SpawnLimits, Task};

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

/// User study 08, finding F1: a malformed configure address must never let the program
/// run. The middleware's own validation is contract-correct -- its `configure` returns
/// `result<l4-impl, string>` and a bad dotted-quad comes back as the typed error -- but
/// the synthesized `eo9:rt/configured.bind` entrypoint cannot carry an error outward
/// (its signature is `func()`), so today the refusal surfaces as a *bind-time trap*
/// whose message names the failing step but discards the validation reason. plan/09
/// Decision 22 records the complete fix (bind grows an error channel; areas 02/03 plus
/// the three executors).
///
/// This test pins the safety property that does hold -- the program never spawns, the
/// failure is attributed to compose-time configuration -- so the gap cannot silently
/// widen into "malformed config runs with defaults". When bind gains its error channel,
/// strengthen the message assertion to require the middleware's own reason text
/// ("not a dotted-quad IPv4 address").
#[test]
fn a_malformed_configure_address_never_lets_the_program_run() {
    guest::ensure_components(&[
        "eo9-stub-entropy-seeded",
        "eo9-stub-time-monotonic-stub",
        "eo9-stub-net-l2-deny",
        "eo9-stub-net-l4-over-l2",
        "eo9-example-l4check",
    ]);

    // The algebra bakes the malformed string happily: "not-an-ip" is a perfectly valid
    // *string*. Only the middleware knows IPv4 semantics, and only at bind time.
    let configured = configure(
        &guest::load_stub("net.l4.over-l2"),
        &[
            ("address", "\"not-an-ip\""),
            ("prefix-length", "24"),
            ("gateway", "\"192.168.7.1\""),
        ],
    )
    .expect("configure() bakes the string; validation is the provider's job at bind time");

    let stack = compose(&configured, &guest::load_example("l4check"))
        .expect("configure(net.l4.over-l2, …) $ l4check");
    let stack = compose(&guest::load_stub("net.l2.deny"), &stack).expect("net.l2.deny $ …");
    let stack =
        compose(&guest::load_stub("time.monotonic-stub"), &stack).expect("time.monotonic-stub $ …");
    let stack = compose(&guest::load_stub("entropy.seeded"), &stack).expect("entropy.seeded $ …");

    let image = run::compile_component(&stack);
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none())
        .map(|_| ())
        .expect_err("a malformed configure address must be refused at bind time, before main");
    let reason = format!("{err:?}");
    assert!(
        reason.contains("configuration") && reason.contains("bind"),
        "the refusal must be attributed to compose-time configuration: {reason}"
    );
}
