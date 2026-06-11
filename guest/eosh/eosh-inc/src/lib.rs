//! eosh-inc — the incremental parser core for eosh's per-keystroke editor.
//!
//! A line editor that understands the shell grammar needs to answer, after every
//! keystroke: is this prefix still viable (mark dead input), what characters could come
//! next (TAB's forced-prefix walk), and what words complete here (TAB's menu). This
//! crate provides that as a small parser-combinator library — parsers that consume one
//! byte at a time and report their admissible-next-byte set — plus a v1 grammar module
//! that mirrors eosh-core's lexer and parser.
//!
//! Since the one-parser unification there is no separate "real parser" to mirror:
//! the incremental grammar (now `eosh_core::grammar`) IS the shell's parser — the
//! same states this editor steps for marking and completion construct the executed
//! `Command`, and Enter hands the accumulated parse to the session (no second parse
//! of a submitted line). The old soundness rule ("a superset of `parse_command`,
//! false green allowed, false red never") became an identity at the unification; the
//! exact differential that proved it lives on as the corpus pins in the grammar
//! module.
//!
//! Theory and the small invariant-bearing pieces (the u128 [`charset::Charset`], the
//! step/admissible ontology with `hard_required`, the forced-prefix TAB walk, the
//! exhaustive admissibility check) are carried from wyager/audio2 `code/repl`
//! (relicensed by the author for this repository, 2026-06-08); the combinator layer is
//! reimplemented on `alloc` (`Box<dyn …>` + `Rc` closures) instead of audio2's
//! monomorphized no-alloc sums.
//!
//! Milestone M1 landed the parser core alone; M2 adds [`editor`] — the per-keystroke
//! line editor built on it (fed by the `eo9:text` `read-key` operation, emitting the
//! echo/marker byte stream) — which the eosh component drives when its transport
//! supports per-key input, falling back to the classic read-line loop when not.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod editor;

// The parser core and the grammar moved into eosh-core (the single-parser
// unification): eosh-core owns the one grammar that acceptance, completion, marking,
// and execution all read; this crate keeps the per-keystroke editor built on it.
// Re-exported here so embedders keep their import paths.
pub use eosh_core::{charset, check, comb, grammar, inc, input};
