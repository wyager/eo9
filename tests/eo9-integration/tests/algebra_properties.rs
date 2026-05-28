//! Generative property suite for the component algebra (PL user study, finding #6).
//!
//! The single-case law tests in `capabilities.rs` / `determinism.rs` and the shipped
//! corpus in `soundness_corpus.rs` each pin one shape; this suite *enumerates* small
//! component graphs from a fixed catalog of fixtures and asserts the spec's algebraic
//! properties hold across all of them. It is the bounded, deterministic form of the
//! "generative property suite over component triples" the study asked for, and it is the
//! generalization that would have caught the two bugs that study surfaced:
//!   * the drop-law bug (`fs.none $ cat` once produced an internal *encode/validation*
//!     failure instead of a clean drop) — caught by the **encoder-soundness** property;
//!   * the rename-of-a-residual-import bug (an artifact that failed the runtime parser) —
//!     caught by the **rename executability** property.
//!
//! Determinism: there is no randomness or wall-clock here — the catalog is a fixed set of
//! fixtures and every property enumerates over it, so a failure always reproduces.
//!
//! ## Operational `≡`
//!
//! The spec writes its laws with an abstract `≡` and does not pin down component instance
//! identity under composition (study finding #5). Throughout this suite, `a ≡ b` is the
//! **observational** stand-in: compiled and run with the same arguments under the same
//! root providers, `a` and `b` produce the same `program-outcome` (same success/failure
//! value text). That gap in the spec is recorded in plan/13 for a future SPEC
//! clarification; this file does not depend on the unspecified part.

use eo9_component::{
    Component, ComposeError, InterfaceRef, RenameError, RestrictError, compose, extend, rename,
    restrict,
};
use eo9_integration::{fixtures, guest, run};
use eo9_runtime::{Outcome, Providers};

// -----------------------------------------------------------------------------------------
// A fixed catalog of fixtures (the "generators"), all over the resource-free
// `eo9-tests:cap` vocabulary so that fully-wired graphs run under `Providers::none()`.
// -----------------------------------------------------------------------------------------

const ANSWER: &str = "eo9-tests:cap/answer";
const ANSWER_OPTIONAL: &str = "eo9-tests:cap/answer-optional";
const STORE: &str = "eo9-tests:cap/store";

/// Named binaries (consumers).
fn consumers() -> Vec<(&'static str, Component)> {
    vec![
        ("answer-consumer", fixtures::answer_consumer()),
        ("two-answers", fixtures::two_answers_consumer()),
        ("optional-consumer", fixtures::optional_consumer()),
        ("storage-consumer", fixtures::storage_consumer()),
    ]
}

/// Named providers, covering: a plain capability, the optional flavor (present/absent),
/// and a resource-free result-typed capability (working / deny).
fn providers() -> Vec<(String, Component)> {
    let mut v = vec![
        (
            "optional-present(5)".to_string(),
            fixtures::optional_provider_present(5),
        ),
        (
            "optional-absent".to_string(),
            fixtures::optional_provider_absent(),
        ),
        ("store-ok(11)".to_string(), fixtures::store_ok_provider(11)),
        ("store-deny".to_string(), fixtures::store_deny_provider()),
    ];
    for value in [1u32, 7, 42] {
        v.push((format!("answer({value})"), fixtures::answer_provider(value)));
    }
    v
}

// -----------------------------------------------------------------------------------------
// Property 1 — encoder soundness.
//
// For every algebra operation applied across the catalog, the result is EITHER a validated
// component (compose/extend/restrict/rename all re-load the encoded bytes, so `Ok` implies
// a valid component) OR a *typed* refusal — never an internal wiring/encoding failure.
// -----------------------------------------------------------------------------------------

/// Accumulates how many cases ran and any that returned an `Internal(_)` variant.
#[derive(Default)]
struct Soundness {
    cases: usize,
    internal: Vec<String>,
}

