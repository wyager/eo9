//! Build script for the bare-metal kernel.
//!
//! For bare-metal targets (`target_os = "none"`) this injects the architecture's linker
//! script, which lays the image out for QEMU's `virt` machine. When the `wasm-seed` / `wasm-hello`
//! features are enabled it additionally checks that the host-precompiled artifacts were
//! supplied (via the `EO9_SEED_CWASM` / `EO9_HELLO_CWASM` environment variables set by
//! `cargo xtask build-kernel`), so a bad invocation fails here with a clear message
//! instead of deep inside `include_bytes!`.

use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // ---- the compiler fingerprint (docs/spikes/spawn-latency.md, plan/12 entry 73) ----
    // blake3 over the vendored compiler sources (kernel/vendor/**) plus the engine-config
    // sources. Every *persistent* compiled-artifact cache key includes it, so a vendored
    // cranelift/wasmtime change (including a miscompile fix) or an engine-config change
    // makes old entries an unreachable clean miss — no human REV bump required, no
    // stale-but-compatible artifact ever served. Codegen target features are build-time
    // fixed (the engine sets an explicit target + explicit cranelift flags; the only
    // runtime probe is x86's load-time CPUID *verification*), so no runtime-detected
    // feature set needs to join the key; that fact is part of what hashing mod.rs pins.
    emit_compiler_fingerprint();

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "none" {
        // Host-triple builds (unit tests) compile the stub entry point; nothing to do.
        return;
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let script = match arch.as_str() {
        "aarch64" => "linker-aarch64.ld",
        "riscv64" => "linker-riscv64.ld",
        "x86_64" => "linker-x86_64.ld",
        other => panic!(
            "no linker script for target arch `{other}`: the bare-metal kernel covers aarch64, \
             riscv64 and x86_64 so far (plan/12-kernel.md)"
        ),
    };
    let linker_script = Path::new(&manifest_dir).join(script);
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg-bins=-T{}", linker_script.display());

    require_artifact_env("WASM_SEED", "EO9_SEED_CWASM", "seed component");
    require_artifact_env("WASM_HELLO", "EO9_HELLO_CWASM", "eo9-example-hello program");
}

/// If the cargo feature named by `CARGO_FEATURE_<feature>` is enabled, require `env_var`
/// to point at the host-precompiled artifact it embeds, failing with a clear message
/// otherwise.
fn require_artifact_env(feature: &str, env_var: &str, what: &str) {
    println!("cargo:rerun-if-env-changed={env_var}");
    if env::var(format!("CARGO_FEATURE_{feature}")).is_err() {
        return;
    }
    match env::var(env_var) {
        Ok(path) => println!("cargo:rerun-if-changed={path}"),
        Err(_) => panic!(
            "this feature needs the {env_var} environment variable to point at the \
             host-precompiled {what}; build the kernel via `cargo xtask build-kernel <arch>`, \
             which precompiles it and sets the variable"
        ),
    }
}

/// See main(): blake3 over kernel/vendor/** + the engine-config sources, emitted as the
/// `EO9_COMPILER_FINGERPRINT` env for `env!` in diskcache.rs/shellexec.rs.
fn emit_compiler_fingerprint() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set");
    let manifest = Path::new(&manifest_dir);
    let vendor = manifest.join("../vendor");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"eo9-compiler-fingerprint-v1\0");
    hash_tree(&mut hasher, &vendor, &vendor);
    for config_source in ["src/wasm/mod.rs", "src/wasm/codegen.rs"] {
        let path = manifest.join(config_source);
        hasher.update(config_source.as_bytes());
        hasher.update(&std::fs::read(&path).unwrap_or_default());
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed={}", vendor.display());
    println!(
        "cargo:rustc-env=EO9_COMPILER_FINGERPRINT={}",
        hasher.finalize().to_hex()
    );
}

/// Deterministic (sorted, relative-path-labelled) content hash of a directory tree.
fn hash_tree(hasher: &mut blake3::Hasher, root: &Path, dir: &Path) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(read) => read.filter_map(Result::ok).map(|e| e.path()).collect(),
        Err(_) => return,
    };
    entries.sort();
    for path in entries {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative.to_string_lossy();
        // Skip build products and VCS metadata; hash sources only.
        if name.contains("target/") || name.ends_with(".lock") || name.contains(".git") {
            continue;
        }
        if path.is_dir() {
            hash_tree(hasher, root, &path);
        } else if let Ok(contents) = std::fs::read(&path) {
            hasher.update(name.as_bytes());
            hasher.update(&(contents.len() as u64).to_le_bytes());
            hasher.update(&contents);
        }
    }
}
