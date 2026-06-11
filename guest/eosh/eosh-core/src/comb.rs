//! The combinators and leaf parsers.
//!
//! Combinators on `alloc`: [`Pure`], [`Map`], [`Bind`] (`Rc<dyn Fn>` continuation —
//! all laziness and recursion in a grammar goes through a bind's right side or
//! [`lazy`]), n-ary [`Alt`] over `Vec<BoxP<T>>`, and the derived [`star`]/[`rep`].
//!
//! Leaf parsers, mirroring eosh-core's lexer (`eosh-core/src/lex.rs`): [`Lit`]
//! (suffix-slice byte literal, self-delimiting — structural characters), [`Kw`]
//! (a literal that must end at a word boundary — keywords), [`Words`] (the runtime
//! vocabulary primitive: a set of tagged words, matched with boundary semantics,
//! implementing `completions` directly), [`Word`] (the free bare word: maximal munch
//! over eosh's word bytes, with exact-word exclusion for reserved/dispatch words and
//! the `--`/compound-start carve-outs), [`Quoted`] (the 5 escapes), [`Compound`]
//! (the balanced `[…]`/`{…}` literal with opaque embedded strings, mirroring
//! `lex_compound`), [`Ws`], [`CommentRest`], and the carried [`Nat`].
//!
//! Word-boundary convention (the crate-wide one, from `lex.rs::ends_word`): ASCII
//! whitespace, the structural bytes `$ & ( ) , =`, `"`, and `#` end a word; `Eof`
//! counts as a boundary. Everything else ASCII — including `[ ] { }` mid-word, `-`,
//! and control bytes — is a word byte, because that is exactly what eosh's lexer does.
//!
//! Portions derived from wyager/audio2 code/repl (relicensed by the author for this
//! repository, 2026-06-08): `Lit`'s suffix-slice shape, `Nat`, `Pure`, `Map`, and
//! `Bind` (including its admissibility commentary) are close ports; `Alt` is
//! reworked for the [`crate::inc::Step::Both`] fork.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use crate::charset::Charset;
use crate::inc::{Admissible, BoxP, Completion, IncParse, Step, Tag};
use crate::input::Input;

// ---------------------------------------------------------------------------
// The word-boundary convention and its charsets
// ---------------------------------------------------------------------------

/// Does `byte` end a bare word? Mirrors `eosh-core/src/lex.rs::ends_word` plus the
/// structural set: ASCII whitespace (`char::is_whitespace` over ASCII is exactly
/// 0x09..=0x0D and space), `$ & ( ) , =`, `"`, `#`.
pub const fn is_boundary_byte(byte: u8) -> bool {
    matches!(
        byte,
        0x09..=0x0D | b' ' | b'$' | b'&' | b'(' | b')' | b',' | b'=' | b'"' | b'#'
    )
}

/// Is `byte` a bare-word byte (ASCII and not a boundary)?
pub const fn is_word_byte(byte: u8) -> bool {
    byte < 0x80 && !is_boundary_byte(byte)
}

const fn build_word_charset() -> Charset {
    let mut cs = Charset::empty();
    let mut b = 0u8;
    while b < 0x80 {
        if is_word_byte(b) {
            cs.add(b);
        }
        b += 1;
    }
    cs
}

const fn build_ws_charset() -> Charset {
    let mut cs = Charset::empty();
    let mut b = 0x09u8;
    while b <= 0x0D {
        cs.add(b);
        b += 1;
    }
    cs.add(b' ');
    cs
}

/// All bare-word bytes.
pub const WORD_CHARSET: Charset = build_word_charset();
/// ASCII whitespace.
pub const WS_CHARSET: Charset = build_ws_charset();

// ---------------------------------------------------------------------------
// Pure
// ---------------------------------------------------------------------------

/// Succeeds immediately with a value, consuming nothing.
#[derive(Clone)]
pub struct Pure<T: Clone> {
    value: T,
}

impl<T: Clone + 'static> IncParse<T> for Pure<T> {
    fn step(&self, input: Input) -> Option<Step<T>> {
        Some(Step::Done {
            value: self.value.clone(),
            rejected: input,
        })
    }

    fn admissible(&self) -> Admissible {
        Admissible::TIME_TO_FINISH
    }

    fn clone_box(&self) -> BoxP<T> {
        Box::new(self.clone())
    }
}

pub fn pure<T: Clone + 'static>(value: T) -> BoxP<T> {
    Box::new(Pure { value })
}

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

/// Applies a function to the parsed value.
pub struct Map<A, B> {
    parser: BoxP<A>,
    f: Rc<dyn Fn(A) -> B>,
}

impl<A: 'static, B: 'static> IncParse<B> for Map<A, B> {
    fn step(&self, input: Input) -> Option<Step<B>> {
        Some(match self.parser.step(input)? {
            Step::Done { value, rejected } => Step::Done {
                value: (self.f)(value),
                rejected,
            },
            Step::Continue(cont) => Step::Continue(Box::new(Map {
                parser: cont,
                f: self.f.clone(),
            })),
            Step::Both {
                value,
                rejected,
                cont,
            } => Step::Both {
                value: (self.f)(value),
                rejected,
                cont: Box::new(Map {
                    parser: cont,
                    f: self.f.clone(),
                }),
            },
        })
    }

    fn admissible(&self) -> Admissible {
        self.parser.admissible()
    }

    fn completions(&self, out: &mut Vec<Completion>) {
        self.parser.completions(out);
    }

    fn clone_box(&self) -> BoxP<B> {
        Box::new(Map {
            parser: self.parser.clone(),
            f: self.f.clone(),
        })
    }
}

pub fn map<A: 'static, B: 'static>(parser: BoxP<A>, f: impl Fn(A) -> B + 'static) -> BoxP<B> {
    Box::new(Map {
        parser,
        f: Rc::new(f),
    })
}

// ---------------------------------------------------------------------------
// Bind
// ---------------------------------------------------------------------------

/// Monadic sequencing: run `P1`; when it finishes, build the continuation parser from
/// its value and feed it the rejected input. The continuation closure is where all
/// grammar laziness and recursion live (it runs only when the left side completes),
/// and what value-dependent grammar (the `with (…) as (…)` tuple arity) hangs off.
pub enum Bind<A, B> {
    P1(BoxP<A>, Rc<dyn Fn(A) -> BoxP<B>>),
    P2(BoxP<B>),
}

impl<A: 'static, B: 'static> Bind<A, B> {
    /// Wrap a right-side step result, keeping a still-alive left side as a parallel
    /// branch (the `Both` fork from an ambiguous left side).
    fn merge_rhs(rhs: Step<B>, left_alive: Option<BoxP<B>>) -> Step<B> {
        match (rhs, left_alive) {
            (step, None) => step,
            (Step::Done { value, rejected }, Some(left)) => Step::Both {
                value,
                rejected,
                cont: left,
            },
            (Step::Continue(cont), Some(left)) => Step::Continue(alt(vec![left, cont])),
            (
                Step::Both {
                    value,
                    rejected,
                    cont,
                },
                Some(left),
            ) => Step::Both {
                value,
                rejected,
                cont: alt(vec![left, cont]),
            },
        }
    }
}

