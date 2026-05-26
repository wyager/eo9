//! The invoker-configured deterministic environment — the compose-time `configure`
//! payoff. The real stub provider components (`time.frozen`, `entropy.seeded`, built by
//! `xtask build-guest`) are configured by the *invoker* through the algebra's
//! `configure` operation: the fixture guest imports no config interfaces and performs no
//! configuration of its own, yet observes the frozen clock (including its async `sleep`,
//! which exercises the binder's async forwarding at run time) and the seeded entropy.
//! Repeated runs are byte-identical and the composed environment is sealed against the
//! ambient root providers.

use eo9_component::{Component, compose, configure, extend};
use eo9_integration::{fixtures, guest, run};
use eo9_runtime::providers::{CaptureText, FrozenTime, SeededEntropy};
use eo9_runtime::{Outcome, Providers};

/// The stub components this suite configures and composes.
const STUBS: &[&str] = &["eo9-stub-time-frozen", "eo9-stub-entropy-seeded"];

/// The wall-clock seconds the invoker binds into `time.frozen`.
const FROZEN_SECONDS: i64 = 4242;
/// The monotonic reading the invoker binds into `time.frozen`.
const FROZEN_MONOTONIC_NS: u64 = 5353;
/// The seed the invoker binds into `entropy.seeded`.
const SEED: u64 = 9999;

/// `configure(time.frozen, now-seconds, monotonic-ns)` — an async-API provider bound
/// entirely by the invoker.
fn configured_frozen() -> Component {
    guest::ensure_components(STUBS);
    configure(
        &guest::load_stub("time.frozen"),
        &[
            ("now-seconds", FROZEN_SECONDS.to_string()),
            ("monotonic-ns", FROZEN_MONOTONIC_NS.to_string()),
        ],
    )
    .expect("configure(time.frozen, …) should succeed")
}

/// `configure(entropy.seeded, seed)`.
fn configured_seeded() -> Component {
    guest::ensure_components(STUBS);
    configure(
        &guest::load_stub("entropy.seeded"),
        &[("seed", SEED.to_string())],
    )
    .expect("configure(entropy.seeded, …) should succeed")
}

/// The `$`-chain form: `configure(time.frozen, …) $ configure(entropy.seeded, …) $ guest`.
fn composed_dollar_chain() -> Component {
    let program = fixtures::invoker_env_guest();
    let program =
        compose(&configured_seeded(), &program).expect("configured entropy.seeded $ guest");
    compose(&configured_frozen(), &program).expect("configured time.frozen $ …")
}

/// The `&` form: `(configure(time.frozen, …) & configure(entropy.seeded, …)) $ guest`.
fn composed_via_environment() -> Component {
    let env = extend(&configured_frozen(), &configured_seeded())
        .expect("configured time.frozen & configured entropy.seeded");
    compose(&env, &fixtures::invoker_env_guest()).expect("env $ guest")
}

/// Run a composed program with an ambient text capture (plus any extra ambient providers
/// the caller wants to try to sneak in).
fn run_with_ambient(program: &Component, extra_ambient: Providers) -> (Outcome, String) {
    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        ..extra_ambient
    };
    let outcome = run::run_component(program, &[], providers);
    (outcome, capture.stdout())
}

/// The outcome the guest reports: the xor of the first two seeded samples plus the two
/// configured clock readings (the guest folds every provider observation into its
/// result). The stub and the runtime's in-memory `SeededEntropy` implement the same
/// PRNG, so the host-side provider doubles as the reference implementation.
fn expected_outcome() -> u64 {
    use eo9_runtime::EntropyProvider;
    let mut reference = SeededEntropy::new(SEED);
    let fold = reference.get_u64() ^ reference.get_u64();
    fold.wrapping_add(FROZEN_SECONDS as u64)
        .wrapping_add(FROZEN_MONOTONIC_NS)
}

