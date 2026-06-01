//! `pci.deny $ lspci` — the PCI refusal stub (study 09 finding 4 / plan/12 D62).
//!
//! `pci.deny` exports the full `eo9:pci/pci` surface with `enumerate`/`open` answering
//! the API's own `denied` error, so a program that *requires* PCI can be composed into
//! a runnable binary that observes a typed refusal — instead of `pci.none`'s absence
//! (which only satisfies optional imports) or an unsatisfiable required import that the
//! loader refuses before the program ever runs.

use eo9_component::compose;
use eo9_integration::{guest, run};
use eo9_runtime::{Outcome, Providers};

#[test]
fn pci_deny_seals_the_import_and_the_program_reports_denied() {
    guest::ensure_components(&[
        "eo9-stub-pci-deny",
        "eo9-stub-text-null",
        "eo9-example-lspci",
    ]);

    // `pci.deny $ lspci`: the stub's exported pci seals lspci's required import.
    let sealed = compose(&guest::load_stub("pci.deny"), &guest::load_example("lspci"))
        .expect("pci.deny $ lspci must compose");

    let info = sealed.describe();
    let residual: Vec<&str> = info.imports.iter().map(|i| i.interface.as_str()).collect();
    assert!(
        !residual.iter().any(|i| i.starts_with("eo9:pci/")),
        "pci.deny must seal the pci import (no residual pci requirement): {residual:?}"
    );

    // lspci also prints through eo9:text; seal that with text.null so the whole
    // composition runs against no host providers at all.
    let sealed = compose(&guest::load_stub("text.null"), &sealed).expect("text.null $ …");

    // The composition runs without any host pci provider, and the program reports the
    // refusal in its own vocabulary (lspci's `denied` failure case) — never a trap, and
    // never a loader error.
    let outcome = run::run_component(&sealed, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected lspci's own `denied` failure: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}
