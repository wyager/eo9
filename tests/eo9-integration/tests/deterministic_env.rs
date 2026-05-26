//! The deterministic-environment suite (integration milestone I2, plan/13-tests.md
//! milestone 2): the real stub provider components — `time.frozen`, `entropy.seeded`,
//! `fs.memfs`, built by `xtask build-guest` — are composed around a test program with
//! `eo9-component` per the spec's `$`/`&` semantics, configured through their config
//! interfaces, and executed with `eo9-runtime`. The program observes the frozen clock, the
//! seeded entropy, and the memfs it populates; repeated runs are byte-identical; and the
//! composed environment is sealed against the ambient root providers.

use eo9_component::{Component, compose, extend};
use eo9_integration::{fixtures, guest, run};
use eo9_runtime::providers::{CaptureText, FrozenTime, MemFs, SeededEntropy};
use eo9_runtime::{Outcome, Providers};

/// The stub components this suite composes (guest packages built by `xtask build-guest`).
const STUBS: &[&str] = &[
    "eo9-stub-time-frozen",
    "eo9-stub-entropy-seeded",
    "eo9-stub-fs-memfs",
    "eo9-stub-text-null",
];

fn frozen() -> Component {
    guest::load_stub("time.frozen")
}

fn seeded() -> Component {
    guest::load_stub("entropy.seeded")
}

fn memfs() -> Component {
    guest::load_stub("fs.memfs")
}

/// The `$`-chain form of the deterministic environment around the fixture guest:
/// `time.frozen $ entropy.seeded $ fs.memfs $ guest`.
fn composed_dollar_chain() -> Component {
    guest::ensure_components(STUBS);
    let program = fixtures::det_env_guest();
    let program = compose(&memfs(), &program).expect("fs.memfs $ guest");
    let program = compose(&seeded(), &program).expect("entropy.seeded $ …");
    compose(&frozen(), &program).expect("time.frozen $ …")
}

/// The `&` form: `(time.frozen & entropy.seeded & fs.memfs) $ guest`.
fn composed_via_environment() -> Component {
    guest::ensure_components(STUBS);
    let env = extend(&frozen(), &seeded()).expect("time.frozen & entropy.seeded");
    let env = extend(&env, &memfs()).expect("… & fs.memfs");
    compose(&env, &fixtures::det_env_guest()).expect("env $ guest")
}

/// Run a composed deterministic-environment program with an ambient text capture (plus
/// any extra ambient providers the caller wants to try to sneak in).
fn run_with_ambient(program: &Component, extra_ambient: Providers) -> (Outcome, String) {
    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        ..extra_ambient
    };
    let outcome = run::run_component(program, &[], providers);
    (outcome, capture.stdout())
}

/// The outcome value the fixture reports when every internal check passed: the xor of the
/// first two samples of a SplitMix64 stream over the configured seed. The stub and the
/// runtime's in-memory `SeededEntropy` implement the same PRNG, so the host-side provider
/// doubles as the reference implementation here.
fn expected_entropy_fold() -> u64 {
    use eo9_runtime::EntropyProvider;
    let mut reference = SeededEntropy::new(fixtures::DET_ENV_SEED);
    reference.get_u64() ^ reference.get_u64()
}

/// The stdout the fixture produces: the fixed line plus one character derived from the
/// first entropy sample.
fn expected_stdout() -> String {
    use eo9_runtime::EntropyProvider;
    let first = SeededEntropy::new(fixtures::DET_ENV_SEED).get_u64();
    let derived = (b'a' + (first & 0xF) as u8) as char;
    format!("{}{derived}", fixtures::DET_ENV_OUTPUT_LINE)
}

