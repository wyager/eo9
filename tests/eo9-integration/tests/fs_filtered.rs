//! `fs.policy-subtree $ fs.filtered $ program` — the path-policy filesystem attenuator
//! ("policies are programs", SPEC: Eo9 API design; docs/design/policy-components.md).
//!
//! `fs.filtered` gates every path operation through a composed `eo9:fs/path-policy`
//! component; `fs.policy-subtree` is the standard policy (allow under a prefix,
//! read-write or read-only; deny outside). These tests pin:
//!
//! * end-to-end behavior over `fs.memfs` in usermode (allow / deny / read-only verdicts
//!   surface as the program's own typed outcomes),
//! * **the path-traversal defense**: `/docs/../secret.txt` is normalized before the
//!   policy ever sees it, so it can never sneak past a `/docs` rule — and the refusal is
//!   the *policy's* `denied`, not a backend error, proving normalization (not luck)
//!   caught it,
//! * the never-trap rules (an unconfigured policy composes and runs as deny-all),
//! * purity and composition shape.

use eo9_component::{Component, compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

/// `fs.filtered $ readwrite`, with the path-policy and underlying-fs imports open.
fn filtered_readwrite() -> Component {
    guest::ensure_components(&["eo9-stub-fs-filtered", "eo9-example-readwrite"]);
    compose(
        &guest::load_stub("fs.filtered"),
        &guest::load_component("eo9-example-readwrite"),
    )
    .expect("fs.filtered $ readwrite must compose")
}

/// The standard subtree policy, configured.
fn subtree_policy(prefix: &str, access: &str) -> Component {
    guest::ensure_components(&["eo9-stub-fs-policy-subtree"]);
    configure(
        &guest::load_stub("fs.policy-subtree"),
        &[
            ("prefix", format!("{prefix:?}").as_str()),
            ("access", access),
        ],
    )
    .expect("configure(fs.policy-subtree, --prefix … --access …) must bake")
}

/// Close `policy $ fs.filtered $ readwrite` over an empty in-memory filesystem.
fn closed_chain(policy: &Component) -> Component {
    guest::ensure_components(&["eo9-stub-fs-memfs"]);
    let chain = compose(policy, &filtered_readwrite()).expect("policy $ fs.filtered $ readwrite");
    compose(&guest::load_stub("fs.memfs"), &chain).expect("fs.memfs $ …")
}

/// Run the closed chain against a path, returning the outcome.
fn run_readwrite(chain: &Component, path: &str) -> Outcome {
    run::run_component(
        chain,
        &[
            NamedArg::new("path", format!("{path:?}")),
            NamedArg::new("contents", "\"policy-test\""),
        ],
        Providers::none(),
    )
}

/// Assert the outcome is the program's own failure mentioning `needle` (dashes ignored,
/// so `read-only` matches both `ReadOnly` and `read-only` renderings).
fn assert_failure_contains(outcome: &Outcome, needle: &str) {
    let normalize = |s: &str| s.to_lowercase().replace('-', "");
    match outcome {
        Outcome::Failure(failure) => assert!(
            normalize(&failure.value).contains(&normalize(needle)),
            "expected the program's failure to mention {needle:?}: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}

#[test]
fn writes_inside_the_allowed_prefix_succeed() {
    let chain = closed_chain(&subtree_policy("/", "read-write"));
    let outcome = run_readwrite(&chain, "/note.txt");
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "a write inside the allowed prefix must round-trip: {outcome:?}"
    );
}

#[test]
fn paths_outside_the_prefix_are_denied_by_the_policy() {
    let chain = closed_chain(&subtree_policy("/docs", "read-write"));
    let outcome = run_readwrite(&chain, "/secret.txt");
    assert_failure_contains(&outcome, "denied");
}

#[test]
fn path_traversal_cannot_escape_the_subtree() {
    // The security property (and the reason fs.filtered normalizes): a path that is
    // *textually* under the prefix but *semantically* outside it must be refused by the
    // policy gate. Naive prefix matching would let "/docs/../secret.txt" through.
    let chain = closed_chain(&subtree_policy("/docs", "read-write"));
    let outcome = run_readwrite(&chain, "/docs/../secret.txt");
    // The refusal must be the policy's `denied` — not the backend's not-found (which is
    // what a forwarded raw path would produce on an empty memfs with no /docs directory).
    assert_failure_contains(&outcome, "denied");

    // Escaping the root entirely is also denied (never forwarded, never trapped).
    let outcome = run_readwrite(&chain, "/../../etc/passwd");
    assert_failure_contains(&outcome, "denied");
}

#[test]
fn a_read_only_subtree_blocks_writes_with_the_fs_apis_own_error() {
    let chain = closed_chain(&subtree_policy("/", "read-only"));
    // readwrite opens with create|write, so the policy's read-only verdict surfaces as
    // the fs API's own `read-only` error in the program's failure.
    let outcome = run_readwrite(&chain, "/note.txt");
    assert_failure_contains(&outcome, "readonly");
}

#[test]
fn a_read_only_subtree_still_forwards_reads() {
    // `ls` only reads (list-directory), so a read-only policy must let it through to the
    // underlying (empty) filesystem: success with zero entries, not a refusal.
    guest::ensure_components(&[
        "eo9-stub-fs-filtered",
        "eo9-stub-fs-policy-subtree",
        "eo9-stub-fs-memfs",
        "eo9-stub-text-null",
        "eo9-coreutil-ls",
    ]);
    let chain = compose(
        &guest::load_stub("fs.filtered"),
        &guest::load_component("eo9-coreutil-ls"),
    )
    .expect("fs.filtered $ ls");
    let chain = compose(&subtree_policy("/", "read-only"), &chain).expect("policy $ …");
    let chain = compose(&guest::load_stub("fs.memfs"), &chain).expect("fs.memfs $ …");
    let chain = compose(&guest::load_stub("text.null"), &chain).expect("text.null $ …");

    let outcome = run::run_component(&chain, &[], Providers::none());
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "a read under a read-only policy must be forwarded, not refused: {outcome:?}"
    );
}

#[test]
fn an_unconfigured_policy_denies_everything_and_never_traps() {
    guest::ensure_components(&["eo9-stub-fs-policy-subtree"]);
    // No configure: the policy's documented default is deny-all.
    let chain = closed_chain(&guest::load_stub("fs.policy-subtree"));
    let outcome = run_readwrite(&chain, "/anything.txt");
    assert_failure_contains(&outcome, "denied");
}

#[test]
fn the_policy_is_pure_and_the_composition_seals_it() {
    guest::ensure_components(&["eo9-stub-fs-policy-subtree", "eo9-stub-fs-filtered"]);

    // Purity: no capability imports (types-only uses and rt riders are not capabilities).
    let info = guest::load_stub("fs.policy-subtree").describe();
    assert!(
        info.imports
            .iter()
            .all(|need| need.authority_free || need.interface.starts_with("eo9:rt/")),
        "fs.policy-subtree must import nothing but types and rt riders: {:?}",
        info.imports
            .iter()
            .map(|n| (n.interface.clone(), n.authority_free))
            .collect::<Vec<_>>()
    );

    // Shape: the policy seals the middleware's path-policy import; the underlying fs
    // requirement stays visible.
    let chain = compose(
        &subtree_policy("/docs", "read-write"),
        &filtered_readwrite(),
    )
    .expect("policy $ fs.filtered $ readwrite");
    let residual: Vec<String> = chain
        .describe()
        .imports
        .iter()
        .map(|need| need.interface.clone())
        .collect();
    assert!(
        !residual.iter().any(|i| i == "eo9:fs/path-policy"),
        "the path-policy import must be sealed: {residual:?}"
    );
    assert!(
        residual.iter().any(|i| i == "eo9:fs/fs"),
        "the attenuator's underlying fs requirement must remain: {residual:?}"
    );
}

#[test]
fn reviewer_traversal_corpus_no_false_allows_and_no_false_denies() {
    // Reviewer-added corpus: trickier spellings in both directions — semantically-outside
    // paths must be denied no matter how they are written, and semantically-inside paths
    // must NOT be collateral damage of the normalization.
    let chain = closed_chain(&subtree_policy("/docs", "read-write"));

    // Semantically outside /docs — every spelling must be denied.
    for outside in [
        "//docs//../..",           // repeated separators + escape to root
        "/docs/./../secret.txt",   // dot segments resolving outside
        "/docs/..",                // exactly the parent
        "/docsx/file.txt",         // segment-aware prefix: /docsx is not /docs
        "/docs/../docsx/file.txt", // escape then re-enter a sibling
        "/docs/sub/../../other",   // nested escape
        "/../docs/../etc/passwd",  // leading escape, re-enter, escape again
    ] {
        let outcome = run_readwrite(&chain, outside);
        assert_failure_contains(&outcome, "denied");
    }

    // Semantically inside /docs — normalization must not deny legitimate paths. The
    // memfs is empty (no /docs directory), so the proof that the policy gate passed the
    // path through is that the failure comes from the *backend* (not-found), never the
    // gate (denied).
    for inside in [
        "/docs/file.txt",        // plain
        "/docs/sub/../file.txt", // internal .. staying inside
        "//docs///file.txt",     // repeated separators
        "/./docs/./file.txt",    // dot segments
        "/docs/file.txt/",       // trailing slash
    ] {
        let outcome = run_readwrite(&chain, inside);
        match &outcome {
            Outcome::Failure(failure) => assert!(
                !failure.value.to_lowercase().contains("denied"),
                "an inside path must not be denied by normalization: {inside:?} -> {outcome:?}"
            ),
            other => panic!("expected the backend's not-found for {inside:?}, got {other:?}"),
        }
    }

    // And with a root prefix (no subdirectory needed on the empty memfs), the same
    // tricky-but-inside spellings genuinely succeed end to end.
    let chain = closed_chain(&subtree_policy("/", "read-write"));
    for inside in ["/note.txt", "/sub/../note.txt", "//note.txt", "/./note.txt"] {
        let outcome = run_readwrite(&chain, inside);
        assert!(
            matches!(outcome, Outcome::Success(_)),
            "a legitimately-inside path must round-trip: {inside:?} -> {outcome:?}"
        );
    }
}