/// The stdout the guest produces: the fixed line plus one character derived from the
/// first entropy sample.
fn expected_stdout() -> String {
    use eo9_runtime::EntropyProvider;
    let first = SeededEntropy::new(SEED).get_u64();
    let derived = (b'a' + (first & 0xF) as u8) as char;
    format!("{}{derived}", fixtures::INVOKER_ENV_OUTPUT_LINE)
}

#[test]
fn the_invoker_configured_environment_runs_and_the_program_observes_it() {
    let program = composed_dollar_chain();

    // The guest imports no config interfaces, and the composition seals time and entropy
    // (the stubs' config interfaces are sealed inside the configured providers and never
    // appear at all); only text is left residual, on purpose.
    let imports: Vec<String> = program
        .describe()
        .imports
        .iter()
        .map(|i| i.interface.clone())
        .collect();
    for sealed in [
        "eo9:time/time",
        "eo9:time/frozen-config",
        "eo9:entropy/entropy",
        "eo9:entropy/seeded-config",
    ] {
        assert!(
            !imports.iter().any(|i| i == sealed),
            "{sealed} must be sealed by the composition, residuals: {imports:?}"
        );
    }
    assert!(
        imports.iter().any(|i| i == "eo9:text/text"),
        "text is deliberately left residual, residuals: {imports:?}"
    );

    // The program observes the invoker-configured frozen clock and seeded entropy — and
    // its sleep through the configured clock completes, which is the async-forwarding
    // proof.
    let (outcome, stdout) = run_with_ambient(&program, Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        expected_outcome().to_string(),
        "unexpected outcome — the guest folds every provider observation into its result"
    );
    assert_eq!(stdout, expected_stdout());
}

#[test]
fn repeated_runs_of_the_invoker_configured_environment_are_byte_identical() {
    let program = composed_dollar_chain();

    let (first_outcome, first_stdout) = run_with_ambient(&program, Providers::none());
    let (second_outcome, second_stdout) = run_with_ambient(&program, Providers::none());
    assert!(first_outcome.is_normal(), "{first_outcome:?}");
    assert_eq!(first_outcome, second_outcome);
    assert_eq!(first_stdout, second_stdout);

    // Configuring and composing the environment again from the same stub components is
    // also byte-identical, so the composition stays cache-keyable.
    assert_eq!(composed_dollar_chain().save(), program.save());
}

#[test]
fn the_invoker_configured_environment_is_sealed_against_the_ambient_root_providers() {
    let program = composed_dollar_chain();
    let (reference, reference_stdout) = run_with_ambient(&program, Providers::none());

    // Granting conflicting ambient root providers — a different frozen clock, a different
    // seed — changes nothing: the invoker-configured providers win.
    let ambient = Providers {
        time: Some(Box::new(FrozenTime::new(999_999, 999))),
        entropy: Some(Box::new(SeededEntropy::new(SEED + 1))),
        ..Providers::none()
    };
    let (with_ambient, with_ambient_stdout) = run_with_ambient(&program, ambient);
    assert_eq!(
        with_ambient, reference,
        "ambient root providers must never reach a sealed capability"
    );
    assert_eq!(with_ambient_stdout, reference_stdout);
    assert_eq!(
        run::success_value(&reference),
        expected_outcome().to_string()
    );
}

#[test]
fn the_environment_form_and_the_dollar_chain_agree() {
    // SPEC "Environments and the `&` operator", action law — here with invoker-configured
    // providers rather than program-configured stubs.
    let chain = composed_dollar_chain();
    let environment = composed_via_environment();

    assert_eq!(chain.describe().imports, environment.describe().imports);

    let (chain_outcome, chain_stdout) = run_with_ambient(&chain, Providers::none());
    let (env_outcome, env_stdout) = run_with_ambient(&environment, Providers::none());
    assert_eq!(chain_outcome, env_outcome);
    assert_eq!(chain_stdout, env_stdout);
    assert_eq!(
        run::success_value(&env_outcome),
        expected_outcome().to_string()
    );
}