impl<A: 'static, B: 'static> IncParse<B> for Bind<A, B> {
    fn step(&self, input: Input) -> Option<Step<B>> {
        match self {
            Bind::P1(p1, f) => match p1.step(input)? {
                Step::Done { value, rejected } => {
                    // rejected is always the stepped input; the continuation sees it.
                    let p2 = f(value);
                    Some(Self::wrap_p2(p2.step(rejected)?, None))
                }
                Step::Continue(p1) => Some(Step::Continue(Box::new(Bind::P1(p1, f.clone())))),
                Step::Both {
                    value,
                    rejected,
                    cont,
                } => {
                    let left: BoxP<B> = Box::new(Bind::P1(cont, f.clone()));
                    let p2 = f(value);
                    match p2.step(rejected) {
                        // The continuation refuses the input the left side consumed:
                        // only the left branch survives.
                        None => Some(Step::Continue(left)),
                        Some(rhs) => Some(Self::wrap_p2(rhs, Some(left))),
                    }
                }
            },
            Bind::P2(p2) => Some(Self::wrap_p2(p2.step(input)?, None)),
        }
    }

    fn admissible(&self) -> Admissible {
        match self {
            Bind::P1(p1, f) => {
                let p1e = p1.admissible();
                // Carried commentary (audio2 incremental.rs, Bind::admissible) — this
                // is a documented approximation:
                //
                // We have a >>= (\x -> b).
                // If !a.admissible().hard_required, then we could wrap up `a` and move
                // on to `b`. However, the admissible set of `b` could depend on `x`,
                // which depends on with what char we wrap up `a`! We could, for
                // example, only expand the RHS of the bind if `a` has no chars it's
                // willing to accept, but is !hard_required. Or, as in the source: if
                // P1 can in principle wrap up, wrap it up with Eof, pass the result to
                // the RHS, and union the two admissible sets. This may not produce the
                // correct result! But it usually will, especially for \() -> b.
                //
                // Two refinements over the source, both pushed by this crate's grammar
                // and pinned by the test-side checker:
                //
                // * The blanket union OVER-claims when P1 finishes only on a subset of
                //   inputs — a completed keyword wraps up at a word boundary but FAILS
                //   on a word byte (`svc log` + `x` is `svc logx`, not a service name),
                //   yet the RHS's word bytes would all be claimed. So each RHS byte
                //   outside P1's own charset is probed: claimed only if P1 actually
                //   finishes on it. Over-claims are the dangerous direction (the TAB
                //   walk types a claimed byte), so they are spent on probe steps, not
                //   tolerated.
                // * What remains approximate is exactly the value dependence: the RHS
                //   probed is f(Eof-completion value), and a byte-completion could in
                //   principle produce a different value and a different RHS. The eosh
                //   grammar keeps its binds value-independent (or value-fixed by
                //   completion time, like the tuple-arity count) so the residual is
                //   zero there — the checker quantifies it over reachable states.
                if p1e.hard_required {
                    // We cannot possibly expand the RHS of the bind, so we should
                    // return only admissible stuff from the LHS of the bind.
                    p1e
                } else {
                    match p1.step(Input::Eof) {
                        Some(Step::Done { value, rejected })
                        | Some(Step::Both {
                            value, rejected, ..
                        }) => {
                            // Justifiable only because rejected is Eof (assuming p1 is
                            // well-behaved).
                            if rejected != Input::Eof {
                                panic!("Unexpected non-EOF in bind admissibility check");
                            }
                            let p2e = f(value).admissible();
                            let mut charset = p1e.charset;
                            for byte in p2e.charset.bytes() {
                                if charset.contains(byte) {
                                    continue;
                                }
                                let input = Input::byte(byte).expect("charset bytes are ASCII");
                                if matches!(
                                    p1.step(input),
                                    Some(Step::Done { .. }) | Some(Step::Both { .. })
                                ) {
                                    charset.add(byte);
                                }
                            }
                            // hard_required depends on p2: anything admissible for p1
                            // is fine; anything else goes to p2, so we fail only where
                            // p2 hard-requires.
                            Admissible {
                                charset,
                                hard_required: p2e.hard_required,
                                non_ascii_ok: p1e.non_ascii_ok || p2e.non_ascii_ok,
                            }
                        }
                        Some(Step::Continue(_)) => {
                            panic!("Parser continued after getting EOF in bind admissibility")
                        }
                        None => panic!("Parser lied about accepting EOF in bind admissibility"),
                    }
                }
            }
            Bind::P2(p2) => p2.admissible(),
        }
    }

    fn completions(&self, out: &mut Vec<Completion>) {
        match self {
            Bind::P1(p1, f) => {
                p1.completions(out);
                // When the left side could wrap up here, the right side's first word
                // is also completable (e.g. the keyword after optional whitespace) —
                // same Eof peek, same approximation, as `admissible`.
                if !p1.admissible().hard_required
                    && let Some(value) = p1.step(Input::Eof).and_then(Step::value)
                {
                    f(value).completions(out);
                }
            }
            Bind::P2(p2) => p2.completions(out),
        }
    }

    fn clone_box(&self) -> BoxP<B> {
        Box::new(match self {
            Bind::P1(p1, f) => Bind::P1(p1.clone(), f.clone()),
            Bind::P2(p2) => Bind::P2(p2.clone()),
        })
    }
}

impl<A: 'static, B: 'static> Bind<A, B> {
    fn wrap_p2(step: Step<B>, left_alive: Option<BoxP<B>>) -> Step<B> {
        let wrapped = match step {
            Step::Done { value, rejected } => Step::Done { value, rejected },
            Step::Continue(p2) => Step::Continue(Box::new(Bind::<A, B>::P2(p2))),
            Step::Both {
                value,
                rejected,
                cont,
            } => Step::Both {
                value,
                rejected,
                cont: Box::new(Bind::<A, B>::P2(cont)),
            },
        };
        Self::merge_rhs(wrapped, left_alive)
    }
}

pub fn bind<A: 'static, B: 'static>(
    parser: BoxP<A>,
    f: impl Fn(A) -> BoxP<B> + 'static,
) -> BoxP<B> {
    Box::new(Bind::P1(parser, Rc::new(f)))
}

// ---------------------------------------------------------------------------
// Alt
// ---------------------------------------------------------------------------

/// N-ary alternation, breadth-first: every branch is stepped; finished branches
/// contribute a value (first finisher wins the value slot — the grammar keeps
/// simultaneous finishers value-equal), continuing branches are kept alive together.
pub struct Alt<T> {
    branches: Vec<BoxP<T>>,
}