impl Soundness {
    fn compose(&mut self, label: impl AsRef<str>, r: &Result<Component, ComposeError>) {
        self.cases += 1;
        if let Err(ComposeError::Internal(m)) = r {
            self.internal.push(format!("{}: {m}", label.as_ref()));
        }
    }
    fn restrict(&mut self, label: impl AsRef<str>, r: &Result<Component, RestrictError>) {
        self.cases += 1;
        if let Err(RestrictError::Internal(m)) = r {
            self.internal.push(format!("{}: {m}", label.as_ref()));
        }
    }
    fn rename(&mut self, label: impl AsRef<str>, r: &Result<Component, RenameError>) {
        self.cases += 1;
        if let Err(RenameError::Internal(m)) = r {
            self.internal.push(format!("{}: {m}", label.as_ref()));
        }
    }
    fn finish(self, min_cases: usize) {
        assert!(
            self.internal.is_empty(),
            "the algebra reported {} internal failure(s) across {} cases (encoder \
             soundness): an operation must yield a validated component or a typed \
             refusal, never an internal error:\n{}",
            self.internal.len(),
            self.cases,
            self.internal.join("\n")
        );
        assert!(
            self.cases >= min_cases,
            "expected at least {min_cases} generated cases, only ran {}",
            self.cases
        );
        eprintln!(
            "algebra encoder-soundness: {} cases, 0 internal",
            self.cases
        );
    }
}

#[test]
fn enumerated_algebra_operations_never_fail_internally() {
    let consumers = consumers();
    let providers = providers();
    let mut s = Soundness::default();

    // compose: every provider $ every consumer (matches → Ok; mismatches → typed).
    for (pn, p) in &providers {
        for (cn, c) in &consumers {
            let r = compose(p, c);
            s.compose(format!("{pn} $ {cn}"), &r);
            // nested: a second provider layered over a successful composition.
            if let Ok(inner) = &r {
                for (qn, q) in &providers {
                    s.compose(format!("{qn} $ ({pn} $ {cn})"), &compose(q, inner));
                }
            }
        }
    }

    // compose with a binary on the left must be a typed `NotAProvider`, not Internal.
    for (an, a) in &consumers {
        for (bn, b) in &consumers {
            s.compose(format!("{an} $ {bn} (binary on the left)"), &compose(a, b));
        }
    }

    // extend (&): every provider pair, then compose the environment onto each consumer.
    for (xn, x) in &providers {
        for (yn, y) in &providers {
            let env = extend(x, y);
            s.compose(format!("{xn} & {yn}"), &env);
            if let Ok(env) = &env {
                for (cn, c) in &consumers {
                    s.compose(format!("({xn} & {yn}) $ {cn}"), &compose(env, c));
                }
            }
        }
    }
    // extend with a binary operand → typed NotAProvider.
    s.compose(
        "answer-consumer & answer(1) (binary operand)",
        &extend(&consumers[0].1, &providers[4].1),
    );

    // restrict (only): allow-lists that admit, that seal an optional, that reject a
    // required import, and a malformed entry.
    let admit_answer = [InterfaceRef::any(ANSWER)];
    let admit_optional = [InterfaceRef::any(ANSWER_OPTIONAL)];
    let admit_store = [InterfaceRef::any(STORE)];
    let empty: [InterfaceRef; 0] = [];
    let malformed = [InterfaceRef {
        interface: ANSWER.to_string(),
        version: Some("not-a-semver".to_string()),
    }];
    for (cn, c) in &consumers {
        s.restrict(format!("only [] $ {cn}"), &restrict(c, &empty));
        s.restrict(format!("only [answer] $ {cn}"), &restrict(c, &admit_answer));
        s.restrict(
            format!("only [answer-optional] $ {cn}"),
            &restrict(c, &admit_optional),
        );
        s.restrict(format!("only [store] $ {cn}"), &restrict(c, &admit_store));
        s.restrict(
            format!("only [bad-version] $ {cn}"),
            &restrict(c, &malformed),
        );
    }

    // rename: a present import slot, a present export slot, a missing slot, and a
    // collision (renaming `left` onto the existing `right` slot of `two-answers`).
    s.rename(
        "rename answer→s on answer-consumer",
        &rename(&consumers[0].1, ANSWER, "s"),
    );
    s.rename(
        "rename answer→s on answer(7) provider",
        &rename(&providers[5].1, ANSWER, "s"),
    );
    s.rename(
        "rename missing slot",
        &rename(&consumers[0].1, "eo9-tests:cap/nope", "x"),
    );
    s.rename(
        "rename left→right collision on two-answers",
        &rename(&consumers[1].1, "left", "right"),
    );

    s.finish(80);
}

