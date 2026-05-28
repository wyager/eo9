//! Interposition: a guest middleware (a provider that imports and re-exports the same
//! interface) layered over a guest provider of that interface — the spec's worked
//! example of attenuation, and the composition shape the PL user study found broken
//! (docs/user-studies/05-pl-researcher.md, finding 1).
//!
//! Shapes covered, all with the real built stubs and the real `hello` example:
//!   * plain (default-configured) `time.frozen $ time.fuzzy $ hello`;
//!   * configured `configure(time.frozen, …) $ configure(time.fuzzy, …) $ hello`;
//!   * the `&` form `(configure(time.frozen, …) & configure(time.fuzzy, …)) $ hello`.
//! In every case the program must run to `success` and observe the frozen clock as
//! quantized by the fuzzy layer — never trap.

use eo9_component::{Component, compose, configure, extend};
use eo9_integration::{guest, run};
use eo9_runtime::providers::CaptureText;
use eo9_runtime::{NamedArg, Outcome, Providers};

const STUBS: &[&str] = &[
    "eo9-stub-time-frozen",
    "eo9-stub-time-fuzzy",
    "eo9-example-hello",
];

fn hello_args() -> Vec<NamedArg> {
    vec![
        NamedArg::new("name", "\"layered\""),
        NamedArg::new("excited", "false"),
    ]
}

/// Run with only a text capture ambient: time must come from the composition.
fn run_with_capture(program: &Component, args: &[NamedArg]) -> (Outcome, String) {
    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        ..Providers::none()
    };
    let outcome = run::run_component(program, args, providers);
    (outcome, capture.stdout())
}

fn assert_frozen_through_fuzzy(outcome: &Outcome, stdout: &str, expected_seconds: i64) {
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "interposed chain must not trap: {outcome:?} (stdout: {stdout:?})"
    );
    let expected = format!("[{expected_seconds}.000000000] Hello, layered.");
    assert!(
        stdout.contains(&expected),
        "expected the frozen instant {expected:?} through the fuzzy layer in {stdout:?}"
    );
}

/// `time.frozen $ time.fuzzy $ hello` with no configuration anywhere: the documented
/// defaults apply (frozen 2000-01-01, 1 ms fuzzy granularity) and the chain runs.
#[test]
fn plain_middleware_over_plain_provider_runs() {
    guest::ensure_components(STUBS);
    let frozen = guest::load_stub("time.frozen");
    let fuzzy = guest::load_stub("time.fuzzy");
    let hello = guest::load_example("hello");

    let inner = compose(&fuzzy, &hello).expect("time.fuzzy $ hello");
    let chain = compose(&frozen, &inner).expect("time.frozen $ (time.fuzzy $ hello)");

    let (outcome, stdout) = run_with_capture(&chain, &hello_args());
    assert_frozen_through_fuzzy(&outcome, &stdout, 946_684_800);
}

/// The configured `$` chain from the study report: a configured guest middleware over a
/// configured guest provider of the same interface.
#[test]
#[ignore = "configured-over-configured interposition still traps: the binder's configuration \
            gate requires `configure` to complete eagerly, and a configure that calls through \
            another composed provider does not (and the gate cannot wait: wasmtime 45 forbids a \
            synchronously-lifted task from blocking) — see plan/03 Decision 15"]
fn configured_middleware_over_configured_provider_runs() {
    guest::ensure_components(STUBS);
    let frozen = configure(
        &guest::load_stub("time.frozen"),
        &[("now-seconds", "50"), ("monotonic-ns", "123456789")],
    )
    .expect("configure(time.frozen, …)");
    let fuzzy = configure(
        &guest::load_stub("time.fuzzy"),
        &[("granularity-ns", "1000000000")],
    )
    .expect("configure(time.fuzzy, …)");
    let hello = guest::load_example("hello");

    let inner = compose(&fuzzy, &hello).expect("configured fuzzy $ hello");
    let chain = compose(&frozen, &inner).expect("configured frozen $ (fuzzy $ hello)");

    let (outcome, stdout) = run_with_capture(&chain, &hello_args());
    assert_frozen_through_fuzzy(&outcome, &stdout, 50);
}

/// The `&` form of the same environment: `(frozen & fuzzy) $ hello`.
#[test]
#[ignore = "configured-over-configured interposition still traps: the binder's configuration \
            gate requires `configure` to complete eagerly, and a configure that calls through \
            another composed provider does not (and the gate cannot wait: wasmtime 45 forbids a \
            synchronously-lifted task from blocking) — see plan/03 Decision 15"]
fn configured_environment_with_middleware_runs() {
    guest::ensure_components(STUBS);
    let frozen = configure(
        &guest::load_stub("time.frozen"),
        &[("now-seconds", "50"), ("monotonic-ns", "123456789")],
    )
    .expect("configure(time.frozen, …)");
    let fuzzy = configure(
        &guest::load_stub("time.fuzzy"),
        &[("granularity-ns", "1000000000")],
    )
    .expect("configure(time.fuzzy, …)");
    let hello = guest::load_example("hello");

    let env = extend(&frozen, &fuzzy).expect("frozen & fuzzy");
    let chain = compose(&env, &hello).expect("(frozen & fuzzy) $ hello");

    let (outcome, stdout) = run_with_capture(&chain, &hello_args());
    assert_frozen_through_fuzzy(&outcome, &stdout, 50);
}