impl<T: 'static> IncParse<T> for Alt<T> {
    fn step(&self, input: Input) -> Option<Step<T>> {
        let mut done: Option<(T, Input)> = None;
        let mut conts: Vec<BoxP<T>> = Vec::new();
        for branch in &self.branches {
            match branch.step(input) {
                None => {}
                Some(Step::Done { value, rejected }) => {
                    if done.is_none() {
                        done = Some((value, rejected));
                    }
                }
                Some(Step::Continue(cont)) => conts.push(cont),
                Some(Step::Both {
                    value,
                    rejected,
                    cont,
                }) => {
                    if done.is_none() {
                        done = Some((value, rejected));
                    }
                    conts.push(cont);
                }
            }
        }
        let cont = match conts.len() {
            0 => None,
            1 => Some(conts.pop().expect("len checked")),
            _ => Some(Box::new(Alt { branches: conts }) as BoxP<T>),
        };
        match (done, cont) {
            (None, None) => None,
            (Some((value, rejected)), None) => Some(Step::Done { value, rejected }),
            (None, Some(cont)) => Some(Step::Continue(cont)),
            (Some((value, rejected)), Some(cont)) => Some(Step::Both {
                value,
                rejected,
                cont,
            }),
        }
    }

    fn admissible(&self) -> Admissible {
        let mut branches = self.branches.iter();
        let first = branches
            .next()
            .expect("Alt is never constructed empty")
            .admissible();
        branches.fold(first, |acc, branch| acc.either(&branch.admissible()))
    }

    fn completions(&self, out: &mut Vec<Completion>) {
        for branch in &self.branches {
            branch.completions(out);
        }
    }

    fn clone_box(&self) -> BoxP<T> {
        Box::new(Alt {
            branches: self.branches.clone(),
        })
    }
}

/// Alternation. Panics on an empty branch list (an empty alternation is the parser
/// that always fails; no grammar here wants one).
pub fn alt<T: 'static>(branches: Vec<BoxP<T>>) -> BoxP<T> {
    assert!(!branches.is_empty(), "alt of zero branches");
    if branches.len() == 1 {
        let mut branches = branches;
        return branches.pop().expect("len checked");
    }
    Box::new(Alt { branches })
}

// ---------------------------------------------------------------------------
// Lazy
// ---------------------------------------------------------------------------

/// Defers construction to first use — for recursive grammar references outside a
/// bind's right side.
pub struct Lazy<T> {
    build: Rc<dyn Fn() -> BoxP<T>>,
}

impl<T: 'static> IncParse<T> for Lazy<T> {
    fn step(&self, input: Input) -> Option<Step<T>> {
        (self.build)().step(input)
    }

    fn admissible(&self) -> Admissible {
        (self.build)().admissible()
    }

    fn completions(&self, out: &mut Vec<Completion>) {
        (self.build)().completions(out);
    }

    fn clone_box(&self) -> BoxP<T> {
        Box::new(Lazy {
            build: self.build.clone(),
        })
    }
}

pub fn lazy<T: 'static>(build: impl Fn() -> BoxP<T> + 'static) -> BoxP<T> {
    Box::new(Lazy {
        build: Rc::new(build),
    })
}

// ---------------------------------------------------------------------------
// star / rep — derived repetition
// ---------------------------------------------------------------------------

/// Zero or more items. Defined as `alt[done-now, item then star]`, so the
/// "no more items" branch stays alive in parallel with a started item — repetition
/// inherits the breadth-first fork instead of committing (see [`crate::inc::Step::Both`]).
pub fn star(item: impl Fn() -> BoxP<()> + Clone + 'static) -> BoxP<()> {
    let again = item.clone();
    alt(vec![pure(()), bind(item(), move |()| star(again.clone()))])
}

/// Exactly `n` items (used by the tuple-arity grammar).
pub fn rep(n: usize, item: impl Fn() -> BoxP<()> + Clone + 'static) -> BoxP<()> {
    if n == 0 {
        pure(())
    } else {
        let again = item.clone();
        bind(item(), move |()| rep(n - 1, again.clone()))
    }
}

// ---------------------------------------------------------------------------
// Lit — exact byte literal (structural characters)
// ---------------------------------------------------------------------------

/// An exact byte literal, stored as the remaining suffix (carried shape). Once all
/// bytes are consumed it is finished and hands back whatever comes next — structural
/// characters are self-delimiting, so no boundary check (contrast [`Kw`]).
#[derive(Clone)]
pub struct Lit {
    rest: &'static [u8],
}

impl IncParse<()> for Lit {
    fn step(&self, input: Input) -> Option<Step<()>> {
        if self.rest.is_empty() {
            return Some(Step::Done {
                value: (),
                rejected: input,
            });
        }
        match input {
            Input::Eof => None,
            Input::Byte(byte) => {
                if byte.get() == self.rest[0] {
                    Some(Step::Continue(Box::new(Lit {
                        rest: &self.rest[1..],
                    })))
                } else {
                    None
                }
            }
        }
    }

    fn admissible(&self) -> Admissible {
        match self.rest.first() {
            None => Admissible::TIME_TO_FINISH,
            Some(&byte) => Admissible::new(Charset::singleton(byte), true, false),
        }
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(self.clone())
    }
}

pub fn lit(bytes: &'static [u8]) -> BoxP<()> {
    Box::new(Lit { rest: bytes })
}

/// A single-byte literal (the structural characters `$ & ( ) , =` and `#`).
pub fn lit_byte(byte: u8) -> BoxP<()> {
    const TABLE: [u8; 128] = {
        let mut t = [0u8; 128];
        let mut i = 0;
        while i < 128 {
            t[i] = i as u8;
            i += 1;
        }
        t
    };
    let i = byte as usize;
    lit(&TABLE[i..=i])
}

// ---------------------------------------------------------------------------
// Kw — keyword: literal + word boundary
// ---------------------------------------------------------------------------

/// A keyword: the exact word, which must end at a word boundary — `let` matches
/// `let x` but not `letx` (where eosh's lexer sees one word `letx`). Completable.
#[derive(Clone)]
pub struct Kw {
    word: &'static str,
    pos: usize,
    tag: Tag,
}

impl IncParse<()> for Kw {
    fn step(&self, input: Input) -> Option<Step<()>> {
        let bytes = self.word.as_bytes();
        match input {
            Input::Eof => {
                if self.pos == bytes.len() {
                    Some(Step::Done {
                        value: (),
                        rejected: input,
                    })
                } else {
                    None
                }
            }
            Input::Byte(byte) => {
                let b = byte.get();
                if self.pos == bytes.len() {
                    if is_boundary_byte(b) {
                        Some(Step::Done {
                            value: (),
                            rejected: input,
                        })
                    } else {
                        None
                    }
                } else if bytes[self.pos] == b {
                    Some(Step::Continue(Box::new(Kw {
                        word: self.word,
                        pos: self.pos + 1,
                        tag: self.tag,
                    })))
                } else {
                    None
                }
            }
        }
    }

    fn admissible(&self) -> Admissible {
        let bytes = self.word.as_bytes();
        if self.pos == bytes.len() {
            // Complete: only a boundary (or Eof) finishes it; word bytes kill it.
            // hard_required=false with an empty charset is exactly that: nothing
            // consumable, finishing possible.
            Admissible::TIME_TO_FINISH
        } else {
            Admissible::new(Charset::singleton(bytes[self.pos]), true, false)
        }
    }

    fn completions(&self, out: &mut Vec<Completion>) {
        // A COMPLETE keyword still reports itself (matched == len, nothing left to
        // type), like [`Words`] does: the editor's name-marking oracle reads "some
        // name-tagged completion is alive" as "this word still names something", and
        // a fully typed `help` must stay green. TAB on it just appends the space.
        out.push(Completion {
            word: String::from(self.word),
            matched: self.pos,
            tag: self.tag,
            desc: None,
            glue: false,
        });
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(self.clone())
    }
}