// -----------------------------------------------------------------------------------------
// Property 2 — the action law: `(x & y) $ c ≡ x $ y $ c`, observed across a range of
// values so a mis-wiring (swapped slots) would change the outcome.
//
// `two-answers` imports `left: answer` and `right: answer`; wiring an `answer` provider
// to each slot (by renaming its export) lets the program report `left*100 + right`.
// -----------------------------------------------------------------------------------------

#[test]
fn the_action_law_holds_observationally() {
    for (a, b) in [(3u32, 4u32), (1, 2), (9, 0), (12, 34)] {
        let x = rename(&fixtures::answer_provider(a), ANSWER, "left").expect("rename to left");
        let y = rename(&fixtures::answer_provider(b), ANSWER, "right").expect("rename to right");
        let c = fixtures::two_answers_consumer();

        // (x & y) $ c
        let env = extend(&x, &y).expect("x & y");
        let lhs = compose(&env, &c).expect("(x & y) $ c");
        // x $ y $ c   ==   x $ (y $ c)
        let rhs = compose(&x, &compose(&y, &c).expect("y $ c")).expect("x $ (y $ c)");

        let expected = (a * 100 + b).to_string();
        assert_equiv(
            &lhs,
            &rhs,
            &format!("action law at (left={a}, right={b}) → {expected}"),
        );
        assert_eq!(
            run::success_value(&run::run_component(&lhs, &[], Providers::none())),
            expected,
            "(x & y) $ c must report left*100+right",
        );
    }
}

// -----------------------------------------------------------------------------------------
// Property 3 — sealing: in `outer $ (inner $ c)` the inner provider always wins, for any
// inner value (the outer grant has nothing left to satisfy and is dropped).
// -----------------------------------------------------------------------------------------

#[test]
fn the_innermost_provider_always_wins() {
    let outer = 99u32;
    for inner in [1u32, 7, 42, 1000] {
        let c = fixtures::answer_consumer();
        let sealed = compose(&fixtures::answer_provider(inner), &c).expect("inner $ c");
        assert!(
            sealed.describe().imports.is_empty(),
            "the matched import must be sealed away"
        );
        let regranted =
            compose(&fixtures::answer_provider(outer), &sealed).expect("outer $ (inner $ c)");

        let want = inner.to_string();
        assert_eq!(
            run::success_value(&run::run_component(&sealed, &[], Providers::none())),
            want
        );
        assert_eq!(
            run::success_value(&run::run_component(&regranted, &[], Providers::none())),
            want,
            "the outer provider must never reach the sealed import"
        );
    }
}

// -----------------------------------------------------------------------------------------
// Property 4 — `only` semantics observed: sealing an optional makes the program observe
// absence; rejecting a required import is a *typed* refusal naming the offender.
// -----------------------------------------------------------------------------------------

