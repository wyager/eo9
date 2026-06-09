//! The incremental-parser trait: step one input, report what could come next.
//!
//! The ontology is audio2's (carried commentary, `incremental.rs`):
//!
//! 1. What set of bytes will the parser consume without rejecting? — the
//!    [`Admissible::charset`].
//! 2. Is it ready to finish? — `!hard_required`: a byte outside the charset (or `Eof`)
//!    makes a finishable parser wrap up and hand that input back (`Step::Done`'s
//!    `rejected`); an unfinishable one fails.
//!
//! This crate adds two things the eosh grammar needs that audio2's did not:
//!
//! * `Admissible::non_ascii_ok` — eosh words, quoted strings, compound literals, and
//!   comments accept arbitrary non-ASCII text. The step machinery itself is
//!   ASCII-guarded; this flag tells the editor (and [`feed_bytes`]) that a byte
//!   >= 0x80 is fine here and behaves like a generic text byte.
//!
//! * [`Step::Both`] — DESIGN DEVIATION from the study sketch (`Step::{Done,Continue}`).
//!   audio2's `Alt` returns the first `Done` and drops continuing branches, which is
//!   sound only when alternatives are mutually exclusive — true for its grammars,
//!   false for eosh's: after `browser` the command is complete (Done: Enter would run
//!   it) AND an argument list may still be coming (Continue: ` --url …`). A first-Done
//!   `Alt` would kill the argument branch and falsely redden ` --url`; preferring
//!   Continue would falsely redden Enter-able lines elsewhere. `Both` carries the fork:
//!   a value is available *and* parsing may continue. Leaf parsers never produce it;
//!   `Alt` (and `Bind` over an ambiguous left side) do.
//!
//! Invariant: `Done`/`Both`'s `rejected` is always exactly the input just stepped — a
//! parser hands back the byte it refused, never an earlier one (`Bind` relies on this
//! to feed the right-hand side).
//!
//! Portions derived from wyager/audio2 code/repl (relicensed by the author for this
//! repository, 2026-06-08): the trait shape, `Admissible`, and the forced-prefix walk.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::charset::Charset;
use crate::input::Input;

/// What a parser state will do with the next input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admissible {
    /// Which bytes the parser will consume (continue or finish-consuming).
    pub charset: Charset,
    /// If the next input is not in `charset` (including `Eof`), will the parser fail?
    /// audio2's example: a number parser that has seen no digits hard-requires one (a
    /// space fails it); with at least one digit it would finish and hand the space back.
    ///
    /// Refinement over the source (the boundary-gated finishers, [`crate::comb::Kw`]
    /// and [`crate::comb::Words`], need it): `hard_required == false` guarantees that
    /// `Eof` wraps the parser up, and that bytes outside the charset never *consume* —
    /// but such a byte may either finish-and-reject (a free word at a space) or fail
    /// (a completed keyword at a word byte: `letx` is not `let`). `hard_required ==
    /// true` still means every non-charset input fails. The test-side checker pins
    /// exactly this contract.
    pub hard_required: bool,
    /// Are bytes >= 0x80 acceptable here, behaving as generic text bytes? True inside
    /// eosh words, quoted strings, compound literals, and comments. See [`feed_bytes`]
    /// for the substitution policy.
    pub non_ascii_ok: bool,
}

impl Admissible {
    /// Nothing consumable, ready to finish.
    pub const TIME_TO_FINISH: Self = Admissible {
        charset: Charset::empty(),
        hard_required: false,
        non_ascii_ok: false,
    };

    pub fn new(charset: Charset, hard_required: bool, non_ascii_ok: bool) -> Self {
        Self {
            charset,
            hard_required,
            non_ascii_ok,
        }
    }

    /// The alternation of two admissibilities: either side's bytes work; finishing is
    /// possible if either side can finish; non-ASCII is fine if it is fine for either.
    pub fn either(&self, other: &Admissible) -> Admissible {
        Admissible {
            charset: self.charset.union(&other.charset),
            hard_required: self.hard_required && other.hard_required,
            non_ascii_ok: self.non_ascii_ok || other.non_ascii_ok,
        }
    }
}

/// A boxed incremental parser producing `T`.
pub type BoxP<T> = Box<dyn IncParse<T>>;

/// The result of stepping one input.
pub enum Step<T> {
    /// The parser finished. `rejected` is the input it refused (always the input just
    /// stepped; `Eof` when it wrapped up at end of line).
    Done { value: T, rejected: Input },
    /// The parser consumed the input and wants more.
    Continue(BoxP<T>),
    /// Ambiguity fork (see module docs): one alternative finished refusing the input,
    /// another consumed it and continues.
    Both {
        value: T,
        rejected: Input,
        cont: BoxP<T>,
    },
}

impl<T> Step<T> {
    /// The finished value, if this step finished (`Done` or `Both`).
    pub fn value(self) -> Option<T> {
        match self {
            Step::Done { value, .. } | Step::Both { value, .. } => Some(value),
            Step::Continue(_) => None,
        }
    }

