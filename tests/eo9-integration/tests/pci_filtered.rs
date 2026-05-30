//! `pci.filtered --allow [{…}]` -- the compound-configured attenuator (plan/03 D21
//! acceptance). The allow-list is a `list<device-address>` (a list of records), which
//! only bakes under the alias + bind configure construction; this exercises compound
//! argument lowering, configuration of a resource-owning provider (`eo9:pci/pci`), bind
//! propagation through `$`, and the loader still refusing the composition in usermode
//! (where no pci root provider exists -- the metal shell is where it runs).

use eo9_component::{compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::{Providers, SpawnLimits, Task};

#[test]
fn pci_filtered_allow_list_bakes_and_composes() {
    guest::ensure_components(&["eo9-stub-pci-filtered", "eo9-example-lspci"]);

    let configured = configure(
        &guest::load_stub("pci.filtered"),
        &[("allow", "[{segment: 0, bus: 0, device: 1, function: 0}]")],
    )
    .expect("configure(pci.filtered, --allow [{…}]) must bake under alias + bind");

    // The configured attenuator: pci re-exported, config sealed, bind entrypoint carried.
    let info = configured.describe();
    let exports: Vec<&str> = info.exports.iter().map(|e| e.interface.as_str()).collect();
    assert!(exports.contains(&"eo9:pci/pci"), "{exports:?}");
    assert!(exports.contains(&"eo9:rt/configured"), "{exports:?}");
    assert!(
        !exports.contains(&"eo9:pci/filtered-config"),
        "the config interface must be sealed away: {exports:?}"
    );

    // It composes with lspci into a binary that still needs an underlying pci provider
    // (the attenuator filters a capability someone below must grant).
    let bound = compose(&configured, &guest::load_example("lspci"))
        .expect("configure(pci.filtered, …) $ lspci");
    let bound_info = bound.describe();
    let needs: Vec<&str> = bound_info
        .imports
        .iter()
        .map(|i| i.interface.as_str())
        .collect();
    assert!(
        needs.contains(&"eo9:pci/pci"),
        "the filtered attenuator's own underlying pci import must remain: {needs:?}"
    );

    // Usermode has no pci root provider, so spawning refuses -- with the loader's typed
    // missing-capability refusal naming pci, not a trap and not an algebra error. (The
    // composition itself runs on the metal shell, where the kernel grants pci.)
    let image = run::compile_component(&bound);
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none())
        .map(|_| ())
        .expect_err("usermode has no pci provider; the spawn must refuse");
    let reason = format!("{err:?}");
    assert!(
        reason.contains("eo9:pci"),
        "the refusal must name the missing pci capability: {reason}"
    );
}