#[test]
fn only_seals_optionals_and_typed_refuses_required() {
    // Sealing the optional `answer` away: the program runs and observes absence.
    let gated = restrict(&fixtures::optional_consumer(), &[]).expect("only [] over an optional");
    assert_eq!(
        run::success_value(&run::run_component(&gated, &[], Providers::none())),
        fixtures::OPTIONAL_ABSENT_SENTINEL.to_string(),
        "a sealed optional must be observed as absent"
    );

    // Excluding a required import is a typed compose-time refusal that names it.
    let err = restrict(&fixtures::answer_consumer(), &[])
        .expect_err("only [] over a required import must be refused");
    match err {
        RestrictError::RequiredOutsideAllowList(offenders) => assert!(
            offenders.iter().any(|o| o.contains("answer")),
            "the refusal must name the offending interface: {offenders:?}"
        ),
        other => panic!("expected RequiredOutsideAllowList, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------------------
// Property 5 — rename round-trips (identity is recovered) and the renamed artifact is
// executable (its executable form compiles in the runtime). This is the generative form
// of the study's rename-residual bug guard.
// -----------------------------------------------------------------------------------------

#[test]
fn rename_round_trips_and_stays_executable() {
    for (label, base, slot, is_binary) in [
        (
            "answer-consumer import",
            fixtures::answer_consumer(),
            ANSWER,
            true,
        ),
        (
            "answer provider export",
            fixtures::answer_provider(7),
            ANSWER,
            false,
        ),
    ] {
        let renamed = rename(&base, slot, "relabelled").unwrap_or_else(|e| panic!("{label}: {e}"));
        // The renamed slot is visible under its new name with the same interface identity
        // (imports are keyed by `.slot`, exports by `.name`).
        let info = renamed.describe();
        let interface = info
            .imports
            .iter()
            .find(|i| i.slot == "relabelled")
            .map(|i| i.interface.clone())
            .or_else(|| {
                info.exports
                    .iter()
                    .find(|e| e.name == "relabelled")
                    .map(|e| e.interface.clone())
            })
            .unwrap_or_else(|| panic!("{label}: renamed slot must be present"));
        assert_eq!(interface, slot, "{label}: interface identity preserved");

        // Round-trip: renaming back recovers the original describe surface.
        let back = rename(&renamed, "relabelled", slot).expect("rename back");
        assert_eq!(
            describe_surface(&back),
            describe_surface(&base),
            "{label}: rename round-trip must recover the original surface"
        );

        // The executable form of the renamed artifact is a valid component (the algebra
        // keeps the annotated form for identity; the runtime is handed the stripped form).
        let executable = Component::load(renamed.executable_bytes())
            .expect("the renamed component's executable form is a valid component");
        // Only a binary can be compiled to a runnable image; for the provider case the
        // valid-component check above is the executability guarantee.
        if is_binary {
            let _image = run::compile_component(&executable);
        }
    }
}

// -----------------------------------------------------------------------------------------
// Property 6 — breadth over the real shipped components: rename / restrict / extend on
// resource-owning and stateful-configured providers (the generators the cap vocabulary
// can't express) are encoder-sound too. Keeps the count small so the guest build cost
// stays modest.
// -----------------------------------------------------------------------------------------

#[test]
fn shipped_components_survive_rename_restrict_extend_soundly() {
    guest::ensure_components(&[
        "eo9-example-hello",
        "eo9-stub-time-frozen",
        "eo9-stub-entropy-seeded",
        "eo9-stub-fs-memfs",
    ]);
    let hello = guest::load_example("hello");
    let frozen = guest::load_stub("time.frozen");
    let seeded = guest::load_stub("entropy.seeded");
    let memfs = guest::load_stub("fs.memfs");

    let mut s = Soundness::default();

    // restrict over a real program: admit exactly its imports (Ok) vs drop a required one
    // (typed RequiredOutsideAllowList).
    s.restrict(
        "only [text,time] $ hello",
        &restrict(
            &hello,
            &[
                InterfaceRef::any("eo9:text/text"),
                InterfaceRef::any("eo9:time/time"),
            ],
        ),
    );
    s.restrict(
        "only [text] $ hello (drops required time)",
        &restrict(&hello, &[InterfaceRef::any("eo9:text/text")]),
    );

    // rename a residual import on a real program (the study-3 shape) — must be sound and
    // its executable form must compile.
    let renamed = rename(&hello, "eo9:time/time", "wallclock");
    s.rename("rename eo9:time/time→wallclock on hello", &renamed);
    if let Ok(renamed) = &renamed {
        let exec = Component::load(renamed.executable_bytes())
            .expect("renamed hello executable form is valid");
        let _ = run::compile_component(&exec);
    }

    // extend over real, stateful, resource-owning providers.
    s.compose("time.frozen & entropy.seeded", &extend(&frozen, &seeded));
    s.compose("entropy.seeded & fs.memfs", &extend(&seeded, &memfs));

    s.finish(5);
}

// -----------------------------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------------------------

/// Observational equivalence: same outcome value text under the same (empty) providers.
fn assert_equiv(a: &Component, b: &Component, label: &str) {
    let ra = outcome_repr(&run::run_component(a, &[], Providers::none()));
    let rb = outcome_repr(&run::run_component(b, &[], Providers::none()));
    assert_eq!(
        ra, rb,
        "{label}: components are not observationally equivalent"
    );
}

fn outcome_repr(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Success(v) => format!("success({})", v.value),
        Outcome::Failure(v) => format!("failure({})", v.value),
        other => format!("{other:?}"),
    }
}

/// A stable, comparable rendering of a component's import/export slots (slot → interface).
fn describe_surface(c: &Component) -> Vec<(String, String)> {
    let info = c.describe();
    let mut surface: Vec<(String, String)> = info
        .imports
        .iter()
        .map(|i| (format!("import {}", i.slot), i.interface.clone()))
        .chain(
            info.exports
                .iter()
                .map(|e| (format!("export {}", e.name), e.interface.clone())),
        )
        .collect();
    surface.sort();
    surface
}
