//! Default configurations for the configurable stubs (plan/09 Decision 14, the owner's
//! option-C ruling): a configurable provider composed *without* `configure` self-binds
//! its documented default on first use instead of trapping — `fs.memfs` an empty
//! filesystem, `time.frozen` 2000-01-01T00:00:00 UTC / monotonic 0, `entropy.seeded` the
//! seed `0xE09` — and `configure` (or the shell's provider flags) still overrides it.
//! These tests compose the real stub components plainly (no config call anywhere) around
//! real programs and assert the documented defaults are what the programs observe,
//! deterministically.

use eo9_component::compose;
use eo9_integration::{guest, run};
use eo9_runtime::providers::CaptureText;
use eo9_runtime::{EntropyProvider, NamedArg, Outcome, Providers};

/// The documented default seed of `entropy.seeded` (must match the stub).
const DEFAULT_SEED: u64 = 0xE09;

/// The documented default frozen instant of `time.frozen` (must match the stub).
const DEFAULT_NOW_SECONDS: i64 = 946_684_800;

/// Run `program` with an ambient text capture (and nothing else ambient), returning the
/// outcome and captured stdout.
fn run_with_capture(
    program: &eo9_component::Component,
    args: &[NamedArg],
) -> (Outcome, String) {
    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        ..Providers::none()
    };
    let outcome = run::run_component(program, args, providers);
    (outcome, capture.stdout())
}

/// `time.frozen $ hello` with no configuration anywhere: hello runs (no trap) and prints
/// the documented default instant, 2000-01-01T00:00:00 UTC.
#[test]
fn unconfigured_time_frozen_defaults_to_the_documented_epoch() {
    guest::ensure_components(&["eo9-stub-time-frozen", "eo9-example-hello"]);
    let program = compose(
        &guest::load_stub("time.frozen"),
        &guest::load_example("hello"),
    )
    .expect("time.frozen $ hello");

    let (outcome, stdout) = run_with_capture(
        &program,
        &[
            NamedArg::new("name", "\"default\""),
            NamedArg::new("excited", "true"),
        ],
    );
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "unconfigured time.frozen must not trap: {outcome:?}"
    );
    let expected_prefix = format!("[{DEFAULT_NOW_SECONDS}.000000000] Hello, default");
    assert!(
        stdout.contains(&expected_prefix),
        "expected the documented default epoch in {stdout:?}"
    );
}

/// `entropy.seeded $ rng --count 2` with no configuration anywhere: rng runs (no trap)
/// and prints exactly the SplitMix64 stream of the documented default seed; two runs are
/// identical.
#[test]
fn unconfigured_entropy_seeded_defaults_to_the_documented_seed() {
    guest::ensure_components(&["eo9-stub-entropy-seeded", "eo9-coreutil-rng"]);
    let program = compose(
        &guest::load_stub("entropy.seeded"),
        &guest::load_component("eo9-coreutil-rng"),
    )
    .expect("entropy.seeded $ rng");
    let args = [NamedArg::new("count", "2")];

    let (first_outcome, first_stdout) = run_with_capture(&program, &args);
    assert!(
        matches!(first_outcome, Outcome::Success(_)),
        "unconfigured entropy.seeded must not trap: {first_outcome:?}"
    );

    // The runtime's in-memory SeededEntropy is the same SplitMix64 PRNG, so it doubles
    // as the reference for the documented default seed.
    let mut reference = eo9_runtime::providers::SeededEntropy::new(DEFAULT_SEED);
    let expected = format!("{}\n{}\n", reference.get_u64(), reference.get_u64());
    assert_eq!(
        first_stdout, expected,
        "the default seed must produce the documented SplitMix64 stream"
    );

    // Deterministic: a second instance of the same composition produces the same bytes.
    let (_, second_stdout) = run_with_capture(&program, &args);
    assert_eq!(first_stdout, second_stdout);
}

/// `fs.memfs $ readwrite` with no configuration anywhere: the write/read round-trip
/// succeeds against the documented default (an empty filesystem) instead of trapping.
#[test]
fn unconfigured_fs_memfs_defaults_to_an_empty_filesystem() {
    guest::ensure_components(&["eo9-stub-fs-memfs", "eo9-example-readwrite"]);
    let program = compose(
        &guest::load_stub("fs.memfs"),
        &guest::load_example("readwrite"),
    )
    .expect("fs.memfs $ readwrite");

    let outcome = run::run_component(
        &program,
        &[
            NamedArg::new("path", "\"note.txt\""),
            NamedArg::new("contents", "\"default-config\""),
        ],
        Providers::none(),
    );
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "unconfigured fs.memfs must not trap: {outcome:?}"
    );
}
