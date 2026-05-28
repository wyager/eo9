//! `fs.overlay` — the overlay filesystem provider (SPEC.md "Overlay filesystems",
//! plan/09-providers-stubs.md Decisions 11–13).
//!
//! With `fs-impl` declared in the `fs` interface itself (SPEC "Multi-instance imports and
//! type identity"), each named slot of the overlay mints its own root-handle type, so two
//! *independent* guest filesystems can be wired into `upper`/`lower` and the fused
//! component encodes and validates — the type-identity blocker of plan/09 Decision 12 is
//! gone. And with the default-configuration rule (plan/09 Decision 14), an unconfigured
//! `fs.memfs` leaf self-binds its documented default (the empty filesystem) instead of
//! trapping, so the behavioral round-trip below runs end to end as well.

use eo9_component::{Component, compose, rename};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

fn overlay() -> Component {
    guest::ensure_components(&["eo9-stub-fs-overlay"]);
    guest::load_stub("fs.overlay")
}

/// The built `fs.overlay` component has the surface the spec describes: two named
/// same-interface fs slots (`upper`, `lower`), each minting its own root-handle type, the
/// shared buffers import, and a single exported `eo9:fs/fs` (no types-only sibling — the
/// root handle lives in the interface itself).
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
    // The fs slots are real capability asks (functions, not just types), and there is no
    // separate eo9:fs/types import anymore — the root handle lives in the interface.
    assert!(
        info.imports
            .iter()
            .filter(|need| need.interface == "eo9:fs/fs")
            .all(|need| !need.authority_free),
        "the fs slots must carry authority: {slots:?}"
    );
    assert!(
        info.imports
            .iter()
            .all(|need| need.interface != "eo9:fs/types"),
        "no eo9:fs/types import is expected after the root-handle move: {slots:?}"
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

    // Renaming a named slot is the `with … as …` building block; it must work on both.
    rename(&overlay, "upper", "primary").expect("the upper slot should be renameable");
    rename(&overlay, "lower", "secondary").expect("the lower slot should be renameable");
}

/// Guest-leaf layering is now well-typed: `with memfs-A as upper, memfs-B as lower $
/// fs.overlay $ readwrite` — two independent memfs instances wired into the two
/// same-interface slots — composes, encodes, and validates (every `compose` call below
/// re-encodes and re-validates the fused component), and the result is a binary whose fs
/// need is fully satisfied by the overlay. Before the root-handle move this exact
/// construction failed validation (plan/09 Decision 12).
#[test]
fn guest_leaf_layering_composes_and_validates() {
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

    // The stack is a provider exporting eo9:fs/fs with both slots sealed.
    let stack_info = stack.describe();
    assert!(
        stack_info
            .imports
            .iter()
            .all(|need| need.interface != "eo9:fs/fs"),
        "both fs slots must be sealed by the leaves: {:?}",
        stack_info.imports
    );

    // … $ readwrite: the program's fs import is satisfied by the overlay's export.
    let program = compose(&stack, &guest::load_component("eo9-example-readwrite"))
        .expect("overlay stack $ readwrite");
    let info = program.describe();
    assert!(
        info.imports
            .iter()
            .all(|need| need.interface != "eo9:fs/fs"),
        "the program's fs import must be satisfied by the overlay: {:?}",
        info.imports
    );
}

/// The behavioral round-trip: the program's writes land in the lower memfs leaf and read
/// back through the overlay's fall-through path. The leaves are composed unconfigured —
/// their `memfs-config` interfaces are dropped by the slot wiring — and run on the
/// documented default (an empty filesystem) per plan/09 Decision 14, which is what
/// closed the configuration half of plan/09 Decision 13.
#[test]
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
