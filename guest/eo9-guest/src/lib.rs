//! Guest SDK for writing Eo9 programs in Rust.
//!
//! An Eo9 program is a WASM component whose imports are exactly the `eo9:*` OS APIs it
//! needs and whose `main` export reports its outcome as a typed
//! `result<program-success, program-failure>` (see SPEC.md, "WASM runtime"). This crate
//! gives guest code everything it needs to be written comfortably:
//!
//! * [`api`] — the `wit-bindgen`-generated bindings for the `eo9:*` WIT packages,
//!   re-exported one module per API, shared by every guest crate;
//! * the `no_std + alloc` guest runtime (allocator + panic handler, see `rt`), so
//!   components never grow hidden WASI or libc imports;
//! * thin wrappers over each API's `default()` accessor and common operations
//!   ([`text`], [`time`], [`entropy`], [`buffer`]);
//! * the [`bindings!`] and [`main!`] macros, which map a program crate onto its WIT
//!   world: `bindings!` generates the world bindings (reusing the shared API modules),
//!   `main!` implements the world's `main` export from a plain Rust function;
//! * [`provider`] — support for crates that *export* eo9 APIs (the standard stub
//!   providers under `guest/stubs/*`): shared provider state and ready-made futures.
//!
//! Program crates build as `cdylib`s for `wasm32-unknown-unknown` and are componentized
//! by `cargo xtask build-guest`; see `guest/examples/*` for complete programs.

#![no_std]

extern crate alloc;

mod rt;

#[doc(hidden)]
mod bindings {
    // Parses this crate's `wit/` directory: the `sdk` world plus, under `wit/deps/`,
    // symlinks to the repo-level `wit/<api>` packages (the interface source of truth).
    wit_bindgen::generate!({
        world: "sdk",
        generate_all,
    });
}

pub mod api;
pub mod buffer;
pub mod entropy;
pub mod provider;
pub mod text;
pub mod time;

mod macros;

/// Run a future to completion on the calling task, blocking until it resolves.
///
/// The blocking `eo9:*` operations are `async func`s, generated as async Rust functions;
/// a program whose world declares `main: async func` (the spec's convention) simply
/// awaits them — see the `async fn main` form of [`main!`]. `block_on` remains for
/// driving an auxiliary future from a context that is already allowed to block; a
/// sync-lifted export cannot block, so it is not a way around an async `main`. Waiting
/// uses the Component Model's own waitable-set machinery, so the host can schedule other
/// tasks while this one is parked.
pub use wit_bindgen::block_on;
