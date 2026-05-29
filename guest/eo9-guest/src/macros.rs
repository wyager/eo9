//! The [`bindings!`](crate::bindings!) and [`main!`](crate::main!) macros: everything a
//! program crate needs to target its WIT world.
//!
//! A program crate invokes both at its crate root:
//!
//! ```ignore
//! // Generate bindings for this crate's world, reusing the shared eo9 API modules.
//! eo9_guest::bindings!({
//!     world: "hello",
//!     apis: [text, time],
//! });
//!
//! // Implement the world's `main` export with its typed success/failure variants.
//! eo9_guest::main! {
//!     fn main(name: String) -> Result<ProgramSuccess, ProgramFailure> {
//!         // ...
//!     }
//! }
//! ```
//!
//! The crate's world lives in its own `wit/` directory, with the repo-level `wit/<api>`
//! packages it imports symlinked under `wit/deps/` (the same convention the repo-level
//! packages use for their own dependencies). The crate must list both `eo9-guest` and
//! `wit-bindgen` as dependencies, under those names: the expansion refers to
//! `eo9_guest::api` for the remapped interface modules and the generated code refers to
//! `wit_bindgen::rt` for its runtime support.

/// Generate bindings for a program crate's WIT world, mapping the standard `eo9:*` API
/// interfaces onto the shared modules in [`crate::api`].
///
/// * `world` — the world name, defined in the crate's own `wit/` directory (with the
///   repo-level packages it imports symlinked under `wit/deps/`).
/// * `apis` — which eo9 APIs the world imports, as bare identifiers (`io`, `text`,
///   `time`, `entropy`, `perf`, `disk`, `fs`, `net`, `pci`). Listing an API maps its
///   interfaces onto [`crate::api`] instead of regenerating them; `io` must be listed
///   exactly when the world's imports use `eo9:io/buffers` (i.e. for `disk`, `fs`, and
///   `net`).
///
/// The API list must match the world's imports exactly — a missing entry fails with
/// wit-bindgen's "no remapping found" error, an extra one with its "unused remappings"
/// error — so the import list stays auditable in one place.
///
/// Must be invoked at the crate root: the world's own types (argument records, the
/// success/failure variants, the `Guest` trait, the `export!` macro) are generated
/// there, which is where [`crate::main!`] expects them.
#[macro_export]
macro_rules! bindings {
    ({
        world: $world:literal,
        apis: [$($api:ident),* $(,)?] $(,)?
    }) => {
        $crate::__bindings_with!(
            apis [$($api)*]
            with []
            world $world
        );
    };
}

/// Internal helper for [`bindings!`]: turns the `apis` list into `with` remappings by
/// push-down accumulation, then emits the final `wit_bindgen::generate!` invocation.
#[doc(hidden)]
#[macro_export]
macro_rules! __bindings_with {
    (apis [io $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:io/buffers@0.1.0": eo9_guest::api::io::buffers,
            ]
            $($tail)*
        );
    };
    (apis [text $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:text/types@0.1.0": eo9_guest::api::text::types,
                "eo9:text/text@0.1.0": eo9_guest::api::text::text,
            ]
            $($tail)*
        );
    };
    (apis [time $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:time/types@0.1.0": eo9_guest::api::time::types,
                "eo9:time/time@0.1.0": eo9_guest::api::time::time,
            ]
            $($tail)*
        );
    };
    (apis [entropy $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:entropy/types@0.1.0": eo9_guest::api::entropy::types,
                "eo9:entropy/entropy@0.1.0": eo9_guest::api::entropy::entropy,
            ]
            $($tail)*
        );
    };
    (apis [perf $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:perf/types@0.1.0": eo9_guest::api::perf::types,
                "eo9:perf/perf@0.1.0": eo9_guest::api::perf::perf,
            ]
            $($tail)*
        );
    };
    (apis [disk $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:disk/types@0.1.0": eo9_guest::api::disk::types,
                "eo9:disk/disk@0.1.0": eo9_guest::api::disk::disk,
            ]
            $($tail)*
        );
    };
    (apis [fs $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:fs/fs@0.1.0": eo9_guest::api::fs::fs,
            ]
            $($tail)*
        );
    };
    (apis [net $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:net/types@0.1.0": eo9_guest::api::net::types,
                "eo9:net/net@0.1.0": eo9_guest::api::net::net,
            ]
            $($tail)*
        );
    };
    (apis [pci $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:pci/types@0.1.0": eo9_guest::api::pci::types,
                "eo9:pci/pci@0.1.0": eo9_guest::api::pci::pci,
            ]
            $($tail)*
        );
    };
    // All APIs processed, nothing remapped: a pure-compute world with no eo9 imports.
    (apis [] with [] world $world:literal) => {
        ::wit_bindgen::generate!({
            world: $world,
        });
    };
    // All APIs processed: emit the real generate! invocation with the remappings.
    (apis [] with [$($with:tt)+] world $world:literal) => {
        ::wit_bindgen::generate!({
            world: $world,
            with: { $($with)+ },
        });
    };
}

/// Implement a world's `main` export from a plain Rust function.
///
/// The function's signature must match the world's `main` export exactly: one Rust
/// parameter per named, typed WIT argument, returning
/// `Result<ProgramSuccess, ProgramFailure>` — the world's own success/failure variants
/// as generated by [`crate::bindings!`]. Must be invoked at the crate root, after
/// `bindings!`.
///
/// Worlds whose entrypoint is `export main: async func(...)` — the spec's convention,
/// and required for a program that awaits any `eo9:*` operation (a sync-lifted export
/// cannot block) — use the `async fn main` form; worlds with a plain `func` entrypoint
/// (pure compute, sync-only imports) use the `fn main` form.
#[macro_export]
macro_rules! main {
    (
        $(#[$attr:meta])*
        async fn main($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty
        $body:block
    ) => {
        struct Eo9MainExport;

        impl Guest for Eo9MainExport {
            $(#[$attr])*
            async fn main($($arg: $ty),*) -> $ret $body
        }

        export!(Eo9MainExport);
    };
    (
        $(#[$attr:meta])*
        fn main($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty
        $body:block
    ) => {
        struct Eo9MainExport;

        impl Guest for Eo9MainExport {
            $(#[$attr])*
            fn main($($arg: $ty),*) -> $ret $body
        }

        export!(Eo9MainExport);
    };
}
