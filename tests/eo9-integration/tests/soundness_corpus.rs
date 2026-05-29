//! Encoder-soundness corpus: the obligation the PL user study asked for (study 05,
//! finding 6) in its bounded, deterministic form — for every provider/consumer pair in
//! a corpus of the real shipped components, `$` either succeeds (and the result is a
//! validated component, which `compose` guarantees by re-loading it) or fails with a
//! *typed* error; it never reports an internal wiring/encoding failure. The fully
//! generative property suite over synthetic component triples is tracked in plan/13.

use eo9_component::{ComposeError, compose};
use eo9_integration::guest;

/// Shipped stub providers exercised as the left operand.
const PROVIDERS: &[&str] = &[
    "time.frozen",
    "time.fuzzy",
    "time.monotonic-stub",
    "time.none",
    "entropy.seeded",
    "entropy.none",
    "fs.memfs",
    "fs.none",
    "fs.readonly",
    "fs.overlay",
    "text.null",
    "text.none",
    "net.l2.none",
    "net.l2.deny",
    "net.l3.none",
    "net.l3.deny",
    "net.l4.none",
    "net.l4.deny",
    "net.l4.loopback",
    "perf.null",
    "disk.mem",
    "pci.none",
];

/// Shipped programs (and one provider) exercised as the right operand.
const CONSUMERS: &[&str] = &[
    "eo9-example-hello",
    "eo9-example-readwrite",
    "eo9-example-sockcheck",
    "eo9-example-cruncher",
    "eo9-coreutil-cat",
    "eo9-coreutil-rng",
    "eo9-stub-time-fuzzy",
];

#[test]
fn compose_over_the_shipped_corpus_is_sound() {
    let provider_packages: Vec<String> = PROVIDERS
        .iter()
        .map(|stub| format!("eo9-stub-{}", stub.replace('.', "-")))
        .collect();
    let mut all: Vec<&str> = provider_packages.iter().map(String::as_str).collect();
    all.extend_from_slice(CONSUMERS);
    guest::ensure_components(&all);

    let mut failures = Vec::new();
    for provider_name in PROVIDERS {
        let provider = guest::load_stub(provider_name);
        for consumer_name in CONSUMERS {
            let consumer = guest::load_component(consumer_name);
            match compose(&provider, &consumer) {
                // A validated component (compose re-loads the encoded bytes) or a typed
                // refusal are both sound outcomes.
                Ok(_) | Err(ComposeError::NotAProvider) | Err(ComposeError::TypeMismatch(_)) => {}
                Err(ComposeError::Internal(message)) => {
                    failures.push(format!("{provider_name} $ {consumer_name}: {message}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "compose reported internal failures over the shipped corpus:\n{}",
        failures.join("\n")
    );
}