pub fn kw(word: &'static str, tag: Tag) -> BoxP<()> {
    debug_assert!(word.bytes().all(is_word_byte));
    Box::new(Kw { word, pos: 0, tag })
}

// ---------------------------------------------------------------------------
// Words — the runtime vocabulary primitive
// ---------------------------------------------------------------------------

/// One vocabulary entry for [`Words`]: the word, its menu tag, and the optional
/// candidate-list annotations (M3 — see [`Completion`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintEntry {
    pub word: String,
    pub tag: Tag,
    pub desc: Option<String>,
    pub glue: bool,
}

impl HintEntry {
    pub fn plain(word: String, tag: Tag) -> Self {
        HintEntry {
            word,
            tag,
            desc: None,
            glue: false,
        }
    }
}

/// A set of tagged vocabulary words (builtins ∪ session bindings ∪ /bin listing,
/// snapshotted per prompt; M3 adds flag-name and value-hint sets), matched like
/// [`Kw`]: an alive entry completes only at a word boundary, mirroring the lexer's
/// maximal munch (`time` does not finish inside `time.frozen`). This is the
/// completion source for name positions.
///
/// Construction drops entries that are not lexable as one plain word — bytes outside
/// the word set, a leading `[`/`{` (the lexer reads those as compound literals), a
/// leading `--` (a flag token) — because matching them here would diverge from the
/// real lexer. The filter is also the M3 soundness backstop: every surviving entry's
/// language is a subset of the free bare word's, so adding a `Words` branch to an
/// alternation never changes what is admissible — only what completes.
pub struct Words {
    vocab: Rc<Vec<HintEntry>>,
    alive: Vec<u32>,
    pos: usize,
}

impl Words {
    pub fn entry_is_word(entry: &str) -> bool {
        // Printable ASCII only (beyond the word-byte rule): control bytes are word
        // bytes to the lexer (it never sees them — the key decoders map them to Ctrl
        // keys), but an entry containing one would carry it into `completions()` and
        // from there raw onto the terminal in a TAB menu. Construction-side
        // sanitization (eosh-core's manual merge) already strips them; this is the
        // grammar-side backstop so no caller can put an escape in the menu.
        !entry.is_empty()
            && entry
                .bytes()
                .all(|byte| is_word_byte(byte) && (0x21..=0x7e).contains(&byte))
            && !entry.starts_with('[')
            && !entry.starts_with('{')
            && !entry.starts_with("--")
    }
}

impl IncParse<()> for Words {
    fn step(&self, input: Input) -> Option<Step<()>> {
        let complete = |pos: usize| {
            self.alive
                .iter()
                .any(|&i| self.vocab[i as usize].word.len() == pos)
        };
        match input {
            Input::Eof => {
                if complete(self.pos) {
                    Some(Step::Done {
                        value: (),
                        rejected: input,
                    })
                } else {
                    None
                }
            }
            Input::Byte(byte) => {
                let b = byte.get();
                if is_boundary_byte(b) {
                    if complete(self.pos) {
                        Some(Step::Done {
                            value: (),
                            rejected: input,
                        })
                    } else {
                        None
                    }
                } else {
                    let alive: Vec<u32> = self
                        .alive
                        .iter()
                        .copied()
                        .filter(|&i| {
                            self.vocab[i as usize].word.as_bytes().get(self.pos) == Some(&b)
                        })
                        .collect();
                    if alive.is_empty() {
                        None
                    } else {
                        Some(Step::Continue(Box::new(Words {
                            vocab: self.vocab.clone(),
                            alive,
                            pos: self.pos + 1,
                        })))
                    }
                }
            }
        }
    }

    fn admissible(&self) -> Admissible {
        let mut charset = Charset::empty();
        let mut any_complete = false;
        for &i in &self.alive {
            let entry = self.vocab[i as usize].word.as_bytes();
            match entry.get(self.pos) {
                Some(&b) => charset.add(b),
                None => any_complete = true,
            }
        }
        Admissible::new(charset, !any_complete, false)
    }

    fn completions(&self, out: &mut Vec<Completion>) {
        for &i in &self.alive {
            let entry = &self.vocab[i as usize];
            out.push(Completion {
                word: entry.word.clone(),
                matched: self.pos,
                tag: entry.tag,
                desc: entry.desc.clone(),
                glue: entry.glue,
            });
        }
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(Words {
            vocab: self.vocab.clone(),
            alive: self.alive.clone(),
            pos: self.pos,
        })
    }
}

/// The vocabulary parser over a shared entry list (see [`Words`]). Entries that are
/// not single plain words are skipped.
pub fn hint_words(vocab: Rc<Vec<HintEntry>>) -> BoxP<()> {
    let alive: Vec<u32> = vocab
        .iter()
        .enumerate()
        .filter(|(_, entry)| Words::entry_is_word(&entry.word))
        .map(|(i, _)| i as u32)
        .collect();
    Box::new(Words {
        vocab,
        alive,
        pos: 0,
    })
}

/// [`hint_words`] over plain `(word, tag)` pairs (no descriptions, no glue).
pub fn words(vocab: Rc<Vec<(String, Tag)>>) -> BoxP<()> {
    let entries: Vec<HintEntry> = vocab
        .iter()
        .map(|(word, tag)| HintEntry::plain(word.clone(), *tag))
        .collect();
    hint_words(Rc::new(entries))
}

// ---------------------------------------------------------------------------
// Word — the free bare word
// ---------------------------------------------------------------------------

/// The free bare word: one or more word bytes, ended by a boundary — the lexer's
/// maximal munch. Configurable for its two grammar roles:
///
/// * `bare` words (names, values): may not *start* with `[` or `{` (the lexer reads
///   those as compound literals) or with `--` (that is a flag token), and may not be
///   *exactly* one of the `excluded` words (reserved words in name/value positions,
///   plus the command-dispatch words at the head of a run command) — typing beyond an
///   excluded word resumes normal life (`lets` is a fine word).
/// * flag names (`bare=false`, after `--`): any nonempty run of word bytes.
///
/// Words accept arbitrary non-ASCII text in eosh (`non_ascii_ok` everywhere here).
#[derive(Clone)]
pub struct Word {
    excluded: &'static [&'static str],
    bare: bool,
    len: usize,
    /// Consumed exactly "-" so far (only tracked for bare words: "--…" is a flag).
    dash1: bool,
    /// Bit i set: `excluded[i]` still equals the consumed prefix.
    excl_alive: u32,
}

/// What one word byte does to a [`Word`] state (shared between the plain and the
/// capturing wrappers).
enum WordStep {
    /// A boundary byte (or Eof) and the word is complete: finish, hand the input back.
    Finish,
    /// No parse continues through this byte.
    Fail,
    /// The byte extends the word.
    Advance(Word),
}

impl Word {
    fn finishable(&self) -> bool {
        self.len > 0
            && !self
                .excluded
                .iter()
                .enumerate()
                .any(|(i, word)| self.excl_alive & (1 << i) != 0 && word.len() == self.len)
    }

