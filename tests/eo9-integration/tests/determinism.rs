//! Determinism suite (plan/13-tests.md milestone 1): the same inputs produce the same
//! bytes — for executed outcomes under deterministic providers, for the component
//! algebra's operations, for the fixture build pipeline itself, and for the store's
//! compile-cache keys.
//!
//! Scope note: everything here is *in-process, same-machine* determinism. Cross-machine /
//! cross-compiler-codegen determinism is the store-cache concern tracked by areas 04 and
//! 06 and is out of scope for this suite. Byte-identity of `eo9_runtime::Image::compile`
//! output cannot be tested yet because `Image` exposes no serialized form (image
//! serialization is deferred in plan/04-runtime.md § D7); see plan/13-tests.md Decisions.

use eo9_component::{compose, extend, rename, restrict};
use eo9_integration::{fixtures, run};
use eo9_runtime::providers::{CaptureText, FrozenTime, SeededEntropy};
use eo9_runtime::{NamedArg, Outcome, Providers};
use eo9_store::{CacheKeyParams, ObjectHash};

/// One run of the determinism guest under fully deterministic providers: frozen time,
/// seeded entropy, captured text. Everything the guest observes is fixed by `seed`.
fn run_det_guest(seed: u64) -> (Outcome, String) {
    let guest = fixtures::det_guest();
    let image = run::compile_component(&guest);
    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        time: Some(Box::new(FrozenTime::new(1_750_000_000, 123_456_789))),
        entropy: Some(Box::new(SeededEntropy::new(seed))),
        fs: None,
    };
    let outcome = run::run_image(&image, &[NamedArg::new("tag", "\"det-run\"")], providers);
    (outcome, capture.stdout())
}

/// SPEC "The module store and compilation cache" / "How readiness is implemented": with
/// deterministic providers (frozen time, seeded entropy) and fixed WAVE arguments, the
/// same component run twice produces byte-identical rendered outcomes and output.
#[test]
fn deterministic_providers_give_byte_identical_outcomes_across_runs() {
    let (first_outcome, first_stdout) = run_det_guest(42);
    let (second_outcome, second_stdout) = run_det_guest(42);

    assert!(
        first_outcome.is_normal(),
        "the guest must finish normally: {first_outcome:?}"
    );
    assert_eq!(
        first_outcome, second_outcome,
        "rendered outcomes must be byte-identical"
    );
    assert_eq!(
        first_stdout, second_stdout,
        "captured output must be byte-identical"
    );

    // The output really does carry provider observations: the echoed tag plus one
    // entropy-derived character.
    assert!(first_stdout.starts_with("det-run"));
    assert_eq!(first_stdout.len(), "det-run".len() + 1);

    // Sanity: the outcome genuinely depends on the providers (a different seed changes it),
    // so the byte-identity above is not vacuous.
    let (other_outcome, _) = run_det_guest(43);
    assert_ne!(first_outcome, other_outcome);
}

/// The component algebra is math on bytes: the same operation on the same operands yields
/// byte-identical components every time (what the content-addressed store keys on).
#[test]
fn component_algebra_operations_are_byte_identical_across_repeated_runs() {
    let provider_seven = fixtures::answer_provider(7);
    let provider_nine = fixtures::answer_provider(9);
    let consumer = fixtures::answer_consumer();
    let optional = fixtures::optional_consumer();

    // `$` — compose.
    assert_eq!(
        compose(&provider_seven, &consumer).unwrap().save(),
        compose(&provider_seven, &consumer).unwrap().save(),
    );
    // `&` — extend.
    assert_eq!(
        extend(&provider_seven, &provider_nine).unwrap().save(),
        extend(&provider_seven, &provider_nine).unwrap().save(),
    );
    // `rename` — slot relabeling.
    assert_eq!(
        rename(&provider_seven, "eo9-tests:cap/answer", "left")
            .unwrap()
            .save(),
        rename(&provider_seven, "eo9-tests:cap/answer", "left")
            .unwrap()
            .save(),
    );
    // `only` — restriction, including the synthesized absent provider for optional sealing.
    assert_eq!(
        restrict(&optional, &[]).unwrap().save(),
        restrict(&optional, &[]).unwrap().save(),
    );
}

/// The fixture build pipeline itself (WIT + core module -> component) is deterministic,
/// so fixture identity is stable enough to key a content-addressed store on.
#[test]
fn fixture_builds_are_byte_identical_across_repeated_runs() {
    assert_eq!(
        fixtures::answer_provider(7).save(),
        fixtures::answer_provider(7).save()
    );
    assert_eq!(fixtures::det_guest().save(), fixtures::det_guest().save());
}

/// `eo9-store` compile-cache keys: stable for identical inputs, different for each field
/// that the compiled artifact depends on — exercised with module hashes taken from real
/// component-algebra outputs.
#[test]
fn store_cache_keys_are_stable_and_sensitive_to_every_field() {
    let env = fixtures::answer_provider(7);
    let app = fixtures::answer_consumer();

    // The same composition always hashes to the same module identity.
    let composed = compose(&env, &app).unwrap();
    let recomposed = compose(&env, &app).unwrap();
    assert_eq!(
        ObjectHash::of(composed.bytes()),
        ObjectHash::of(recomposed.bytes())
    );

    let params = CacheKeyParams {
        module_hashes: vec![ObjectHash::of(env.bytes()), ObjectHash::of(app.bytes())],
        configure_constants: vec![("seed".to_string(), "42".to_string())],
        compile_opts: "{debug-info: false}".to_string(),
        target_triple: "aarch64-apple-darwin".to_string(),
        compiler_version: "wasmtime-45.0.0 cranelift".to_string(),
        compiler_deterministic: false,
    };

    // Stable: identical inputs give identical keys.
    assert_eq!(params.key(), params.key());
    assert_eq!(params.clone().key(), params.key());

    // Sensitive to every field.
    let base = params.key();

    let mut changed = params.clone();
    changed.module_hashes = vec![
        ObjectHash::of(fixtures::answer_provider(9).bytes()),
        ObjectHash::of(app.bytes()),
    ];
    assert_ne!(changed.key(), base, "module content");

    let mut changed = params.clone();
    changed.module_hashes.reverse();
    assert_ne!(changed.key(), base, "module (composition) order");

    let mut changed = params.clone();
    changed.configure_constants[0].1 = "43".to_string();
    assert_ne!(changed.key(), base, "configure constants");

    let mut changed = params.clone();
    changed.compile_opts = "{debug-info: true}".to_string();
    assert_ne!(changed.key(), base, "compile options");

    let mut changed = params.clone();
    changed.target_triple = "riscv64gc-unknown-none-elf".to_string();
    assert_ne!(changed.key(), base, "target triple");

    let mut changed = params.clone();
    changed.compiler_version = "wasmtime-46.0.0 cranelift".to_string();
    assert_ne!(changed.key(), base, "compiler version");

    let mut changed = params.clone();
    changed.compiler_deterministic = true;
    assert_ne!(changed.key(), base, "determinism flag");
}
