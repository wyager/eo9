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
/// The eo9 CLI binary, rebuilt EVERY test run (under a Once) with the bundle-freshness
/// gate: the binary embeds `crates/eo9-bundled-programs/data/*.wasm` at ITS build
/// time, so a guest-source edit tests OLD bytes until `refresh-components` runs — a
/// stale binary silently tests fixed code (the area/52 lane hit exactly this; the
/// svc_shell stale-binary flake was the same class). Two halves, both closed here:
/// build-on-absence (always build instead) and bundle staleness (compare the built
/// component against the committed bundle byte and fail with the instruction).
pub fn fresh_eo9_binary() -> PathBuf {
    static BUILD: Once = Once::new();
    let profile_dir = std::env::current_exe()
        .expect("test executable path")
        .parent()
        .expect("deps dir")
        .parent()
        .expect("profile dir")
        .to_path_buf();
    let binary = profile_dir.join("eo9");
    BUILD.call_once(|| {
        // Freshness gate: when both the built component and the committed bundle
        // bytes exist, they must agree — the binary embeds the BUNDLE, so a stale
        // bundle means the test exercises old guest code whatever we build below.
        let root = repo_root();
        for name in ["init", "eosh"] {
            let built = root
                .join("guest/target/components")
                .join(format!("{name}.wasm"));
            let bundled = root
                .join("crates/eo9-bundled-programs/data")
                .join(format!("{name}.wasm"));
            if built.exists()
                && bundled.exists()
                && std::fs::read(&built).ok() != std::fs::read(&bundled).ok()
            {
                panic!(
                    "the committed bundle is stale for `{name}`: the eo9 binary embeds                      crates/eo9-bundled-programs/data/{name}.wasm, which differs from the                      freshly built guest/target/components/{name}.wasm — run                      `cargo xtask refresh-components` first, or this test silently                      exercises old guest bytes"
                );
            }
        }
        let mut args = vec!["build", "-p", "eo9", "--bin", "eo9"];
        if profile_dir.file_name().and_then(|n| n.to_str()) == Some("release") {
            args.push("--release");
        }
        let status = Command::new("cargo")
            .args(&args)
            .current_dir(root)
            .status()
            .expect("failed to invoke cargo to build the eo9 binary");
        assert!(status.success(), "building the eo9 binary failed");
    });
    assert!(binary.exists(), "eo9 binary missing after the build");
    binary
}

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
