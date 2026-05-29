//! `fs.eofs` — Eo9's native filesystem as a provider component (plan/14-eofs.md, M2).
//!
//! eofs itself (the on-disk format, copy-on-write engine, snapshots, compression,
//! hashing, crash consistency, persistence across mounts) is exercised by
//! `crates/eofs-core`'s own test suite. These tests cover the provider layer: the
//! component has the disk-to-fs shape the plan describes, the algebraic chain
//! `disk.mem $ fs.eofs $ program` composes and validates, and a real program round-trips
//! its data through eofs over an in-memory disk — including the documented defaults
//! (an unconfigured `disk.mem` self-binds its 16 MiB device, and `fs.eofs` formats the
//! blank disk on first use), so the whole chain runs without any `configure`.

use eo9_component::{Component, compose};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

fn eofs() -> Component {
    guest::ensure_components(&["eo9-stub-fs-eofs"]);
    guest::load_stub("fs.eofs")
}

/// The built `fs.eofs` component has the milestone-2 surface: it asks for a raw block
/// device (`eo9:disk/disk`, a real capability need) and provides `eo9:fs/fs`; it does not
/// itself ask for any filesystem.
#[test]
fn eofs_component_has_the_disk_to_fs_shape() {
    let eofs = eofs();
    let info = eofs.describe();

    let imports: Vec<&str> = info
        .imports
        .iter()
        .map(|need| need.interface.as_str())
        .collect();
    assert!(
        imports.contains(&"eo9:disk/disk"),
        "fs.eofs must import the disk capability: {imports:?}"
    );
    assert!(
        info.imports
            .iter()
            .filter(|need| need.interface == "eo9:disk/disk")
            .all(|need| !need.authority_free),
        "the disk import must carry authority: {imports:?}"
    );
    assert!(
        !imports.contains(&"eo9:fs/fs"),
        "fs.eofs provides the filesystem, it must not ask for one: {imports:?}"
    );

    let exports: Vec<&str> = info
        .exports
        .iter()
        .map(|export| export.interface.as_str())
        .collect();
    assert!(
        exports.contains(&"eo9:fs/fs"),
        "fs.eofs must export eo9:fs/fs: {exports:?}"
    );
}

/// `disk.mem $ fs.eofs` composes into a self-contained filesystem provider: the disk need
/// is sealed by the leaf and the stack still offers `eo9:fs/fs`.
#[test]
fn disk_mem_seals_the_eofs_disk_need() {
    guest::ensure_components(&["eo9-stub-disk-mem", "eo9-stub-fs-eofs"]);
    let stack =
        compose(&guest::load_stub("disk.mem"), &eofs()).expect("disk.mem $ fs.eofs must compose");

    let info = stack.describe();
    assert!(
        info.imports
            .iter()
            .all(|need| !need.interface.starts_with("eo9:disk/")),
        "the disk need must be sealed by disk.mem: {:?}",
        info.imports
    );
    assert!(
        info.exports
            .iter()
            .any(|export| export.interface == "eo9:fs/fs"),
        "the stack must still export eo9:fs/fs: {:?}",
        info.exports
    );
}

/// The behavioral round-trip over the full chain `disk.mem $ fs.eofs $ readwrite`: the
/// program creates a file, writes through the owned-buffer path, reads it back, and
/// compares — every byte travelling through the eofs engine and landing on the (in-memory)
/// block device. Everything runs on documented defaults: no `configure` anywhere, the
/// blank disk is formatted by `fs.eofs` on first use.
#[test]
fn readwrite_over_eofs_round_trips() {
    guest::ensure_components(&[
        "eo9-stub-disk-mem",
        "eo9-stub-fs-eofs",
        "eo9-example-readwrite",
    ]);
    let stack =
        compose(&guest::load_stub("disk.mem"), &eofs()).expect("disk.mem $ fs.eofs must compose");
    let program = compose(&stack, &guest::load_component("eo9-example-readwrite"))
        .expect("disk.mem $ fs.eofs $ readwrite must compose");

    let info = program.describe();
    assert!(
        info.imports
            .iter()
            .all(|need| need.interface != "eo9:fs/fs" && !need.interface.starts_with("eo9:disk/")),
        "fs and disk must both be sealed inside the chain: {:?}",
        info.imports
    );

    let outcome = run::run_component(
        &program,
        &[
            NamedArg::new("path", "\"boot.log\""),
            NamedArg::new("contents", "\"hello from eofs\""),
        ],
        Providers::none(),
    );
    match &outcome {
        Outcome::Success(_) => {}
        other => panic!("expected a round-trip through eofs, got {other:?}"),
    }
    assert!(
        run::success_value(&outcome).starts_with("round-tripped("),
        "unexpected success value: {}",
        run::success_value(&outcome)
    );
}

/// Two runs of the identical chain produce the identical outcome: the whole stack —
/// disk.mem's default device, eofs's formatting and allocation, the program — is
/// deterministic by construction (no clock, no entropy anywhere in the chain).
#[test]
fn eofs_round_trip_is_deterministic() {
    guest::ensure_components(&[
        "eo9-stub-disk-mem",
        "eo9-stub-fs-eofs",
        "eo9-example-readwrite",
    ]);
    let stack =
        compose(&guest::load_stub("disk.mem"), &eofs()).expect("disk.mem $ fs.eofs must compose");
    let program = compose(&stack, &guest::load_component("eo9-example-readwrite"))
        .expect("disk.mem $ fs.eofs $ readwrite must compose");

    let args = [
        NamedArg::new("path", "\"again.txt\""),
        NamedArg::new("contents", "\"determinism\""),
    ];
    let first = run::run_component(&program, &args, Providers::none());
    let second = run::run_component(&program, &args, Providers::none());
    assert!(
        matches!(first, Outcome::Success(_)),
        "first run must succeed: {first:?}"
    );
    assert_eq!(
        run::success_value(&first),
        run::success_value(&second),
        "the eofs chain must be deterministic across runs"
    );
}
