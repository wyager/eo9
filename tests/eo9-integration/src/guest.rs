//! Locating (and, when missing, building) the real guest components — the examples and
//! the standard stub providers from `guest/` — so the integration suites can compose and
//! run them.
//!
//! `cargo xtask ci` runs the host-workspace tests *before* `build-guest`, and a fresh
//! checkout has no `guest/target/components` at all, so any suite that needs real guest
//! components builds them on demand (once per test process) by invoking
//! `cargo run -p xtask -- build-guest` — the same convention the runtime's and the CLI's
//! own integration tests use. On a warm tree this is a cheap existence check.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use eo9_component::Component;

/// The repository root (the directory holding `Cargo.toml`, `guest/`, `wit/`, …).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root must exist")
}

/// Where `xtask build-guest` puts componentized guest artifacts.
pub fn components_dir() -> PathBuf {
    repo_root().join("guest/target/components")
}

/// The path of a built guest component, by package name (e.g. `eo9-stub-time-frozen`,
/// `eo9-example-hello`).
pub fn component_path(package: &str) -> PathBuf {
    components_dir().join(format!("{package}.wasm"))
}

/// Ensure the named guest components exist, building the guest workspace once (per test
/// process) if any are missing. Panics if they are still missing afterwards.
pub fn ensure_components(packages: &[&str]) {
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

/// Load a built guest component (building the guest workspace if needed) as a validated
/// [`Component`] value.
pub fn load_component(package: &str) -> Component {
    ensure_components(&[package]);
    let path = component_path(package);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    Component::load(bytes)
        .unwrap_or_else(|err| panic!("guest component {package} failed to load: {err}"))
}

/// Load a standard stub provider by its dotted stub name (e.g. `time.frozen`,
/// `entropy.seeded`, `fs.memfs`, `text.null`).
pub fn load_stub(stub: &str) -> Component {
    let package = format!("eo9-stub-{}", stub.replace('.', "-"));
    load_component(&package)
}

/// Load an example program by its short name (e.g. `hello`, `readwrite`).
pub fn load_example(example: &str) -> Component {
    load_component(&format!("eo9-example-{example}"))
}
