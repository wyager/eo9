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
