//! Rendering a guest trap into a readable `Outcome::Trapped` reason.
//!
//! A guest trap (a Rust `panic!` lowers to the wasm `unreachable` instruction — see the
//! guest SDK's runtime profile) surfaces to the host as a `wasmtime::Error`. Its default
//! `{err:#}` rendering is a multi-line, address-laden backtrace with rustc name
//! disambiguators (`core[73b5…]::panicking::panic_fmt`), which the user studies flagged as
//! unreadable raw text. This module turns that error into a concise, deterministic reason:
//! the trap kind plus a symbol-only call chain, with the code addresses and `[hash]` noise
//! removed.
//!
//! The guest's panic *message* and source location arrive separately: the SDK's panic
//! handler reports them through `eo9:rt/diagnostics.report-panic` just before trapping
//! (a trapped instance cannot be re-entered, so a post-trap export cannot work — see
//! plan/07 Decision 12), the executor parks them in the task's write-once slot, and this
//! module folds them into the front of the reason when the trap is rendered.

use wasmtime::Trap;

/// The most frames we name; deep recursion shouldn't produce an unbounded reason string.
const MAX_FRAMES: usize = 16;

/// Build a readable, deterministic `Outcome::Trapped` reason from a guest-call error.
///
/// Deterministic by construction: only the trap kind and the backtrace's (demangled)
/// symbol names appear — never code addresses or per-build hashes — so the same trap
/// yields the same reason across runs and builds. Symbol names are taken from wasmtime's
/// own demangled backtrace text (`FrameInfo::func_name` returns the still-mangled symbol),
/// so no demangler dependency is needed.
pub(crate) fn trap_reason(err: &wasmtime::Error, panic_message: Option<&str>) -> String {
    // wasmtime's alternate Display is the demangled backtrace; we mine symbol names from it.
    let full = format!("{err:#}");

    // Only rewrite genuine wasm traps (panics/unreachable, out-of-bounds, etc.). Any other
    // error reaching here is a clean in-band/host error that already names itself (e.g. the
    // io-buffer cap messages) — pass it through unchanged.
    let Some(trap) = err.downcast_ref::<Trap>() else {
        return full;
    };

    // A Rust guest panic is an unreachable trap with panic frames on the stack: label it a
    // panic while keeping the trap's own wording (e.g. "wasm `unreachable` instruction
    // executed", "out of bounds memory access").
    let panicked = full.contains("rust_begin_unwind") || full.contains("panic_fmt");
    let kind = match (panicked, panic_message) {
        // The usual case: a Rust panic that reported its message before trapping.
        (true, Some(message)) => format!("guest panicked: {message} — {trap}"),
        (true, None) => format!("guest panicked — {trap}"),
        // A reported message followed by a different trap (e.g. a bounds fault while
        // unwinding-free panicking is impossible, but a hostile guest could do this):
        // still surface what it said, clearly labelled.
        (false, Some(message)) => format!("{trap} (guest reported: {message})"),
        (false, None) => trap.to_string(),
    };

    let mut chain: Vec<String> = Vec::new();
    let mut truncated = false;
    for line in full.lines() {
        // Backtrace frame lines look like:
        //   "   2:    0x8de - module.wasm!core[hash]::panicking::panic_fmt[: <trap msg>]"
        let Some((_, after_dash)) = line.split_once(" - ") else {
            continue;
        };
        let Some((_module, symbol_and_tail)) = after_dash.split_once('!') else {
            continue;
        };
        // The last frame carries the trailing trap message after ": "; drop it.
        let symbol = symbol_and_tail
            .split_once(": ")
            .map_or(symbol_and_tail, |(sym, _tail)| sym);
        let cleaned = clean_symbol(symbol.trim());
        if cleaned.is_empty() {
            continue;
        }
        if chain.len() == MAX_FRAMES {
            truncated = true;
            break;
        }
        chain.push(cleaned);
    }

    if chain.is_empty() {
        return kind;
    }
    if truncated {
        chain.push("…".to_string());
    }
    // Frames are innermost-first; "←" reads "called from".
    format!("{kind}; guest backtrace: {}", chain.join(" ← "))
}

/// Strip rustc's `[hex-hash]` disambiguators from a demangled symbol, leaving the readable
/// path. `core[73b5…]::panicking::panic_fmt` → `core::panicking::panic_fmt`.
fn clean_symbol(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c == '[' {
            // Drop everything through the matching ']'.
            for inner in chars.by_ref() {
                if inner == ']' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::clean_symbol;

    #[test]
    fn strips_hash_disambiguators() {
        assert_eq!(
            clean_symbol("core[73b528423be946ea]::panicking::panic_fmt"),
            "core::panicking::panic_fmt"
        );
        assert_eq!(
            clean_symbol("__rustc[b61d6fc71ed46f55]::rust_begin_unwind"),
            "__rustc::rust_begin_unwind"
        );
        assert_eq!(clean_symbol("main"), "main");
    }
}