#[test]
fn the_deterministic_environment_runs_and_the_program_observes_it() {
    let program = composed_dollar_chain();

    // The environment's capabilities are sealed: time, entropy, and fs (and the config
    // interfaces) are no longer imports of the composition; only text (left residual on
    // purpose) and the memfs stub's own io-buffers import remain.
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
        "eo9:fs/fs",
        "eo9:fs/memfs-config",
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

    // The program observes the configured frozen clock, the seeded entropy, and the memfs
    // it populated: any failed check would surface as a small sentinel value instead.
    let (outcome, stdout) = run_with_ambient(&program, Providers::none());
    assert_eq!(
        run::success_value(&outcome),
        expected_entropy_fold().to_string(),
        "unexpected outcome (sentinel values 1..=12 mean a specific stub check failed)"
    );
    assert_eq!(stdout, expected_stdout());
}

#[test]
fn repeated_runs_of_the_deterministic_environment_are_byte_identical() {
    let program = composed_dollar_chain();

    let (first_outcome, first_stdout) = run_with_ambient(&program, Providers::none());
    let (second_outcome, second_stdout) = run_with_ambient(&program, Providers::none());
    assert!(first_outcome.is_normal(), "{first_outcome:?}");
    assert_eq!(first_outcome, second_outcome);
    assert_eq!(first_stdout, second_stdout);

    // Composing the environment again from the same stub components is also byte-identical,
    // so the composition itself is cache-keyable.
    assert_eq!(composed_dollar_chain().save(), program.save());
}

#[test]
fn the_composed_environment_is_sealed_against_the_ambient_root_providers() {
    let program = composed_dollar_chain();
    let (reference, reference_stdout) = run_with_ambient(&program, Providers::none());

    // Granting conflicting ambient root providers — a different wall clock, a different
    // entropy seed, a pre-populated host fs — changes nothing: the composed-in stubs win.
    let ambient_fs = MemFs::new();
    ambient_fs.insert_file("ambient.txt", b"from the outside".to_vec());
    let ambient = Providers {
        time: Some(Box::new(FrozenTime::new(999_999, 999))),
        entropy: Some(Box::new(SeededEntropy::new(fixtures::DET_ENV_SEED + 1))),
        fs: Some(Box::new(ambient_fs)),
        ..Providers::none()
    };
    let (with_ambient, with_ambient_stdout) = run_with_ambient(&program, ambient);
    assert_eq!(
        with_ambient, reference,
        "ambient root providers must never reach a sealed capability"
    );
    assert_eq!(with_ambient_stdout, reference_stdout);
}

#[test]
fn the_environment_form_and_the_dollar_chain_agree() {
    // SPEC "Environments and the `&` operator", action law: (x & y & z) $ c ≡ x $ y $ z $ c
    // — here with the real stub components rather than algebra-only fixtures.
    let chain = composed_dollar_chain();
    let environment = composed_via_environment();

    let chain_imports: Vec<_> = chain.describe().imports;
    let environment_imports: Vec<_> = environment.describe().imports;
    assert_eq!(chain_imports, environment_imports);

    let (chain_outcome, chain_stdout) = run_with_ambient(&chain, Providers::none());
    let (env_outcome, env_stdout) = run_with_ambient(&environment, Providers::none());
    assert_eq!(chain_outcome, env_outcome);
    assert_eq!(chain_stdout, env_stdout);
    assert_eq!(
        run::success_value(&env_outcome),
        expected_entropy_fold().to_string()
    );
}

#[test]
fn the_real_text_null_stub_seals_the_text_import_like_a_fixture_sink() {
    // Milestone-1's ambient-sealing test used a hand-written text sink; the real
    // `text.null` stub now exists, so exercise the same law through it: once composed,
    // the ambient text provider can no longer observe the program's output.
    guest::ensure_components(STUBS);
    let writer = fixtures::text_writer();
    let sealed = compose(&guest::load_stub("text.null"), &writer).expect("text.null $ writer");
    assert!(sealed.describe().imports.is_empty());

    let ambient = CaptureText::new();
    let outcome = run::run_component(
        &sealed,
        &[],
        Providers {
            text: Some(Box::new(ambient.clone())),
            ..Providers::none()
        },
    );
    assert_eq!(run::success_value(&outcome), "42");
    assert_eq!(
        ambient.stdout(),
        "",
        "output must go to the composed-in text.null, never to the ambient provider"
    );
}
