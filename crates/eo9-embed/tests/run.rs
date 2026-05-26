//! End-to-end tests: embed an Eo9 instance and run real guest components.
//!
//! Mirrors the convention used by the other host-side suites: the guest components are
//! built on demand (once per test process) via `cargo run -p xtask -- build-guest`, since
//! `cargo xtask ci` runs the host tests before building the guest workspace.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use eo9_embed::{EmbedError, Eo9, Grants, NamedArg, Outcome, Sandbox};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root must exist")
}

fn component_path(package: &str) -> PathBuf {
    repo_root()
        .join("guest/target/components")
        .join(format!("{package}.wasm"))
}

fn ensure_components(packages: &[&str]) {
    static BUILD: Once = Once::new();
    if packages.iter().all(|name| component_path(name).exists()) {
        return;
    }
    BUILD.call_once(|| {
        let status = Command::new("cargo")
            .args(["run", "-p", "xtask", "--", "build-guest"])
            .current_dir(repo_root())
            .status()
            .expect("failed to invoke `cargo run -p xtask -- build-guest`");
        assert!(status.success(), "`cargo xtask build-guest` failed");
    });
    for package in packages {
        assert!(
            component_path(package).exists(),
            "guest component {} is still missing after build-guest",
            component_path(package).display()
        );
    }
}

fn example_bytes(name: &str) -> Vec<u8> {
    let package = format!("eo9-example-{name}");
    ensure_components(&[&package]);
    std::fs::read(component_path(&package)).expect("read example component")
}

#[test]
fn sandbox_runs_hello_and_captures_its_greeting() {
    let bytes = example_bytes("hello");
    let sandbox = Sandbox::new();
    let eo9 = Eo9::builder().backend(sandbox.clone()).build().unwrap();

    let outcome = eo9
        .run_bytes(
            &bytes,
            &[
                NamedArg::new("name", "\"sandbox\""),
                NamedArg::new("excited", "true"),
            ],
        )
        .unwrap();

    assert!(
        matches!(outcome, Outcome::Success(_)),
        "hello should succeed, got {outcome:?}"
    );
    assert!(
        sandbox.stdout().contains("sandbox"),
        "the greeting should be captured by the sandbox, got {:?}",
        sandbox.stdout()
    );
}

#[test]
fn sandbox_cruncher_is_deterministic_across_runs() {
    let bytes = example_bytes("cruncher");
    let args = [NamedArg::new("seed", "9"), NamedArg::new("rounds", "50000")];

    let run = || {
        let eo9 = Eo9::builder()
            .grants(Grants::none())
            .backend(Sandbox::new())
            .build()
            .unwrap();
        match eo9.run_bytes(&bytes, &args).unwrap() {
            Outcome::Success(value) => value.value,
            other => panic!("cruncher should succeed, got {other:?}"),
        }
    };

    let first = run();
    let second = run();
    assert_eq!(
        first, second,
        "the same seed/rounds must give the same digest"
    );
}

#[test]
fn sandbox_readwrite_round_trips_through_the_in_memory_fs() {
    let bytes = example_bytes("readwrite");
    let sandbox = Sandbox::new();
    let eo9 = Eo9::builder()
        .backend(sandbox.clone())
        .grant_fs(true)
        .build()
        .unwrap();

    let outcome = eo9
        .run_bytes(
            &bytes,
            &[
                NamedArg::new("path", "\"/scratch.txt\""),
                NamedArg::new("contents", "\"embedded payload\""),
            ],
        )
        .unwrap();

    assert!(
        matches!(outcome, Outcome::Success(_)),
        "readwrite should succeed against the in-memory fs, got {outcome:?}"
    );
    assert_eq!(
        sandbox.file_contents("/scratch.txt").as_deref(),
        Some(b"embedded payload".as_slice()),
        "the written file should be visible in the sandbox filesystem",
    );
}

#[test]
fn a_program_that_requires_fs_is_refused_without_an_fs_grant() {
    let bytes = example_bytes("readwrite");
    // Default grants: text/time/entropy, no fs.
    let eo9 = Eo9::builder().backend(Sandbox::new()).build().unwrap();
    let err = eo9
        .run_bytes(&bytes, &[NamedArg::new("path", "\"/x\"")])
        .unwrap_err();
    assert!(
        matches!(err, EmbedError::MissingCapability(_)),
        "expected a clear missing-capability error, got {err:?}"
    );
}

#[cfg(feature = "host")]
#[test]
fn host_backend_rejects_an_fs_grant_without_a_configured_root() {
    use eo9_embed::Host;
    let bytes = example_bytes("cruncher");
    // fs is granted but the Host backend has no root configured.
    let eo9 = Eo9::builder()
        .backend(Host::new())
        .grant_fs(true)
        .build()
        .unwrap();
    let err = eo9
        .run_bytes(
            &bytes,
            &[NamedArg::new("seed", "1"), NamedArg::new("rounds", "1")],
        )
        .unwrap_err();
    assert!(
        matches!(err, EmbedError::Provider(_)),
        "expected a provider configuration error, got {err:?}"
    );
}
