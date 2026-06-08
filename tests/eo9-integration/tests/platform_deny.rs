//! `platform.deny $ usb.ohci $ usbcheck` — the platform-capability refusal suite (the
//! pci_deny.rs pattern for the new hardware root; docs/board/usb-ohci-plan.md M0).
//!
//! `platform.deny` exports the full `eo9:platform/platform` surface with
//! `enumerate`/`claim` answering the API's own `denied` error, so a driver that
//! *requires* the capability can be composed into a runnable binary that observes a
//! typed refusal — instead of `platform.none`'s absence (which only satisfies
//! optional imports) or an unsatisfiable required import the loader refuses before
//! the program ever runs. The kernel root's own semantics (per-name grants, busy,
//! out-of-range, the cross-region denial) are pinned live by `check-usb`'s platcheck
//! step — they need real hardware behind them, which no host test has.

use eo9_component::compose;
use eo9_integration::{guest, run};
use eo9_runtime::{Outcome, Providers};

#[test]
fn platform_deny_seals_the_driver_and_the_program_reports_denied() {
    guest::ensure_components(&[
        "eo9-stub-platform-deny",
        "eo9-stub-usb-ohci",
        "eo9-stub-text-null",
        "eo9-example-usbcheck",
    ]);

    // `usb.ohci $ usbcheck`: the driver's exported usb seals usbcheck's required
    // import; the platform requirement bubbles up as the composition's residual.
    let stack = compose(
        &guest::load_stub("usb.ohci"),
        &guest::load_example("usbcheck"),
    )
    .expect("usb.ohci $ usbcheck must compose");
    let residual: Vec<String> = stack
        .describe()
        .imports
        .iter()
        .map(|import| import.interface.clone())
        .collect();
    assert!(
        residual.iter().any(|i| i.starts_with("eo9:platform/")),
        "the driver's platform requirement must be the stack's residual import: {residual:?}"
    );
    assert!(
        !residual.iter().any(|i| i.starts_with("eo9:usb/")),
        "usb.ohci must seal the usb import: {residual:?}"
    );

    // `platform.deny $ …`: the refusal stub seals the residual.
    let sealed = compose(&guest::load_stub("platform.deny"), &stack)
        .expect("platform.deny $ usb.ohci $ usbcheck must compose");
    let residual: Vec<String> = sealed
        .describe()
        .imports
        .iter()
        .map(|import| import.interface.clone())
        .collect();
    assert!(
        !residual.iter().any(|i| i.starts_with("eo9:platform/")),
        "platform.deny must seal the platform import: {residual:?}"
    );

    // Both layers print through eo9:text; seal it so the whole composition runs
    // against no host providers at all.
    let sealed = compose(&guest::load_stub("text.null"), &sealed).expect("text.null $ …");

    // The composition runs without any host platform provider, and the program
    // reports the refusal in its own vocabulary (usbcheck's `denied` failure case —
    // the driver mapped the capability's refusal through the usb error) — never a
    // trap, and never a loader error.
    let outcome = run::run_component(&sealed, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected usbcheck's own `denied` failure: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}

#[test]
fn the_qemu_shell_seals_usb_over_pci_and_pci_deny_refuses_it() {
    guest::ensure_components(&[
        "eo9-stub-pci-deny",
        "eo9-stub-usb-ohci-pci",
        "eo9-stub-text-null",
        "eo9-example-usbcheck",
    ]);

    // The QEMU lane's shape: `usb.ohci-pci $ usbcheck` seals usb, leaves pci residual.
    let stack = compose(
        &guest::load_stub("usb.ohci-pci"),
        &guest::load_example("usbcheck"),
    )
    .expect("usb.ohci-pci $ usbcheck must compose");
    let residual: Vec<String> = stack
        .describe()
        .imports
        .iter()
        .map(|import| import.interface.clone())
        .collect();
    assert!(
        residual.iter().any(|i| i.starts_with("eo9:pci/")),
        "the QEMU shell's pci requirement must be residual: {residual:?}"
    );
    assert!(
        !residual.iter().any(|i| i.starts_with("eo9:usb/")),
        "usb.ohci-pci must seal the usb import: {residual:?}"
    );

    // And the PCI refusal stub turns it into the same typed program failure — the
    // capability algebra is uniform across both hardware roots.
    let sealed = compose(&guest::load_stub("pci.deny"), &stack)
        .expect("pci.deny $ usb.ohci-pci $ usbcheck must compose");
    let sealed = compose(&guest::load_stub("text.null"), &sealed).expect("text.null $ …");
    let outcome = run::run_component(&sealed, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected usbcheck's own `denied` failure: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}
