//! Embed an Eo9 instance and run the `hello` example component.
//!
//! Run from the repository root after building the guest components:
//!
//! ```text
//! cargo run -p xtask -- build-guest
//! cargo run -p eo9-embed --example embed_hello
//! ```
//!
//! This grants the default capability set (text + time + entropy) backed by the host, so
//! `hello`'s greeting prints on this process's stdout, and reports the program's outcome.

use std::path::PathBuf;

use eo9_embed::{Eo9, NamedArg, render_outcome};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The componentized `hello` example produced by `cargo xtask build-guest`.
    let component = repo_root().join("guest/target/components/eo9-example-hello.wasm");
    let bytes = std::fs::read(&component).map_err(|err| {
        format!(
            "cannot read {} (run `cargo run -p xtask -- build-guest` first): {err}",
            component.display()
        )
    })?;

    // A host-backed instance with the default grants (text + time + entropy).
    let eo9 = Eo9::builder().build()?;

    let args = [
        NamedArg::new("name", "\"embedder\""),
        NamedArg::new("excited", "true"),
    ];
    let outcome = eo9.run_bytes(&bytes, &args)?;

    let (rendered, code) = render_outcome(&outcome);
    println!("outcome = {rendered}");
    std::process::exit(i32::from(code));
}

fn repo_root() -> PathBuf {
    // crates/eo9-embed/examples/embed_hello.rs -> repo root is three levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root must exist")
}
