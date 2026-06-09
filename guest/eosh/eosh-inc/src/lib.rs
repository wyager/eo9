//! eosh-inc — the incremental parser core for eosh's per-keystroke editor.
//!
//! A line editor that understands the shell grammar needs to answer, after every
//! keystroke: is this prefix still viable (mark dead input), what characters could come
//! next (TAB's forced-prefix walk), and what words complete here (TAB's menu). This
//! crate provides that as a small parser-combinator library — parsers that consume one
//! byte at a time and report their admissible-next-byte set — plus a v1 grammar module
//! that mirrors eosh-core's lexer and parser.
//!
//! THE one invariant (docs/study/incremental-repl-for-eosh.md, "The soundness rule"):
//! the incremental grammar's language is a SUPERSET of what `eosh_core::parse_command`
//! accepts. The editor may show a false green (input that later fails the real parser),
//! never a false red (input the real parser would take). Execution always goes through
//! the battle-tested eosh-core lex/parse path; this crate never replaces it. The
//! property is enforced by the differential host test in [`grammar`], which runs the
//! eosh-core parser corpus plus fuzzed lines through both parsers.
//!
//! Theory and the small invariant-bearing pieces (the u128 [`charset::Charset`], the
//! step/admissible ontology with `hard_required`, the forced-prefix TAB walk, the
//! exhaustive admissibility check) are carried from wyager/audio2 `code/repl`
//! (relicensed by the author for this repository, 2026-06-08); the combinator layer is
//! reimplemented on `alloc` (`Box<dyn …>` + `Rc` closures) instead of audio2's
//! monomorphized no-alloc sums.
//!
//! This is milestone M1: the crate lands and soaks alone. No eosh behavior changes;
//! wiring the editor in (read-key WIT, red marking, TAB) is M2.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod charset;
pub mod comb;
pub mod grammar;
pub mod inc;
pub mod input;

#[cfg(test)]
mod check;
