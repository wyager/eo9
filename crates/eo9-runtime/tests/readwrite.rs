//! End-to-end run of the real `eo9-example-readwrite` component (area 07): an async
//! `main` that awaits the async `eo9:fs/fs` operations and round-trips its argument
//! through the owned-buffer `read`/`write` path, served here by the in-memory fs provider.
//!
//! The component is the one `cargo xtask build-guest` produces. If it has not been built
//! yet (plain `cargo test` before any build-guest), the test builds it the same way xtask
//! does; wasm-tools is on PATH per the area-01 toolchain pin.

use std::path::{Path, PathBuf};
use std::process::Command;

use eo9_runtime::providers::MemFs;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    EngineOptions, Image, NamedArg, Outcome, Providers, ResumeOutcome, SpawnLimits, Task,
    new_engine,
};

/// Locate (building if necessary) the componentized readwrite example.
fn readwrite_component_path() -> PathBuf {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives at <repo>/crates/eo9-runtime")
        .to_path_buf();
    let guest_dir = repo_root.join("guest");
    let component = guest_dir.join("target/components/eo9-example-readwrite.wasm");
    if component.exists() {
        return component;
    }

    // Same steps as `cargo xtask build-guest`, limited to the one package we need.
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            "eo9-example-readwrite",
        ])
        .current_dir(&guest_dir)
        .env_remove("RUSTUP_TOOLCHAIN")
        .status()
        .expect("failed to run cargo for the guest workspace");
    assert!(status.success(), "guest build failed");

    std::fs::create_dir_all(component.parent().unwrap()).unwrap();
    let module = guest_dir.join("target/wasm32-unknown-unknown/release/eo9_example_readwrite.wasm");
    let status = Command::new("wasm-tools")
        .arg("component")
        .arg("new")
        .arg(&module)
        .arg("-o")
        .arg(&component)
        .status()
        .expect("failed to run wasm-tools (componentize)");
    assert!(status.success(), "componentizing the guest failed");
    component
}

fn run_readwrite(memfs: &MemFs, path: &str, contents: &str) -> Outcome {
    let engine = new_engine(&EngineOptions::default()).unwrap();
    let bytes = std::fs::read(readwrite_component_path()).unwrap();
    let image = Image::compile(&engine, bytes).unwrap();

    let mut task = Task::spawn(
        &image,
        &[
            NamedArg::new("path", format!("\"{path}\"")),
            NamedArg::new("contents", format!("\"{contents}\"")),
        ],
        SpawnLimits::default(),
        Providers {
            fs: Some(Box::new(memfs.clone())),
            ..Providers::none()
        },
    )
    .unwrap();

    loop {
        match task.resume(100 * FUEL_QUANTUM) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => continue,
            ResumeOutcome::Blocked => {
                // The in-memory provider completes everything inline, so a blocked task
                // would never wake up again — fail loudly instead of hanging.
                panic!("readwrite blocked on the in-memory fs");
            }
        }
    }
}

#[test]
fn readwrite_example_round_trips_through_the_in_memory_fs() {
    let memfs = MemFs::new();
    let outcome = run_readwrite(&memfs, "/scratch/note.txt", "hello eo9 disk");

    // The program reports its own success vocabulary: round-tripped(bytes-written).
    match &outcome {
        Outcome::Success(value) => {
            assert!(
                value.value.contains("round-tripped"),
                "unexpected success value: {} : {}",
                value.ty,
                value.value
            );
            assert!(
                value.value.contains("14"),
                "expected 14 bytes written, got: {}",
                value.value
            );
        }
        other => panic!("expected success, got {other:?}"),
    }

    // And the write really landed in the provider.
    assert_eq!(
        memfs.file_contents("/scratch/note.txt").as_deref(),
        Some(b"hello eo9 disk".as_slice())
    );
}

#[test]
fn readwrite_example_reports_fs_errors_in_its_own_vocabulary() {
    // Pre-create a *directory* where the program wants a file: open fails with
    // `is-a-directory` and the program maps that into its `fs(...)` failure case.
    let memfs = MemFs::new();
    memfs.insert_dir("/scratch");
    let outcome = run_readwrite(&memfs, "/scratch", "irrelevant");

    match &outcome {
        Outcome::Failure(value) => {
            assert!(
                value.value.contains("fs"),
                "expected the program's fs(...) failure case, got: {}",
                value.value
            );
        }
        other => panic!("expected the program's own failure, got {other:?}"),
    }
}