    fn step_core(&self, input: Input) -> WordStep {
        let byte = match input {
            Input::Eof => {
                return if self.finishable() {
                    WordStep::Finish
                } else {
                    WordStep::Fail
                };
            }
            Input::Byte(byte) => byte.get(),
        };
        if is_boundary_byte(byte) {
            return if self.finishable() {
                WordStep::Finish
            } else {
                WordStep::Fail
            };
        }
        if self.bare {
            if self.len == 0 && (byte == b'[' || byte == b'{') {
                return WordStep::Fail;
            }
            if self.dash1 && byte == b'-' {
                return WordStep::Fail;
            }
        }
        let mut excl_alive = 0u32;
        for (i, word) in self.excluded.iter().enumerate() {
            if self.excl_alive & (1 << i) != 0 && word.as_bytes().get(self.len) == Some(&byte) {
                excl_alive |= 1 << i;
            }
        }
        WordStep::Advance(Word {
            excluded: self.excluded,
            bare: self.bare,
            len: self.len + 1,
            dash1: self.bare && self.len == 0 && byte == b'-',
            excl_alive,
        })
    }

    fn admissible_core(&self) -> Admissible {
        let mut charset = WORD_CHARSET;
        if self.bare {
            if self.len == 0 {
                charset.remove(b'[');
                charset.remove(b'{');
            }
            if self.dash1 {
                charset.remove(b'-');
            }
        }
        Admissible::new(charset, !self.finishable(), true)
    }
}

impl IncParse<()> for Word {
    fn step(&self, input: Input) -> Option<Step<()>> {
        match self.step_core(input) {
            WordStep::Fail => None,
            WordStep::Finish => Some(Step::Done {
                value: (),
                rejected: input,
            }),
            WordStep::Advance(next) => Some(Step::Continue(Box::new(next))),
        }
    }

    fn admissible(&self) -> Admissible {
        self.admissible_core()
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(self.clone())
    }
}

/// [`Word`] plus the consumed text: finishes with the word it read. The grammar's M3
/// argument layer binds on the captured name to look up that program's (or that
/// flag's) completion data — acceptance is identical to the plain [`Word`].
///
/// The captured text is what was *stepped*: the editor substitutes the representative
/// byte for non-ASCII input (see `feed_bytes`), so a word containing non-ASCII text
/// captures mangled — which only makes the M3 lookup miss and fall back to the generic
/// grammar, never changes acceptance.
#[derive(Clone)]
pub struct CapWord {
    word: Word,
    text: String,
}

impl IncParse<String> for CapWord {
    fn step(&self, input: Input) -> Option<Step<String>> {
        match self.word.step_core(input) {
            WordStep::Fail => None,
            WordStep::Finish => Some(Step::Done {
                value: self.text.clone(),
                rejected: input,
            }),
            WordStep::Advance(next) => {
                let mut text = self.text.clone();
                if let Input::Byte(byte) = input {
                    text.push(char::from(byte.get()));
                }
                Some(Step::Continue(Box::new(CapWord { word: next, text })))
            }
        }
    }

    fn admissible(&self) -> Admissible {
        self.word.admissible_core()
    }

    fn clone_box(&self) -> BoxP<String> {
        Box::new(self.clone())
    }
}

fn word_state(excluded: &'static [&'static str], bare: bool) -> Word {
    assert!(excluded.len() <= 32, "exclusion mask is 32 bits");
    Word {
        excluded,
        bare,
        len: 0,
        dash1: false,
        excl_alive: u32::MAX >> (32 - excluded.len().max(1)),
    }
}

/// A bare word excluding the given exact words (see [`Word`]).
pub fn bare_word(excluded: &'static [&'static str]) -> BoxP<()> {
    Box::new(word_state(excluded, true))
}

/// [`bare_word`], capturing the consumed text (see [`CapWord`]).
pub fn cap_bare_word(excluded: &'static [&'static str]) -> BoxP<String> {
    Box::new(CapWord {
        word: word_state(excluded, true),
        text: String::new(),
    })
}

/// A flag name: a nonempty run of word bytes after `--`, no carve-outs.
pub fn flag_name() -> BoxP<()> {
    Box::new(word_state(&[], false))
}

/// [`flag_name`], capturing the consumed text (see [`CapWord`]).
pub fn cap_flag_name() -> BoxP<String> {
    Box::new(CapWord {
        word: word_state(&[], false),
        text: String::new(),
    })
}

// ---------------------------------------------------------------------------
// Quoted — string literal with the 5 escapes
// ---------------------------------------------------------------------------

/// A quoted string, mirroring `lex_quoted`: `"` … `"` with exactly the escapes
/// `\"  \\  \n  \t  \r` (any other escape is a lex error — inadmissible here).
/// Interior bytes are arbitrary text (non-ASCII fine).
#[derive(Clone)]
enum QuotedState {
    Open,
    Body,
    Escape,
    Finished,
}

#[derive(Clone)]
pub struct Quoted {
    state: QuotedState,
}

const fn quote_escape_charset() -> Charset {
    let mut cs = Charset::empty();
    cs.add(b'"');
    cs.add(b'\\');
    cs.add(b'n');
    cs.add(b't');
    cs.add(b'r');
    cs
}

impl IncParse<()> for Quoted {
    fn step(&self, input: Input) -> Option<Step<()>> {
        let byte = match input {
            Input::Eof => {
                return match self.state {
                    QuotedState::Finished => Some(Step::Done {
                        value: (),
                        rejected: input,
                    }),
                    // Unterminated string: a lex error in eosh.
                    _ => None,
                };
            }
            Input::Byte(byte) => byte.get(),
        };
        let next = match self.state {
            QuotedState::Open => {
                if byte == b'"' {
                    QuotedState::Body
                } else {
                    return None;
                }
            }
            QuotedState::Body => match byte {
                b'"' => QuotedState::Finished,
                b'\\' => QuotedState::Escape,
                _ => QuotedState::Body,
            },
            QuotedState::Escape => {
                if quote_escape_charset().contains(byte) {
                    QuotedState::Body
                } else {
                    // UnknownEscape in eosh.
                    return None;
                }
            }
            QuotedState::Finished => {
                return Some(Step::Done {
                    value: (),
                    rejected: input,
                });
            }
        };
        Some(Step::Continue(Box::new(Quoted { state: next })))
    }

    fn admissible(&self) -> Admissible {
        match self.state {
            QuotedState::Open => Admissible::new(Charset::singleton(b'"'), true, false),
            QuotedState::Body => Admissible::new(Charset::all(), true, true),
            QuotedState::Escape => Admissible::new(quote_escape_charset(), true, false),
            QuotedState::Finished => Admissible::TIME_TO_FINISH,
        }
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(self.clone())
    }
}

pub fn quoted() -> BoxP<()> {
    Box::new(Quoted {
        state: QuotedState::Open,
    })
}

// ---------------------------------------------------------------------------
// Compound — balanced […]/{…} literal
// ---------------------------------------------------------------------------

