//! The [`bindings!`](crate::bindings!), [`main!`](crate::main!), and
//! [`manual!`](crate::manual!) macros: everything a program crate needs to target its
//! WIT world and describe itself.
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
///   `time`, `entropy`, `perf`, `disk`, `fs`, `gfx`, `pci`, `platform`, `usb`, and the net layers `net_l2`,
///   `net_l3`, `net_l4`). Listing an API maps its interfaces onto [`crate::api`] instead
///   of regenerating them; `io` must be listed exactly when the world's imports use
///   `eo9:io/buffers` (i.e. for `disk`, `fs`, and the net layers).
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
    (apis [gfx $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:gfx/types@0.1.0": eo9_guest::api::gfx::types,
                "eo9:gfx/gfx@0.1.0": eo9_guest::api::gfx::gfx,
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
    (apis [net_l2 $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:net/l2@0.1.0": eo9_guest::api::net::l2,
            ]
            $($tail)*
        );
    };
    (apis [net_l3 $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:net/l3@0.1.0": eo9_guest::api::net::l3,
            ]
            $($tail)*
        );
    };
    (apis [net_l4 $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:net/l4@0.1.0": eo9_guest::api::net::l4,
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
    (apis [platform $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:platform/types@0.1.0": eo9_guest::api::platform::types,
                "eo9:platform/platform@0.1.0": eo9_guest::api::platform::platform,
            ]
            $($tail)*
        );
    };
    (apis [usb $($rest:ident)*] with [$($acc:tt)*] $($tail:tt)*) => {
        $crate::__bindings_with!(
            apis [$($rest)*]
            with [$($acc)*
                "eo9:usb/types@0.1.0": eo9_guest::api::usb::types,
                "eo9:usb/usb@0.1.0": eo9_guest::api::usb::usb,
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
/// Embed a user-facing manual in the component as the `eo9-manual` custom section
/// (docs/design/component-manuals.md). `man <name>` in eosh renders it; the incremental
/// REPL's argument grammars consume the per-arg hints additively.
///
/// Authors write structured fields; the macro `concat!`s the canonical line-oriented
/// schema text (version header, `name:`/`synopsis:`/`description:`, `arg` blocks,
/// `example:` blocks, `see-also:`, `end`) and emits it as a `#[used]`
/// `#[link_section = "eo9-manual"]` static, which the wasm linker turns into a custom
/// section of the core module. `wasm-tools component new` preserves it; the eosh-side
/// reader scans the outer component and its depth-1 core modules.
///
/// ```ignore
/// eo9_guest::manual! {
///     name: "telnetd",
///     synopsis: "serve eosh sessions over telnet, one fused task per session",
///     description: [
///         "Composes net.virtio $ net.l4.over-l2 $ net.text $ eosh, compiles it once,",
///         "and serves sessions sequentially.",
///     ],
///     args: [
///         { name: "port", ty: "u16", optional, doc: "TCP port to listen on (default 23)" },
///         { name: "nic", ty: "string", optional, doc: "the NIC provider", kind: "component-name" },
///         { name: "address", ty: "string", optional, doc: "IPv4 acquisition mode", values: "dhcp" },
///     ],
///     examples: [
///         { line: "telnetd --port 2323", doc: "serve on a non-privileged port" },
///     ],
///     see_also: "net.l4.over-l2, net.text, eosh",
/// }
/// ```
///
/// Rules the schema enforces (validated at componentize time by `cargo xtask
/// build-guest`; a malformed manual fails the build): every line at most 120 bytes, at
/// most 64 args and 16 examples, at most one of `values:`/`kind:` per arg, the whole
/// section at most 16 KiB. The manual is display-only and self-reported: the WIT
/// argument signature stays the mechanical truth, and the renderer flags any
/// type/optionality disagreement instead of trusting either side. Invoke at most once
/// per crate (the section static's name is fixed; a second invocation fails to
/// compile, which doubles as the one-manual-per-module guarantee).
#[macro_export]
macro_rules! manual {
    (
        name: $name:literal,
        synopsis: $synopsis:literal,
        description: [$($desc:literal),+ $(,)?],
        args: [$($arg:tt),* $(,)?],
        examples: [$($example:tt),* $(,)?],
        see_also: $see:literal $(,)?
    ) => {
        const __EO9_MANUAL_TEXT: &str = concat!(
            "eo9-manual 1\n",
            "name: ", $name, "\n",
            "synopsis: ", $synopsis, "\n",
            "description:\n",
            $("  ", $desc, "\n",)+
            $($crate::__manual_arg!($arg),)*
            $($crate::__manual_example!($example),)*
            "see-also: ", $see, "\n",
            "end\n",
        );
        #[used]
        #[unsafe(link_section = "eo9-manual")]
        static __EO9_MANUAL: [u8; __EO9_MANUAL_TEXT.len()] = {
            // `concat!` yields a `&str`; the section static must be a byte array so the
            // wasm linker emits the bytes verbatim. A const copy loop keeps this free of
            // unsafe and of any runtime cost.
            let text = __EO9_MANUAL_TEXT.as_bytes();
            let mut bytes = [0u8; __EO9_MANUAL_TEXT.len()];
            let mut index = 0;
            while index < bytes.len() {
                bytes[index] = text[index];
                index += 1;
            }
            bytes
        };
    };
}

/// Internal helper for [`manual!`]: one `arg` block. `required`/`optional` mirrors the
/// WIT signature (an `option<…>` parameter is `optional`, its `ty` the inner type); at
/// most one of `values:` (literal alternatives, comma-separated) or `kind:` (a value
/// vocabulary tag: url, path, component-name, interface-name, port) may follow `doc:`.
#[doc(hidden)]
#[macro_export]
macro_rules! __manual_arg {
    ({ name: $n:literal, ty: $t:literal, required, doc: $d:literal $(,)? }) => {
        concat!("arg ", $n, " ", $t, " required\n  doc: ", $d, "\n")
    };
    ({ name: $n:literal, ty: $t:literal, optional, doc: $d:literal $(,)? }) => {
        concat!("arg ", $n, " ", $t, " optional\n  doc: ", $d, "\n")
    };
    ({ name: $n:literal, ty: $t:literal, required, doc: $d:literal, values: $v:literal $(,)? }) => {
        concat!(
            "arg ",
            $n,
            " ",
            $t,
            " required\n  doc: ",
            $d,
            "\n  values: ",
            $v,
            "\n"
        )
    };
    ({ name: $n:literal, ty: $t:literal, optional, doc: $d:literal, values: $v:literal $(,)? }) => {
        concat!(
            "arg ",
            $n,
            " ",
            $t,
            " optional\n  doc: ",
            $d,
            "\n  values: ",
            $v,
            "\n"
        )
    };
    ({ name: $n:literal, ty: $t:literal, required, doc: $d:literal, kind: $k:literal $(,)? }) => {
        concat!(
            "arg ",
            $n,
            " ",
            $t,
            " required\n  doc: ",
            $d,
            "\n  kind: ",
            $k,
            "\n"
        )
    };
    ({ name: $n:literal, ty: $t:literal, optional, doc: $d:literal, kind: $k:literal $(,)? }) => {
        concat!(
            "arg ",
            $n,
            " ",
            $t,
            " optional\n  doc: ",
            $d,
            "\n  kind: ",
            $k,
            "\n"
        )
    };
}

/// Internal helper for [`manual!`]: one `example` block (the `doc:` line is optional).
#[doc(hidden)]
#[macro_export]
macro_rules! __manual_example {
    ({ line: $l:literal, doc: $d:literal $(,)? }) => {
        concat!("example: ", $l, "\n  doc: ", $d, "\n")
    };
    ({ line: $l:literal $(,)? }) => {
        concat!("example: ", $l, "\n")
    };
}

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
