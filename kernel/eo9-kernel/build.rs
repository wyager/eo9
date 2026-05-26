//! Build script for the bare-metal kernel.
//!
//! For bare-metal targets (`target_os = "none"`) this injects the linker script that lays
//! the image out for QEMU's aarch64 `virt` machine. When the `wasm-seed` feature is
//! enabled it additionally checks that the host-precompiled seed component was supplied
//! (via the `EO9_SEED_CWASM` environment variable set by `cargo xtask build-kernel`), so a
//! bad invocation fails here with a clear message instead of deep inside `include_bytes!`.

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
    let linker_script = Path::new(&manifest_dir).join("linker.ld");
    println!("cargo:rerun-if-changed={}", linker_script.display());
    println!("cargo:rustc-link-arg-bins=-T{}", linker_script.display());

    println!("cargo:rerun-if-env-changed=EO9_SEED_CWASM");
    if env::var("CARGO_FEATURE_WASM_SEED").is_ok() {
        match env::var("EO9_SEED_CWASM") {
            Ok(path) => println!("cargo:rerun-if-changed={path}"),
            Err(_) => panic!(
                "the `wasm-seed` feature needs the EO9_SEED_CWASM environment variable to point \
                 at the host-precompiled seed component; build the kernel via \
                 `cargo xtask build-kernel aarch64`, which precompiles the seed and sets it"
            ),
        }
    }
}
