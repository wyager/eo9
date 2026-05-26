//! Build-time facts for the compile-cache key (plan/06-store.md Decisions, key v1) and
//! the embedded component set.
//!
//! The cache key must cover the target triple and the compiler version. Both are known
//! at build time and nowhere at run time:
//!
//! * the target triple is cargo's `TARGET` (the eo9 binary only ever compiles for the
//!   host it runs on — cross-compilation of images is not part of this milestone);
//! * the compiler version is the pinned `wasmtime` version, read out of the workspace
//!   `Cargo.lock` so the string can never drift from the actual pin.
//!
//! The **embedded component set** is everything `cargo xtask build-guest` has produced
//! under `guest/target/components/` at the time this crate is built: eosh, the examples,
//! and the standard stubs, baked into the binary so a freshly installed `eo9` can seed an
//! empty store and offer a working shell out of the box. When that directory is absent
//! (a fresh checkout built before `build-guest`) the set is simply empty and the dev-tree
//! fallbacks keep working; packaged/release builds must run `cargo xtask build-guest`
//! before building this crate.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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

    embed_components(Path::new(&manifest_dir));
}

/// Generate `$OUT_DIR/embedded_components.rs`: the `(file stem, bytes)` list of every
/// built component found under `guest/target/components/`, or an empty list when none
/// have been built yet.
fn embed_components(manifest_dir: &Path) {
    let components_dir = manifest_dir.join("../../guest/target/components");
    // Re-run when the directory appears or its entry set changes; the per-file lines
    // below cover rebuilds of existing components (a rewrite does not always bump the
    // directory's own mtime).
    println!("cargo:rerun-if-changed={}", components_dir.display());

    let mut components: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&components_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("wasm") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let path = path.canonicalize().expect("built component path resolves");
            println!("cargo:rerun-if-changed={}", path.display());
            components.push((stem.to_string(), path));
        }
    }
    components.sort();

    let mut code = String::from(
        "/// The components baked into this binary at build time (file stem, bytes):\n\
         /// everything `cargo xtask build-guest` had produced when the binary was built.\n\
         pub(crate) static EMBEDDED_COMPONENTS: &[(&str, &[u8])] = &[\n",
    );
    for (stem, path) in &components {
        code.push_str(&format!(
            "    ({stem:?}, include_bytes!({:?}) as &[u8]),\n",
            path.display()
        ));
    }
    code.push_str("];\n");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("embedded_components.rs");
    fs::write(&out, code).expect("can write the embedded component list");
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
