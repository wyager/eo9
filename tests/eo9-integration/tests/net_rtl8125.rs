//! `net.rtl8125` — the RTL8125 driver's composition shape and typed refusals
//! (plan/09 D46).
//!
//! No host environment carries the silicon (QEMU has no RTL8125 model; the device
//! exists on the Orange Pi 5 Plus board), so what usermode pins is exactly what a
//! device-less environment can observe:
//!
//! * the driver composes like its sibling `net.virtio` — its exported l2 seals a
//!   consumer's link-layer import, leaving the pci requirement as the residual,
//! * a refused PCI capability (`pci.deny` underneath) surfaces as the *consumer's*
//!   own typed failure, never a trap or a loader error,
//! * the driver's own ring/PHY arithmetic is host-tested separately in
//!   `crates/eo9-rtl8125` (`cargo test -p eo9-rtl8125`).
//!
//! The no-device-on-the-bus refusal (typed, naming 10ec:8125) needs a pci provider
//! with an empty view, which usermode does not link — it is exercised at the QEMU
//! metal prompt (`net.rtl8125 $ l2check` under a `pci` boot grant enumerates QEMU's
//! virtio-only bus) and on the board.

use eo9_component::compose;
use eo9_integration::{guest, run};
use eo9_runtime::{Outcome, Providers};

/// `net.rtl8125 $ l2check`: l2 sealed, pci (and text) the residuals.
#[test]
fn the_driver_seals_l2_and_leaves_pci_as_the_residual() {
    guest::ensure_components(&["eo9-stub-net-rtl8125", "eo9-example-l2check"]);

    let stack = compose(
        &guest::load_stub("net.rtl8125"),
        &guest::load_example("l2check"),
    )
    .expect("net.rtl8125 $ l2check must compose");

    let info = stack.describe();
    let residual: Vec<&str> = info.imports.iter().map(|i| i.interface.as_str()).collect();
    assert!(
        !residual.iter().any(|i| i.starts_with("eo9:net/l2")),
        "the driver must seal the consumer's l2 import: {residual:?}"
    );
    assert!(
        residual.iter().any(|i| i.starts_with("eo9:pci/pci")),
        "the driver's pci requirement must stay visible as the residual: {residual:?}"
    );
}

/// `pci.deny $ net.rtl8125 $ l2check` runs against no host providers at all and the
/// program reports the refusal in its own vocabulary.
#[test]
fn a_denied_pci_capability_surfaces_as_the_consumers_typed_failure() {
    guest::ensure_components(&[
        "eo9-stub-pci-deny",
        "eo9-stub-text-null",
        "eo9-stub-net-rtl8125",
        "eo9-example-l2check",
    ]);

    let stack = compose(
        &guest::load_stub("net.rtl8125"),
        &guest::load_example("l2check"),
    )
    .expect("net.rtl8125 $ l2check");
    let stack = compose(&guest::load_stub("pci.deny"), &stack).expect("pci.deny $ …");
    let stack = compose(&guest::load_stub("text.null"), &stack).expect("text.null $ …");

    let outcome = run::run_component(&stack, &[], Providers::none());
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected l2check's own typed failure carrying the pci denial: {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}
