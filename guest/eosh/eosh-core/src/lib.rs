//! eosh-core — the Eo9 shell's language and planning logic, as a library.
//!
//! The shell itself (`guest/eosh/eosh`) is an ordinary Eo9 program: a wasm component
//! importing `eo9:exec` (component algebra, compile, task), `eo9:text`, and `eo9:fs`,
//! with no private powers (see SPEC.md, "Shell" and "Execution APIs"). Everything that
//! does not need those imports to exist — the grammar, the evaluator, argument handling,
//! outcome rendering, the session state — lives here, behind the [`Backend`] trait, so
//! it can be unit-tested on the host and so that swapping in the store-backed name
//! resolution of area 11 is a change to one `Backend` implementation, not to the shell.
//!
//! The grammar (SPEC.md, "Programs as values", "Composition and the `$` operator",
//! "Environments and the `&` operator", "The capability algebra", "Capability slots,
//! `rename`, and `with`"):
//!
//! * argument application binds tightest — `--flag value` pairs and positional
//!   arguments attach to the program on their left, type-directed by the callee's
//!   declared argument types;
//! * `&` (environment extension) binds next, associating to the left;
//! * `$` (composition) binds loosest and associates to the right;
//! * `only <allow-list>`, `rename <from> <to>`, and `with <provider> as <slot>, …`
//!   (including the positional tuple form) are gate terms: keyword-first prefixes of a
//!   `$` operand that apply to everything on their right;
//! * `let <name> = <expr>` binds a component or environment value for the session;
//! * `()` is the only grouping construct; names are dotted (`virtualfs.create`).
//!
//! The top-level rule is the spec's: compose the shell's granted environment onto the
//! command, `compile`, `spawn` with the WAVE-encoded arguments, await the outcome, and
//! print it ([`Session`]).

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod ast;
pub mod backend;
pub mod envinfo;
pub mod eval;
pub mod lex;
pub mod parse;
pub mod render;
pub mod session;
pub mod wave;

#[cfg(test)]
mod testutil;

pub use ast::{Arg, ArgValue, Command, Expr};
pub use backend::{
    ArgSpec, Backend, BackendError, ComponentInfo, ComponentKind, ExportSlot, ImportNeed,
    InterfaceRef, NamedArg, Outcome, WaveValue,
};
pub use envinfo::SESSION_MANIFEST_PATH;
pub use eval::{EvalError, EvalOutput, Evaluator};
pub use parse::{ParseError, parse_command, parse_expr};
pub use render::render_outcome;
pub use session::{LineResult, Session};

use alloc::format;
use alloc::string::String;

/// The interim name-resolution convention (until the store-backed resolution of area 11
/// lands): a program name resolves to a wasm component file on the shell's granted
/// filesystem at `/bin/<name>.wasm`, with the dotted shell name used verbatim — e.g.
/// `virtualfs.create` → `/bin/virtualfs.create.wasm`. The file is opened *for
/// execution* (`open-exec`), read through the immutable handle, and handed to the
/// component algebra's `load`.
pub fn module_path(name: &str) -> String {
    format!("/bin/{name}.wasm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_uses_the_dotted_name_verbatim() {
        assert_eq!(module_path("browser"), "/bin/browser.wasm");
        assert_eq!(
            module_path("virtualfs.create"),
            "/bin/virtualfs.create.wasm"
        );
        assert_eq!(module_path("time.frozen"), "/bin/time.frozen.wasm");
    }
}
