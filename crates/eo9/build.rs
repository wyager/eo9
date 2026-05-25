//! Build-time facts for the compile-cache key (plan/06-store.md Decisions, key v1).
//!
//! The cache key must cover the target triple and the compiler version. Both are known
//! at build time and nowhere at run time:
//!
//! * the target triple is cargo's `TARGET` (the eo9 binary only ever compiles for the
//!   host it runs on — cross-compilation of images is not part of this milestone);
//! * the compiler version is the pinned `wasmtime` version, read out of the workspace
//!   `Cargo.lock` so the string can never drift from the actual pin.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!(
        "cargo:rustc-env=EO9_TARGET_TRIPLE={}",
        env::var("TARGET").expect("cargo always sets TARGET for build scripts")
    );

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let lockfile = Path::new(&manifest_dir).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lockfile.display());
    println!(
        "cargo:rustc-env=EO9_WASMTIME_VERSION={}",
        wasmtime_version(&lockfile).unwrap_or_else(|| "unknown".to_string())
    );
}

/// The `wasmtime` package version recorded in the workspace lockfile.
fn wasmtime_version(lockfile: &Path) -> Option<String> {
    let text = fs::read_to_string(lockfile).ok()?;
    let mut in_wasmtime = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_wasmtime = false;
        } else if line == "name = \"wasmtime\"" {
            in_wasmtime = true;
        } else if in_wasmtime
            && let Some(version) = line
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        {
            return Some(version.to_string());
        }
    }
    None
}
