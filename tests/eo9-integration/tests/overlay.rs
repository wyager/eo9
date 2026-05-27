//! `fs.overlay` — the overlay filesystem provider (SPEC.md "Overlay filesystems",
//! plan/09-providers-stubs.md Decisions 11–12).
//!
//! What runs today: the surface-contract test below — the built provider exposes exactly
//! the two named same-interface slots (`upper`, `lower`) plus the shared types/buffers
//! imports, and exports its own `eo9:fs/types` + `eo9:fs/fs`.
//!
//! The end-to-end layering test is `#[ignore]`d: wiring two *independent* component
//! leaves (each exporting its own `eo9:fs/types`) into the overlay is ill-typed at the
//! component level, because the overlay world's two `fs` imports `use` a single imported
//! `eo9:fs/types.fs-impl` (see plan/09 Decision 12 for the analysis and options). It is
//! kept compiling so it can be enabled the moment the slot-typing question is resolved.

use eo9_component::{Component, compose, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

fn overlay() -> Component {
    guest::ensure_components(&["eo9-stub-fs-overlay"]);
    guest::load_stub("fs.overlay")
}

/// The built `fs.overlay` component has the surface the spec describes: two named
/// same-interface fs slots (`upper`, `lower`), the shared types/buffers imports, and its
/// own exported `eo9:fs/types` + `eo9:fs/fs`.
#[test]
fn overlay_component_exposes_upper_and_lower_slots() {
    let overlay = overlay();
    let info = overlay.describe();

    let slots: Vec<(&str, &str)> = info
        .imports
        .iter()
        .map(|need| (need.slot.as_str(), need.interface.as_str()))
        .collect();
    assert!(
        slots.contains(&("upper", "eo9:fs/fs")),
        "missing the named `upper` eo9:fs/fs slot: {slots:?}"
    );
    assert!(
        slots.contains(&("lower", "eo9:fs/fs")),
        "missing the named `lower` eo9:fs/fs slot: {slots:?}"
    );
    assert!(
        info.imports.iter().all(|need| need.interface != "eo9:fs/fs"
            || need.slot == "upper"
            || need.slot == "lower"),
        "no default-slot fs import is expected: {slots:?}"
    );

    let exports: Vec<&str> = info
        .exports
        .iter()
        .map(|export| export.interface.as_str())
        .collect();
    assert!(
        exports.contains(&"eo9:fs/fs"),
        "the overlay must export eo9:fs/fs: {exports:?}"
    );
    assert!(
        exports.contains(&"eo9:fs/types"),
        "the overlay must export its own eo9:fs/types: {exports:?}"
    );

    // Renaming a named slot is the `with … as …` building block; it must work on both.
    rename(&overlay, "upper", "primary").expect("the upper slot should be renameable");
    rename(&overlay, "lower", "secondary").expect("the lower slot should be renameable");
}

/// The intended end-to-end check: `with memfs-A as upper, memfs-B as lower $ fs.overlay
/// $ readwrite` — a write lands in the lower layer and reads back through the overlay.
///
/// Ignored: two independent leaves cannot currently be wired into the overlay's two
/// same-interface slots, because both slots' `fs-impl` is constrained to the overlay's
/// single imported `eo9:fs/types` while each standalone fs provider exports its own
/// fresh `types` resource — the fused component fails validation. plan/09 Decision 12
/// records the options (runtime-side two-slot linking for the host providers, or a WIT
/// change giving each fs import its own root-handle type). Enable once that lands.
#[test]
#[ignore = "blocked on per-slot fs root-handle typing — see plan/09 Decision 12"]
fn readwrite_through_the_overlay_round_trips() {
    guest::ensure_components(&[
        "eo9-stub-fs-memfs",
        "eo9-stub-fs-overlay",
        "eo9-example-readwrite",
    ]);
    let upper_leaf =
        rename(&guest::load_stub("fs.memfs"), "eo9:fs/fs", "upper").expect("rename to upper");
    let lower_leaf =
        rename(&guest::load_stub("fs.memfs"), "eo9:fs/fs", "lower").expect("rename to lower");

    // with A as upper, B as lower $ fs.overlay
    let stack = compose(
        &upper_leaf,
        &compose(&lower_leaf, &overlay()).expect("lower $ overlay"),
    )
    .expect("upper $ (lower $ overlay)");

    // … $ readwrite: the program's writes go to the lower memfs and read back through
    // the overlay's fall-through path.
    let program = compose(&stack, &guest::load_component("eo9-example-readwrite"))
        .expect("overlay stack $ readwrite");
    let outcome = run::run_component(
        &program,
        &[
            NamedArg::new("path", "\"note.txt\""),
            NamedArg::new("contents", "\"hello-overlay\""),
        ],
        Providers::none(),
    );
    assert!(
        matches!(outcome, Outcome::Success(_)),
        "expected a round-trip through the overlay, got {outcome:?}"
    );
}