/// A compound literal, mirroring `lex_compound`: starts with `[` or `{`, runs verbatim
/// to where brackets and braces balance (kinds not matched — that is the type-directed
/// value parser's job in eosh, not the lexer's). Embedded quoted strings are opaque:
/// their brackets do not count, and inside them a backslash escapes *any* next byte
/// (unlike top-level quoted strings — `lex_compound` copies escapes through
/// unvalidated). It consumes the balancing closer and is then finished.
#[derive(Clone)]
enum CompoundState {
    Open,
    Plain(u32),
    Str(u32),
    StrEscape(u32),
    Finished,
}

#[derive(Clone)]
pub struct Compound {
    state: CompoundState,
}

impl IncParse<()> for Compound {
    fn step(&self, input: Input) -> Option<Step<()>> {
        let byte = match input {
            Input::Eof => {
                return match self.state {
                    CompoundState::Finished => Some(Step::Done {
                        value: (),
                        rejected: input,
                    }),
                    // UnterminatedCompound / UnterminatedString in eosh.
                    _ => None,
                };
            }
            Input::Byte(byte) => byte.get(),
        };
        let next = match self.state {
            CompoundState::Open => match byte {
                b'[' | b'{' => CompoundState::Plain(1),
                _ => return None,
            },
            CompoundState::Plain(depth) => match byte {
                b'[' | b'{' => CompoundState::Plain(depth + 1),
                b']' | b'}' => {
                    if depth == 1 {
                        CompoundState::Finished
                    } else {
                        CompoundState::Plain(depth - 1)
                    }
                }
                b'"' => CompoundState::Str(depth),
                _ => CompoundState::Plain(depth),
            },
            CompoundState::Str(depth) => match byte {
                b'"' => CompoundState::Plain(depth),
                b'\\' => CompoundState::StrEscape(depth),
                _ => CompoundState::Str(depth),
            },
            CompoundState::StrEscape(depth) => CompoundState::Str(depth),
            CompoundState::Finished => {
                return Some(Step::Done {
                    value: (),
                    rejected: input,
                });
            }
        };
        Some(Step::Continue(Box::new(Compound { state: next })))
    }

    fn admissible(&self) -> Admissible {
        match self.state {
            CompoundState::Open => {
                let mut cs = Charset::empty();
                cs.add(b'[');
                cs.add(b'{');
                Admissible::new(cs, true, false)
            }
            CompoundState::Finished => Admissible::TIME_TO_FINISH,
            // Inside, every byte is verbatim text.
            _ => Admissible::new(Charset::all(), true, true),
        }
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(self.clone())
    }
}

pub fn compound() -> BoxP<()> {
    Box::new(Compound {
        state: CompoundState::Open,
    })
}

// ---------------------------------------------------------------------------
// Ws — optional whitespace
// ---------------------------------------------------------------------------

/// Zero or more ASCII whitespace bytes. Stateless: always finishable, hands the first
/// non-whitespace byte back.
#[derive(Clone)]
pub struct Ws;

impl IncParse<()> for Ws {
    fn step(&self, input: Input) -> Option<Step<()>> {
        match input {
            Input::Byte(byte) if WS_CHARSET.contains(byte.get()) => {
                Some(Step::Continue(Box::new(Ws)))
            }
            _ => Some(Step::Done {
                value: (),
                rejected: input,
            }),
        }
    }

    fn admissible(&self) -> Admissible {
        Admissible::new(WS_CHARSET, false, false)
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(Ws)
    }
}

pub fn ws() -> BoxP<()> {
    Box::new(Ws)
}

// ---------------------------------------------------------------------------
// CommentRest — everything after '#'
// ---------------------------------------------------------------------------

/// The body of a comment: consumes every remaining byte (any text, non-ASCII fine)
/// and finishes at end of line.
#[derive(Clone)]
pub struct CommentRest;

impl IncParse<()> for CommentRest {
    fn step(&self, input: Input) -> Option<Step<()>> {
        match input {
            Input::Eof => Some(Step::Done {
                value: (),
                rejected: input,
            }),
            Input::Byte(_) => Some(Step::Continue(Box::new(CommentRest))),
        }
    }

    fn admissible(&self) -> Admissible {
        Admissible::new(Charset::all(), false, true)
    }

    fn clone_box(&self) -> BoxP<()> {
        Box::new(CommentRest)
    }
}

pub fn comment_rest() -> BoxP<()> {
    Box::new(CommentRest)
}

// ---------------------------------------------------------------------------
// Nat — carried decimal natural
// ---------------------------------------------------------------------------

/// A decimal natural number (carried). Not used by the eosh grammar (eosh numbers are
/// just words; typing is the callee's job), but part of the core kit. Accumulation
/// wraps on overflow rather than panicking (deviation from the source, which could
/// overflow in debug on adversarial input — this parser exists to gate *viability*,
/// not to produce trusted values).
#[derive(Clone)]
pub enum Nat {
    Empty,
    Acc(u64),
}

impl IncParse<u64> for Nat {
    fn step(&self, input: Input) -> Option<Step<u64>> {
        let digit = match input {
            Input::Eof => None,
            Input::Byte(byte) => {
                let b = byte.get();
                b.is_ascii_digit().then(|| (b - b'0') as u64)
            }
        };
        match (self, digit) {
            (Nat::Empty, Some(d)) => Some(Step::Continue(Box::new(Nat::Acc(d)))),
            (Nat::Empty, None) => None,
            (Nat::Acc(acc), Some(d)) => Some(Step::Continue(Box::new(Nat::Acc(
                acc.wrapping_mul(10).wrapping_add(d),
            )))),
            (Nat::Acc(acc), None) => Some(Step::Done {
                value: *acc,
                rejected: input,
            }),
        }
    }

    fn admissible(&self) -> Admissible {
        let mut charset = Charset::empty();
        let mut b = b'0';
        while b <= b'9' {
            charset.add(b);
            b += 1;
        }
        Admissible::new(charset, matches!(self, Nat::Empty), false)
    }

    fn clone_box(&self) -> BoxP<u64> {
        Box::new(self.clone())
    }
}

pub fn nat() -> BoxP<u64> {
    Box::new(Nat::Empty)
}

// ---------------------------------------------------------------------------
// Debug helper
// ---------------------------------------------------------------------------