    /// The continuing parser, if parsing can continue (`Continue` or `Both`).
    pub fn cont(self) -> Option<BoxP<T>> {
        match self {
            Step::Continue(p) | Step::Both { cont: p, .. } => Some(p),
            Step::Done { .. } => None,
        }
    }

    #[cfg(test)]
    pub fn assert_done(self) -> (T, Input) {
        match self {
            Step::Done { value, rejected }
            | Step::Both {
                value, rejected, ..
            } => (value, rejected),
            Step::Continue(_) => panic!("Got continue, expected done"),
        }
    }

    #[cfg(test)]
    pub fn assert_continue(self) -> BoxP<T> {
        match self {
            Step::Done { .. } => panic!("Got done, expected continue"),
            Step::Continue(p) | Step::Both { cont: p, .. } => p,
        }
    }
}

/// What kind of word a completion is — the editor's menu annotation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Tag {
    /// A shell builtin (`help`, `describe`, `svc list`, …).
    Builtin,
    /// A grammar keyword (`only`, `with`, `as`, …).
    Keyword,
    /// A program from the store's /bin listing.
    Program,
    /// A session `let` binding.
    Binding,
    /// Anything else a vocabulary provider wants to offer.
    Other,
}

/// One completion candidate: the full word, how many of its bytes are already typed,
/// and what kind of thing it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub word: String,
    pub matched: usize,
    pub tag: Tag,
}

/// An incremental parser state. States are immutable: `step` returns the successor
/// (or `None`: the input is not viable from this state — the editor's red).
pub trait IncParse<T> {
    /// Feed one input. `None` means no parse continues through this input.
    fn step(&self, input: Input) -> Option<Step<T>>;

    /// What the next input could be. See [`Admissible`]; the exhaustive agreement
    /// check between `step` and `admissible` lives in the test-side checker.
    fn admissible(&self) -> Admissible;

    /// Append the word completions available at this state (vocabulary-bearing parsers
    /// implement this; structural parsers contribute nothing).
    fn completions(&self, out: &mut Vec<Completion>) {
        let _ = out;
    }

    /// Clone behind the box ([`BoxP`] implements `Clone` through this).
    fn clone_box(&self) -> BoxP<T>;
}

impl<T: 'static> Clone for BoxP<T> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// The forced-prefix TAB walk (carried from audio2's `Tree::tab`, ~the same loop run
/// directly on parser states instead of a prebuilt tree): while exactly one byte is
/// admissible AND the parser cannot finish instead, that byte is forced — collect it
/// and step. Deviation from the source: audio2 forced single-byte sets even when the
/// parser could finish (`hard_required` unchecked); here a finishable state stops the
/// walk, because Enter (or a boundary byte) is also viable there and forcing would put
/// words in the user's mouth.
pub fn forced_prefix<T: 'static>(parser: &dyn IncParse<T>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut current = parser.clone_box();
    // Defensive cap: the admissibility checker pins step/admissible agreement, but a
    // walk should never be able to loop the editor regardless.
    for _ in 0..4096 {
        let adm = current.admissible();
        if !adm.hard_required {
            break;
        }
        let Some(byte) = adm.charset.one() else {
            break;
        };
        let input = Input::byte(byte).expect("charset bytes are ASCII");
        match current.step(input) {
            Some(Step::Continue(next)) | Some(Step::Both { cont: next, .. }) => {
                out.push(byte);
                current = next;
            }
            // A hard-required state that finishes or fails on its one admissible byte
            // contradicts its own admissibility; stop rather than force further.
            _ => break,
        }
    }
    out
}

/// Feed a whole byte string (the line so far) through a parser, applying the
/// non-ASCII policy: a byte >= 0x80 requires `non_ascii_ok` at the current state and
/// is then stepped as the representative text byte `b'x'` (any plain word byte — at
/// every `non_ascii_ok` position the grammar treats text bytes uniformly, so the
/// substitution preserves acceptance; it is an over-approximation only for branches
/// that wanted a literal `x`, which can only widen, never shrink, the accepted set —
/// the safe direction under the superset rule).
///
/// Returns the state after the last byte; `None` when some byte was not viable
/// (including a parse that finished early and refused a byte no other branch took).
pub fn feed_bytes<T: 'static>(parser: BoxP<T>, bytes: &[u8]) -> Option<BoxP<T>> {
    let mut current = parser;
    for &byte in bytes {
        let input = match Input::byte(byte) {
            Some(input) => input,
            None => {
                if !current.admissible().non_ascii_ok {
                    return None;
                }
                Input::byte(b'x').expect("ascii")
            }
        };
        current = current.step(input)?.cont()?;
    }
    Some(current)
}

/// Step `Eof` and return the finished value, if the parser can wrap up here.
pub fn finish<T: 'static>(parser: &dyn IncParse<T>) -> Option<T> {
    parser.step(Input::Eof)?.value()
}

/// Does the parser accept this whole line (all bytes, then `Eof`)?
pub fn accepts<T: 'static>(parser: BoxP<T>, line: &str) -> bool {
    match feed_bytes(parser, line.as_bytes()) {
        Some(state) => finish(&*state).is_some(),
        None => false,
    }
}