/// A short human-readable description of an admissibility, for assertions.
pub fn describe_admissible(adm: &Admissible) -> String {
    format!(
        "{}{:?}{}",
        if adm.hard_required { "" } else { "⏎" },
        adm.charset,
        if adm.non_ascii_ok { "+u8" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::sanity_check;
    use crate::inc::{accepts, feed_bytes, finish, forced_prefix};

    fn ch(c: char) -> Input {
        Input::try_from(c).unwrap()
    }

    // Carried: test_lit.
    #[test]
    fn test_lit() {
        let lp1 = lit(b"abc");
        sanity_check(&*lp1);
        let lp2 = lp1.step(ch('a')).unwrap().assert_continue();
        sanity_check(&*lp2);
        let lp3 = lp2.step(ch('b')).unwrap().assert_continue();
        sanity_check(&*lp3);
        let lp4 = lp3.step(ch('c')).unwrap().assert_continue();
        sanity_check(&*lp4);
        let ((), rejected) = lp4.step(Input::Eof).unwrap().assert_done();
        assert_eq!(rejected, Input::Eof);

        let ((), rejected) = lp4.step(ch('d')).unwrap().assert_done();
        assert_eq!(rejected, ch('d'));

        assert!(lit(b"abc").step(ch('x')).is_none());
    }

    // Carried: test_bind.
    #[test]
    fn test_bind() {
        let lp1 = bind(lit(b"abc"), |()| pure(5));
        sanity_check(&*lp1);
        let lp2 = lp1.step(ch('a')).unwrap().assert_continue();
        sanity_check(&*lp2);
        let lp3 = lp2.step(ch('b')).unwrap().assert_continue();
        sanity_check(&*lp3);
        let lp4 = lp3.step(ch('c')).unwrap().assert_continue();
        sanity_check(&*lp4);
        let (i, rej) = lp4.step(Input::Eof).unwrap().assert_done();
        assert_eq!(i, 5);
        assert_eq!(rej, Input::Eof);

        let (i2, rej2) = lp4.step(ch('d')).unwrap().assert_done();
        assert_eq!(i2, 5);
        assert_eq!(rej2, ch('d'));

        assert!(lp3.step(ch('x')).is_none());
    }

    // Carried: test_alt + test_alt_multilen, on the n-ary Alt.
    #[test]
    fn test_alt() {
        let lp1 = alt(vec![
            bind(lit(b"abc"), |()| pure(true)),
            bind(lit(b"abde"), |()| pure(false)),
        ]);
        sanity_check(&*lp1);
        let lp2 = lp1.step(ch('a')).unwrap().assert_continue();
        sanity_check(&*lp2);
        let lp3 = lp2.step(ch('b')).unwrap().assert_continue();
        sanity_check(&*lp3);
        let lp4 = lp3.clone_box().step(ch('c')).unwrap().assert_continue();
        sanity_check(&*lp4);
        let (res, rej) = lp4.step(Input::Eof).unwrap().assert_done();
        assert!(res);
        assert_eq!(rej, Input::Eof);

        let lp4 = lp3.step(ch('d')).unwrap().assert_continue();
        let lp5 = lp4.step(ch('e')).unwrap().assert_continue();
        let (res, rej) = lp5.step(Input::Eof).unwrap().assert_done();
        assert!(!res);
        assert_eq!(rej, Input::Eof);
    }

    /// The fork that audio2's first-Done Alt could not represent: one branch finishes,
    /// another continues, and BOTH outcomes must stay live.
    #[test]
    fn alt_forks_done_and_continue() {
        // grammar: "a" optionally followed by "+b"; input "a+b" must parse, and "a"
        // must also parse — the state after 'a' is simultaneously finishable and
        // continuable.
        let g = || bind(lit(b"a"), |()| alt(vec![pure(()), lit(b"+b")]));
        assert!(accepts(g(), "a"));
        assert!(accepts(g(), "a+b"));
        assert!(!accepts(g(), "a+"));
        assert!(!accepts(g(), "a+c"));

        // Observe the fork directly: at '+', the pure branch finishes (a complete
        // parse of "a" rejecting '+') while the "+b" branch consumes — Both.
        let after_a = g().step(ch('a')).unwrap().assert_continue();
        assert!(matches!(after_a.step(ch('+')).unwrap(), Step::Both { .. }));
    }

    /// star() keeps the no-more-items branch alive in parallel with a started item:
    /// committing to a started item must not erase the already-viable shorter parse,
    /// and a dead item must not retroactively accept its consumed bytes.
    #[test]
    fn star_does_not_commit() {
        // item: one or more spaces then "ab".
        let item = || {
            bind(ws(), |()| lit(b"ab")) // ws is zero+, but the item needs 'a' eventually
        };
        let g = || bind(lit(b"x"), move |()| bind(star(item), |()| lit(b"$")));
        assert!(accepts(g(), "x$"));
        assert!(accepts(g(), "xab$"));
        assert!(accepts(g(), "x ab$"));
        assert!(accepts(g(), "x ab  ab$"));
        // The spaces were consumed by a started item whose 'ab' never came: the
        // trailing-$ parse must still see the '$' (spaces belonged to the dead item,
        // but ws-then-$ is not in this grammar — "x $" must FAIL).
        assert!(!accepts(g(), "x $"));
        assert!(!accepts(g(), "xa$"));
    }

    // Carried shape: LitsParser tests, on Words (with boundary semantics).
    #[test]
    fn test_words() {
        let vocab = Rc::new(vec![
            (String::from("abc"), Tag::Program),
            (String::from("abde"), Tag::Binding),
        ]);
        let w = words(vocab.clone());
        sanity_check(&*w);
        let w2 = w.step(ch('a')).unwrap().assert_continue();
        sanity_check(&*w2);
        let w3 = w2.step(ch('b')).unwrap().assert_continue();
        sanity_check(&*w3);

        // Two completions alive at "ab".
        let mut comps = Vec::new();
        w3.completions(&mut comps);
        assert_eq!(comps.len(), 2);
        assert!(comps.iter().all(|c| c.matched == 2));

        let w4 = w3.clone_box().step(ch('c')).unwrap().assert_continue();
        sanity_check(&*w4);
        let ((), rej) = w4.step(Input::Eof).unwrap().assert_done();
        assert_eq!(rej, Input::Eof);

        // Boundary semantics: "abc" finishes at ' ' but not at a word byte.
        let w4 = w3.clone_box().step(ch('c')).unwrap().assert_continue();
        let ((), rej) = w4.clone_box().step(ch(' ')).unwrap().assert_done();
        assert_eq!(rej, ch(' '));
        let w4 = w3.step(ch('c')).unwrap().assert_continue();
        assert!(w4.step(ch('x')).is_none());

        // Invalid entries are dropped at construction.
        let filtered = words(Rc::new(vec![
            (String::from("ok"), Tag::Program),
            (String::from("--flag"), Tag::Program),
            (String::from("[comp"), Tag::Program),
            (String::from("two words"), Tag::Program),
            (String::from(""), Tag::Program),
        ]));
        let mut comps = Vec::new();
        filtered.completions(&mut comps);
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].word, "ok");
    }

    // Carried: test_nat.
    #[test]
    fn test_nat() {
        let lp1 = nat();
        sanity_check(&*lp1);
        let lp2 = lp1.step(ch('1')).unwrap().assert_continue();
        sanity_check(&*lp2);
        let lp3 = lp2.step(ch('2')).unwrap().assert_continue();
        sanity_check(&*lp3);
        let lp4 = lp3.step(ch('3')).unwrap().assert_continue();
        sanity_check(&*lp4);
        let (num, input) = lp4.clone_box().step(Input::Eof).unwrap().assert_done();
        assert_eq!(num, 123);
        assert_eq!(input, Input::Eof);

        let (num, input) = lp4.step(ch(' ')).unwrap().assert_done();
        assert_eq!(num, 123);
        assert_eq!(input, ch(' '));
    }

    #[test]
    fn test_kw_boundary() {
        let k = || kw("let", Tag::Builtin);
        assert!(accepts(k(), "let"));
        assert!(!accepts(k(), "letx"));
        assert!(!accepts(k(), "le"));
        // Completes only at a boundary; the boundary byte is rejected back.
        let s = feed_bytes(k(), b"let").unwrap();
        sanity_check(&*s);
        let ((), rej) = s.step(ch(' ')).unwrap().assert_done();
        assert_eq!(rej, ch(' '));
        // '[' is NOT a boundary in eosh ("only[a]" lexes as one word).
        let s = feed_bytes(k(), b"let").unwrap();
        assert!(s.step(ch('[')).is_none());
    }

    #[test]
    fn test_bare_word() {
        const EXCL: &[&str] = &["let", "as"];
        let w = || bare_word(EXCL);
        assert!(accepts(w(), "hello"));
        assert!(accepts(w(), "time.frozen"));
        assert!(accepts(w(), "eo9:fs/fs@0.1.0"));
        assert!(accepts(w(), "-"));
        assert!(accepts(w(), "-5"));
        assert!(accepts(w(), "a--b")); // '-' runs are fine mid-word
        assert!(accepts(w(), "lets")); // excluded word + more = fine
        assert!(accepts(w(), "x[y]")); // brackets mid-word are word bytes
        assert!(accepts(w(), "]")); // and a closer can even start a word
        assert!(!accepts(w(), "")); // empty is not a word
        assert!(!accepts(w(), "let")); // exact excluded word
        assert!(!accepts(w(), "as"));
        assert!(!accepts(w(), "--flag")); // flag token, not a word
        assert!(!accepts(w(), "[x]")); // compound start
        assert!(!accepts(w(), "{x}"));
        assert!(!accepts(w(), "a b")); // boundary ends the word; trailing byte refused

        // flag_name: no carve-outs.
        assert!(accepts(flag_name(), "-x-"));
        assert!(accepts(flag_name(), "[a]"));
        assert!(!accepts(flag_name(), ""));

        // Walk states under the checker.
        let mut state = w();
        sanity_check(&*state);
        for &b in b"let" {
            state = state
                .step(Input::byte(b).unwrap())
                .unwrap()
                .assert_continue();
            sanity_check(&*state);
        }
    }

    #[test]
    fn test_quoted() {
        let q = quoted;
        assert!(accepts(q(), r#""hello""#));
        assert!(accepts(q(), r#""""#));
        assert!(accepts(q(), r#""a \"b\" \\ c\nd \t \r""#));
        assert!(accepts(q(), r#""structural $ & ( ) , = # stay text""#));
        assert!(!accepts(q(), r#""unterminated"#));
        assert!(!accepts(q(), r#""bad \q escape""#));
        assert!(!accepts(q(), r#"x"""#));

        let mut state = q();
        sanity_check(&*state);
        for &b in br#""a\"# {
            state = state
                .step(Input::byte(b).unwrap())
                .unwrap()
                .assert_continue();
            sanity_check(&*state);
        }
    }

    #[test]
    fn test_compound() {
        let c = compound;
        assert!(accepts(c(), "[]"));
        assert!(accepts(
            c(),
            "[{segment: 0, bus: 0, device: 1, function: 0}]"
        ));
        assert!(accepts(c(), "[[1, 2], [3, 4]]"));
        assert!(accepts(c(), "{a: some(1), b: (5)}"));
        // Kind mismatches balance — the lexer only counts (typing is downstream).
        assert!(accepts(c(), "[}"));
        // Embedded strings are opaque; any escape goes.
        assert!(accepts(c(), r#"["a]b", "c,{d", "e\"]f"]"#));
        assert!(accepts(c(), r#"["\q"]"#));
        assert!(!accepts(c(), "[{segment: 0"));
        assert!(!accepts(c(), r#"["unclosed"#));
        assert!(!accepts(c(), "[] ")); // trailing byte refused (it is a new token)

        let mut state = c();
        sanity_check(&*state);
        for &b in br#"[{a:"x\y"# {
            state = state
                .step(Input::byte(b).unwrap())
                .unwrap()
                .assert_continue();
            sanity_check(&*state);
        }
    }

    #[test]
    fn test_ws_and_comment() {
        assert!(accepts(ws(), ""));
        assert!(accepts(ws(), " \t  "));
        assert!(!accepts(ws(), " x"));
        sanity_check(&*ws());

        let g = || bind(lit_byte(b'#'), |()| comment_rest());
        assert!(accepts(g(), "#"));
        assert!(accepts(g(), "# anything at all $ & ( \" ["));
        assert!(!accepts(g(), "x#"));
        sanity_check(&*comment_rest());
    }

    #[test]
    fn test_forced_prefix() {
        // A literal forces its whole body.
        assert_eq!(forced_prefix(&*lit(b"abc")), b"abc");
        // A keyword forces its body but stops at the (soft) completed state.
        assert_eq!(forced_prefix(&*kw("describe", Tag::Builtin)), b"describe");
        // Mid-keyword: the rest is forced. alt of two keywords with a shared prefix
        // forces nothing until the prefix disambiguates.
        let two = alt(vec![kw("exit", Tag::Builtin), kw("env", Tag::Builtin)]);
        assert_eq!(forced_prefix(&*two), b"e");
        let after_e = feed_bytes(
            alt(vec![kw("exit", Tag::Builtin), kw("env", Tag::Builtin)]),
            b"ex",
        )
        .unwrap();
        assert_eq!(forced_prefix(&*after_e), b"it");
        // A finishable state forces nothing even with one admissible byte.
        let opt = alt(vec![pure(()), lit(b"z")]);
        assert_eq!(forced_prefix(&*opt), b"");
        // Words with a single alive entry force... nothing here either: a free-word
        // grammar never has a single admissible byte, but a lone Words does.
        let v = words(Rc::new(vec![(String::from("only"), Tag::Keyword)]));
        assert_eq!(forced_prefix(&*v), b"only");
    }

    #[test]
    fn test_feed_non_ascii_policy() {
        // Words take non-ASCII text bytes.
        let w = || bare_word(&[]);
        assert!(accepts(w(), "héllo"));
        assert!(accepts(w(), "é"));
        // Quoted interiors too; escape position does not.
        assert!(accepts(quoted(), "\"héllo\""));
        assert!(!accepts(quoted(), "\"a\\é\""));
        // A literal does not.
        assert!(!accepts(lit(b"abc"), "aéc"));
        // Eof'd finish helper.
        let state = feed_bytes(w(), "héllo".as_bytes()).unwrap();
        assert!(finish(&*state).is_some());
    }

    #[test]
    fn test_rep() {
        let g = |n: usize| {
            bind(lit(b"("), move |()| {
                bind(rep(n, || lit(b"x")), |()| lit(b")"))
            })
        };
        assert!(accepts(g(0), "()"));
        assert!(accepts(g(3), "(xxx)"));
        assert!(!accepts(g(3), "(xx)"));
        assert!(!accepts(g(3), "(xxxx)"));
    }

    #[test]
    fn test_lazy() {
        // Balanced 'a' … 'b' nesting via explicit lazy recursion: g := "ab" | "a" g "b"
        fn g() -> BoxP<()> {
            alt(vec![
                lit(b"ab"),
                bind(lit(b"a"), |()| bind(lazy(g), |()| lit(b"b"))),
            ])
        }
        assert!(accepts(g(), "ab"));
        assert!(accepts(g(), "aaabbb"));
        assert!(!accepts(g(), "aab"));
        sanity_check(&*g());
    }
}
