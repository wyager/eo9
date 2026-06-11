//! The v1 eosh grammar, expressed incrementally — a byte-level mirror of
//! eosh-core's lexer + parser composed (`eosh-core/src/{lex,parse}.rs`):
//!
//! ```text
//! line         := ws command? ws ( "#" anything )? EOF
//! command      := "let"  name "=" expr | "save" name "=" expr
//!               | "detach" name "=" expr                          (LOOSE — below)
//!               | "svc" ( ε | "list" | ("log"|"stop"|"clear") name )
//!               | "help" | "history" | "exit" | "quit" | "poweroff"
//!               | "env" ( ε | expr ) | "describe" ("$"|"&"|carded-word|expr)
//!               | "man" ( "$" | "&" | word | compound )            (one token, then EOF)
//!               | "imports" expr
//!               | expr                  (head name not a dispatch word — eosh's
//!                                        command() matches the first Word token)
//! expr         := gate | amp-expr ( "$" expr )?                   ($ right-assoc)
//! gate         := "only" name ("," name)* "$" expr
//!               | "rename" name name "$" expr
//!               | "with" with-item ("," with-item)* "$" expr
//! amp-expr     := app-expr ( "&" app-expr )*                      (& left-assoc)
//! app-expr     := primary arg*                                    (application tightest)
//! primary      := name | "(" expr ")"
//! arg          := "--" flagname value | name | quoted | compound | "(" expr ")"
//! value        := name | quoted | compound | "(" expr ")"
//! with-item    := "(" expr ")" "as" name
//!               | "(" expr ("," expr)+ ")" "as" "(" name ("," name)N ")"   (arity ==)
//!               | amp-expr-with-non-paren-head "as" name
//! name         := bare word (reserved excluded) | compound        (the lexer calls
//!                                                                  both Token::Word)
//! ```
//!
//! Reserved words (`let only rename with as`) are excluded exactly (typing past one
//! resumes normal word life). Name positions also alternate a [`crate::comb::Words`]
//! vocabulary — acceptance is unchanged (the free word already covers every vocabulary
//! word), it exists to power `completions()`; soundness never depends on the vocabulary.
//!
//! ARGUMENT COMPLETION (M3, docs/design/component-manuals.md §3–4): when the embedder
//! has provided a resolved program's argument data ([`Vocab::programs`]), the
//! application grammar captures the head name and binds the matching argument layer in:
//! flag-name positions additionally alternate the program's `--flag` names (from the
//! WIT `describe` signature, with the manual's per-arg doc line as the menu
//! description), and each flag's value position additionally alternates its typed
//! candidates — `union(wit_grammar(ty), words(hint_literals))`. THE HARD RULE (the
//! manuals design's §3): hints are ADDITIVE, NEVER RESTRICTIVE — every added `Words`
//! branch's language is a subset of the free word it alternates with (the entry filter
//! in [`crate::comb::Words`] plus the reserved-word filter below guarantee it), so the
//! WIT-derived/free branch is unconditionally present and a lying manual can produce a
//! false green (a candidate eosh later refuses), never a false red. The manual-fuzzing
//! arm of the differential test pins acceptance EQUAL with and without argument data.
//! Names with no provided data (unresolved, still resolving, or argument-less) keep
//! the generic v1 argument grammar unchanged.
//!
//! DELIBERATE LOOSENESS (the superset rule tolerates false green, never false red):
//!
//! * `detach <name> = <expr>` does not require eosh's `restart <policy>` clause: in the
//!   real parser the line is split at the last top-level `restart` *word* and both
//!   halves must parse — but since `restart` is an ordinary word, any program+policy
//!   pair that eosh accepts is also one plain `expr` (the policy words become
//!   application arguments), so the loose form is a strict superset. The cost: a
//!   `detach` line missing its `restart` clause shows green and fails at parse time.
//! * Bytes >= 0x80 step as the representative text byte (see
//!   [`crate::inc::feed_bytes`]): acceptance can only widen.
//!
//! Everything else is tight by construction and pinned by the differential test below:
//! gates require `$`, `with` tuple arity must match, `svc` subcommands are exact,
//! no-argument builtins reject arguments, reserved words are refused in name/value
//! positions, flags require values.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::comb::{
    HintEntry, Words, alt, bare_word, bind, cap_bare_word, cap_flag_name, comment_rest, compound,
    flag_name, hint_words, kw, lazy, lit, lit_byte, map, pure, quoted, rep, star, ws,
};
use crate::inc::{BoxP, Tag};

/// Words that may not appear as bare names or bare argument values
/// (`eosh-core/src/parse.rs::is_reserved`).
const RESERVED: &[&str] = &["as", "let", "only", "rename", "with"];

/// Words that, as the whole first word of a line, are claimed by eosh's command
/// dispatch (`parse.rs::command()`) and therefore cannot head a plain run-expression:
/// the dispatch words plus the reserved words.
const CMD_HEAD_EXCLUDED: &[&str] = &[
    "as", "describe", "detach", "env", "exit", "help", "history", "imports", "let", "man", "only",
    "poweroff", "quit", "rename", "save", "svc", "with",
];

/// The dynamic vocabulary for name positions: builtins are spoken for by the grammar's
/// own keyword branches; entries here are the store's /bin listing, session bindings,
/// and anything else the embedder wants completable. Snapshotted per prompt (M2).
///
/// `programs` is the M3 argument-completion layer: per resolved program name, the
/// argument data the embedder pulled from `describe` (and the component's manual,
/// when present). Lazily populated — the editor asks for names as they complete
/// ([`crate::editor::Editor::wanted_args`]) and the embedder fills entries in as
/// resolution finishes; absent names simply keep the generic argument grammar.
#[derive(Debug, Clone, Default)]
pub struct Vocab {
    pub entries: Vec<(String, Tag)>,
    pub programs: BTreeMap<String, ProgramArgs>,
}

impl Vocab {
    pub fn new(entries: Vec<(String, Tag)>) -> Self {
        Vocab {
            entries,
            programs: BTreeMap::new(),
        }
    }
}

/// One resolved program's argument data (M3): the flags of its `main`/`configure`
/// signature, dressed with whatever the manual added. The WIT signature is the
/// mechanical truth — the embedder builds one [`FlagSpec`] per `describe` ArgSpec and
/// only *annotates* it from the manual (doc line, `values:` literals, `kind:` tag);
/// manual-only flags never appear here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramArgs {
    pub flags: Vec<FlagSpec>,
}

/// One flag of a resolved program's signature, plus its (additive) completion hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagSpec {
    /// The flag name, without the `--`.
    pub name: String,
    /// The WIT type text (`bool`, `option<string>`, …) — drives the typed candidates.
    pub ty: String,
    /// The manual's per-arg doc first line, shown as the candidate-list description.
    pub doc: Option<String>,
    /// The manual's `values:` literals — ADDITIVE candidates only.
    pub values: Vec<String>,
    /// The manual's `kind:` tag (url, path, component-name, …) — a candidate SOURCE,
    /// never a constraint.
    pub kind: Option<String>,
}

/// Where a name sits: the head of a run-command excludes the dispatch words.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Pos {
    Head,
    Normal,
}

/// One program's argument data converted to ready-to-alternate `Words` entries
/// (computed once per [`command_line`] build, shared by `Rc` through every lazy
/// grammar rebuild): the flag-name vocabulary and, per flag, its value candidates.
#[derive(Clone)]
struct ProgramSlots {
    flags: Rc<Vec<HintEntry>>,
    values: Rc<BTreeMap<String, Rc<Vec<HintEntry>>>>,
}

/// The grammar's shared context: the vocabulary, pre-filtered per position, the
/// per-program argument slots (M3), and the card vocabularies for `describe`/`man`
/// argument positions (the carded shell words from `eosh_core::builtins::card_words`
/// plus the API card words from `eosh_core::apidocs::api_words` — the SAME tables the
/// session routes and renders from, so acceptance, completion, and marking are three
/// views of one datum).
#[derive(Clone)]
struct Cx {
    head_vocab: Rc<Vec<HintEntry>>,
    vocab: Rc<Vec<HintEntry>>,
    programs: Rc<BTreeMap<String, ProgramSlots>>,
    /// Carded words (builtin/operator cards + API package/interface cards): the
    /// completable argument vocabulary of `describe`. Acceptance-relevant ONLY for the
    /// reserved carded words (`let only rename with` — the free expression word covers
    /// every other entry).
    cards: Rc<Vec<HintEntry>>,
    /// `man`'s argument vocabulary: the cards plus the /bin programs (a manual lives on
    /// a card or on a program; bindings have no manual).
    man_vocab: Rc<Vec<HintEntry>>,
}

/// Sequence two parsers, keeping the first's laziness discipline: the second is built
/// eagerly (cheap) and cloned per completion of the first. Recursive references must
/// NOT go through this — use [`expr_l`].
fn then<T: 'static>(first: BoxP<()>, second: BoxP<T>) -> BoxP<T> {
    bind(first, move |()| second.clone())
}

/// `seq!(a, b, c)` = `then(a, then(b, c))`.
macro_rules! seq {
    ($first:expr, $($rest:expr),+ $(,)?) => { then($first, seq!($($rest),+)) };
    ($only:expr $(,)?) => { $only };
}

// -- tokens (each consumes optional leading whitespace) ---------------------------

fn t_byte(byte: u8) -> BoxP<()> {
    then(ws(), lit_byte(byte))
}

fn t_kw(word: &'static str, tag: Tag) -> BoxP<()> {
    then(ws(), kw(word, tag))
}

/// A plain word where eosh runs `expect_word`: a non-reserved bare word — or a
/// compound literal, which the lexer also hands over as `Token::Word` (yes, `let [a] =
/// x` parses in eosh; mirroring that is part of the superset rule).
fn t_plain_word() -> BoxP<()> {
    then(ws(), alt(vec![bare_word(RESERVED), compound()]))
}

/// A lazy reference to `expr` — every recursive edge of the grammar goes through here.
fn expr_l(cx: &Cx, pos: Pos) -> BoxP<()> {
    let cx = cx.clone();
    lazy(move || expr(&cx, pos))
}

/// `"(" expr ")"` (no leading-whitespace handling: callers sit behind a `ws`).
fn paren_expr(cx: &Cx) -> BoxP<()> {
    seq!(lit_byte(b'('), expr_l(cx, Pos::Normal), t_byte(b')'))
}

// -- expressions -------------------------------------------------------------------

/// A name in `primary` position, captured: free word (position-appropriate
/// exclusions), compound, or a vocabulary word (completions only — see module docs).
/// The captured value is the consumed word for the word-shaped branches and `""` for
/// the structural ones. The capturing branch sits FIRST deliberately: every word the
/// vocabulary branch finishes, the free word finishes too (the vocabulary survives
/// [`Words`]' entry filter, so its language is a subset), and `Alt`'s first-finisher
/// rule then always yields the real name to the argument-layer bind.
fn cap_primary(cx: &Cx, pos: Pos) -> BoxP<String> {
    let (excluded, vocab) = match pos {
        Pos::Head => (CMD_HEAD_EXCLUDED, cx.head_vocab.clone()),
        Pos::Normal => (RESERVED, cx.vocab.clone()),
    };
    then(
        ws(),
        alt(vec![
            cap_bare_word(excluded),
            map(hint_words(vocab), |()| String::new()),
            map(compound(), |()| String::new()),
            map(paren_expr(cx), |()| String::new()),
        ]),
    )
}

/// A flag-or-positional application argument (the generic v1 layer — no program data).
fn arg(cx: &Cx) -> BoxP<()> {
    then(
        ws(),
        alt(vec![
            // `--name value`: the flag name is any nonempty word-byte run; the value
            // is mandatory (eosh: "a value after the flag").
            seq!(lit(b"--"), flag_name(), value(cx)),
            bare_word(RESERVED),
            compound(),
            quoted(),
            paren_expr(cx),
        ]),
    )
}

/// A flag value: word, quoted string, compound, or parenthesized expression.
fn value(cx: &Cx) -> BoxP<()> {
    then(
        ws(),
        alt(vec![
            bare_word(RESERVED),
            compound(),
            quoted(),
            paren_expr(cx),
        ]),
    )
}

fn star_args(cx: &Cx) -> BoxP<()> {
    let cx = cx.clone();
    star(move || arg(&cx))
}

/// The argument list for a captured head name (M3): the program's slots when the
/// embedder provided them, the generic layer otherwise. Identical acceptance either
/// way — the slots only add completion branches (module docs, the hard rule).
fn star_args_for(cx: &Cx, name: &str) -> BoxP<()> {
    match cx.programs.get(name) {
        Some(program) => {
            let cx = cx.clone();
            let program = program.clone();
            star(move || arg_known(&cx, &program))
        }
        None => star_args(cx),
    }
}

/// [`arg`] with one program's slots alternated in: the flag token captures its name
/// (offering the program's flags alongside), and the bound value position offers that
/// flag's typed candidates alongside the free forms.
fn arg_known(cx: &Cx, program: &ProgramSlots) -> BoxP<()> {
    let flag = alt(vec![
        cap_flag_name(),
        map(hint_words(program.flags.clone()), |()| String::new()),
    ]);
    let value_cx = cx.clone();
    let values = program.values.clone();
    then(
        ws(),
        alt(vec![
            then(
                lit(b"--"),
                bind(flag, move |name| {
                    value_hinted(&value_cx, values.get(&name).cloned())
                }),
            ),
            bare_word(RESERVED),
            compound(),
            quoted(),
            paren_expr(cx),
        ]),
    )
}

/// [`value`] plus a flag's candidate words, when it has any.
fn value_hinted(cx: &Cx, hints: Option<Rc<Vec<HintEntry>>>) -> BoxP<()> {
    let mut branches = vec![bare_word(RESERVED), compound(), quoted(), paren_expr(cx)];
    if let Some(hints) = hints {
        branches.push(hint_words(hints));
    }
    then(ws(), alt(branches))
}

/// `app-expr := primary arg*`, with the head parser supplied (the `with`-item path
/// needs a non-paren head).
fn app_from(head: BoxP<()>, cx: &Cx) -> BoxP<()> {
    then(head, star_args(cx))
}

fn app(cx: &Cx, pos: Pos) -> BoxP<()> {
    let cx2 = cx.clone();
    bind(cap_primary(cx, pos), move |name| star_args_for(&cx2, &name))
}

fn star_amp(cx: &Cx) -> BoxP<()> {
    let cx = cx.clone();
    star(move || then(t_byte(b'&'), app(&cx, Pos::Normal)))
}

fn amp(cx: &Cx, pos: Pos) -> BoxP<()> {
    then(app(cx, pos), star_amp(cx))
}

fn expr(cx: &Cx, pos: Pos) -> BoxP<()> {
    alt(vec![
        gate_only(cx),
        gate_rename(cx),
        gate_with(cx),
        then(
            amp(cx, pos),
            alt(vec![pure(()), then(t_byte(b'$'), expr_l(cx, Pos::Normal))]),
        ),
    ])
}

// -- gates ---------------------------------------------------------------------------

fn gate_only(cx: &Cx) -> BoxP<()> {
    seq!(
        t_kw("only", Tag::Keyword),
        t_plain_word(),
        star(|| seq!(t_byte(b','), t_plain_word())),
        t_byte(b'$'),
        expr_l(cx, Pos::Normal),
    )
}

fn gate_rename(cx: &Cx) -> BoxP<()> {
    seq!(
        t_kw("rename", Tag::Keyword),
        t_plain_word(),
        t_plain_word(),
        t_byte(b'$'),
        expr_l(cx, Pos::Normal),
    )
}

fn gate_with(cx: &Cx) -> BoxP<()> {
    let item_cx = cx.clone();
    seq!(
        t_kw("with", Tag::Keyword),
        with_item(cx),
        star(move || seq!(t_byte(b','), with_item(&item_cx))),
        t_byte(b'$'),
        expr_l(cx, Pos::Normal),
    )
}

/// One `with` binding. eosh's `with_item` COMMITS on a leading `(`: after `( expr )`
/// only `as <slot>` may follow (`with (a) & b as x` is an error), and the tuple form
/// `( e1, e2, … ) as ( s1, s2, … )` must match arities — mirrored here by binding the
/// provider count into the slot grammar (the monadic bind earning its keep).
fn with_item(cx: &Cx) -> BoxP<()> {
    alt(vec![
        seq!(
            t_byte(b'('),
            expr_l(cx, Pos::Normal),
            alt(vec![
                // Single parenthesized provider: `) as slot`.
                seq!(t_byte(b')'), t_kw("as", Tag::Keyword), t_plain_word()),
                // Tuple: `, e2 [, …] ) as ( s1 [, …] )` with matching arity.
                bind(more_exprs(cx), |extra| {
                    seq!(
                        t_kw("as", Tag::Keyword),
                        t_byte(b'('),
                        t_plain_word(),
                        rep(extra, || seq!(t_byte(b','), t_plain_word())),
                        t_byte(b')'),
                    )
                }),
            ]),
        ),
        // Non-paren-headed amp-expr: `provider as slot`.
        seq!(
            then(
                app_from(
                    then(
                        ws(),
                        alt(vec![
                            bare_word(RESERVED),
                            compound(),
                            hint_words(cx.vocab.clone()),
                        ]),
                    ),
                    cx,
                ),
                star_amp(cx),
            ),
            t_kw("as", Tag::Keyword),
            t_plain_word(),
        ),
    ])
}

/// `("," expr)+ ")"`, counting the extra providers (the first sits before this).
fn more_exprs(cx: &Cx) -> BoxP<usize> {
    let cx2 = cx.clone();
    then(
        seq!(t_byte(b','), expr_l(cx, Pos::Normal)),
        alt(vec![
            map(t_byte(b')'), |()| 1usize),
            lazy(move || map(more_exprs(&cx2), |extra| extra + 1)),
        ]),
    )
}

// -- commands --------------------------------------------------------------------------

/// `<keyword> <name> = <expr>` (let, save, and the deliberately loose detach).
fn name_eq_expr(cx: &Cx, word: &'static str) -> BoxP<()> {
    seq!(
        t_kw(word, Tag::Builtin),
        t_plain_word(),
        t_byte(b'='),
        expr_l(cx, Pos::Normal),
    )
}

fn command(cx: &Cx) -> BoxP<()> {
    alt(vec![
        name_eq_expr(cx, "let"),
        name_eq_expr(cx, "save"),
        // LOOSE: no `restart <policy>` clause required — see module docs.
        name_eq_expr(cx, "detach"),
        then(
            t_kw("svc", Tag::Builtin),
            alt(vec![
                pure(()),
                t_kw("list", Tag::Builtin),
                then(
                    alt(vec![
                        t_kw("log", Tag::Builtin),
                        t_kw("stop", Tag::Builtin),
                        t_kw("clear", Tag::Builtin),
                    ]),
                    t_plain_word(),
                ),
            ]),
        ),
        t_kw("help", Tag::Builtin),
        t_kw("history", Tag::Builtin),
        t_kw("exit", Tag::Builtin),
        t_kw("quit", Tag::Builtin),
        t_kw("poweroff", Tag::Builtin),
        then(
            t_kw("env", Tag::Builtin),
            alt(vec![pure(()), expr_l(cx, Pos::Normal)]),
        ),
        then(
            t_kw("describe", Tag::Builtin),
            alt(vec![
                t_byte(b'$'),
                t_byte(b'&'),
                // The carded words (builtins, operators, API names). Acceptance-wise
                // this branch matters only for the reserved carded words (`describe
                // only` is a card; plain words like `describe help` already parse as
                // expressions) — but completion-wise it is what makes `describe d⇥`
                // offer `describe`'s own card, and marking-wise what keeps a typed
                // carded word green (the owner's `describe describe` report).
                then(ws(), hint_words(cx.cards.clone())),
                expr_l(cx, Pos::Normal),
            ]),
        ),
        // `man <one token>`: exactly one bare word (reserved words included — `man let`
        // is a card), a compound (the lexer hands `[a]` over as one Word token), or a
        // lone `$`/`&` operator; nothing may follow but trailing space/comment.
        then(
            t_kw("man", Tag::Builtin),
            then(
                ws(),
                alt(vec![
                    lit_byte(b'$'),
                    lit_byte(b'&'),
                    bare_word(&[]),
                    compound(),
                    hint_words(cx.man_vocab.clone()),
                ]),
            ),
        ),
        then(t_kw("imports", Tag::Builtin), expr_l(cx, Pos::Normal)),
        expr(cx, Pos::Head),
        pure(()),
    ])
}

/// The card vocabulary: every word with a builtin/operator card and every API card
/// word, tagged Builtin. Built once per [`command_line`] (the tables are static).
fn card_entries() -> Vec<HintEntry> {
    let mut entries: Vec<HintEntry> = eosh_core::builtins::card_words()
        .map(|word| HintEntry::plain(String::from(word), Tag::Builtin))
        .collect();
    entries.extend(
        eosh_core::apidocs::api_words()
            .into_iter()
            .map(|word| HintEntry::plain(word, Tag::Builtin)),
    );
    entries
}

/// Trailing whitespace and an optional comment, then end of line.
fn trailing() -> BoxP<()> {
    then(
        ws(),
        alt(vec![pure(()), then(lit_byte(b'#'), comment_rest())]),
    )
}

/// The typed value candidates the WIT type text alone yields: `bool` (under any
/// `option<…>` nesting) offers its two literals. Everything else is free-form at the
/// grammar's level — richer typing is the callee's job.
fn wit_value_entries(ty: &str) -> Vec<HintEntry> {
    let mut inner = ty.trim();
    while let Some(stripped) = inner
        .strip_prefix("option<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        inner = stripped.trim();
    }
    match inner {
        "bool" => ["false", "true"]
            .iter()
            .map(|word| HintEntry::plain(String::from(*word), Tag::Value))
            .collect(),
        _ => Vec::new(),
    }
}

/// The candidate entries for one flag's value position: the WIT-derived words, the
/// manual's `values:` literals, and the `kind:` tag's candidate source. Every entry is
/// filtered to the plain-word subset AND against the reserved words, so the added
/// `Words` branch never widens (or narrows) what the generic value position accepts —
/// hints are completion-only (module docs, the hard rule).
fn value_entries(spec: &FlagSpec, vocab: &[HintEntry]) -> Vec<HintEntry> {
    let mut entries = wit_value_entries(&spec.ty);
    for value in &spec.values {
        let word = value.trim();
        if Words::entry_is_word(word) && !RESERVED.contains(&word) {
            entries.push(HintEntry::plain(String::from(word), Tag::Value));
        }
    }
    match spec.kind.as_deref() {
        // A canned prefix to keep typing into (`glue`: no trailing space on a unique
        // completion) plus the kind as its label.
        Some("url") => entries.push(HintEntry {
            word: String::from("http://"),
            tag: Tag::Value,
            desc: Some(String::from("url")),
            glue: true,
        }),
        Some("path") => entries.push(HintEntry {
            word: String::from("/"),
            tag: Tag::Value,
            desc: Some(String::from("path")),
            glue: true,
        }),
        // The per-prompt dynamic vocabulary as candidates — retagged Value: a value
        // position takes free text, so its candidates must never read as evidence of
        // a name position (the editor's name-marking oracle keys on the tag).
        Some("component-name") => entries.extend(vocab.iter().map(|entry| HintEntry {
            word: entry.word.clone(),
            tag: Tag::Value,
            desc: None,
            glue: false,
        })),
        // port, interface-name, unknown kinds: display text in `man` only.
        _ => {}
    }
    entries
}

/// Convert one program's [`ProgramArgs`] into ready-to-alternate slots.
fn build_slots(program: &ProgramArgs, vocab: &[HintEntry]) -> ProgramSlots {
    let flags: Vec<HintEntry> = program
        .flags
        .iter()
        .filter(|spec| Words::entry_is_word(&spec.name))
        .map(|spec| HintEntry {
            word: spec.name.clone(),
            tag: Tag::Flag,
            desc: spec.doc.clone(),
            glue: false,
        })
        .collect();
    let mut values: BTreeMap<String, Rc<Vec<HintEntry>>> = BTreeMap::new();
    for spec in &program.flags {
        let entries = value_entries(spec, vocab);
        if !entries.is_empty() {
            values.insert(spec.name.clone(), Rc::new(entries));
        }
    }
    ProgramSlots {
        flags: Rc::new(flags),
        values: Rc::new(values),
    }
}

/// The whole-line parser: feed it the line's bytes (via [`crate::inc::feed_bytes`])
/// and `Eof`; it finishes exactly on lines whose language is a superset of
/// `eosh_core::parse::parse_command`'s.
pub fn command_line(vocab: &Vocab) -> BoxP<()> {
    let filter = |excluded: &'static [&'static str]| -> Rc<Vec<HintEntry>> {
        Rc::new(
            vocab
                .entries
                .iter()
                .filter(|(word, _)| !excluded.contains(&word.as_str()))
                .map(|(word, tag)| HintEntry::plain(word.clone(), *tag))
                .collect(),
        )
    };
    let normal = filter(RESERVED);
    let programs: BTreeMap<String, ProgramSlots> = vocab
        .programs
        .iter()
        .filter(|(_, program)| !program.flags.is_empty())
        .map(|(name, program)| (name.clone(), build_slots(program, &normal)))
        .collect();
    let cards = card_entries();
    let man_vocab: Vec<HintEntry> = cards
        .iter()
        .cloned()
        .chain(
            normal
                .iter()
                .filter(|entry| entry.tag == Tag::Program)
                .cloned(),
        )
        .collect();
    let cx = Cx {
        head_vocab: filter(CMD_HEAD_EXCLUDED),
        vocab: normal,
        programs: Rc::new(programs),
        cards: Rc::new(cards),
        man_vocab: Rc::new(man_vocab),
    };
    then(command(&cx), trailing())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::sanity_check;
    use crate::inc::{accepts, feed_bytes, forced_prefix};
    use crate::input::Input;
    use alloc::format;
    use alloc::string::ToString;
    use eosh_core::parse::{ParseError, parse_command};
    use std::println;

    fn flag(name: &str, ty: &str) -> FlagSpec {
        FlagSpec {
            name: name.to_string(),
            ty: ty.to_string(),
            doc: None,
            values: Vec::new(),
            kind: None,
        }
    }

    fn fake_vocab() -> Vocab {
        let programs = [
            "hello",
            "time.frozen",
            "entropy.seeded",
            "cruncher",
            "browser",
            "virtualnet",
            "virtualfs",
            "memfs",
            "fetcher",
            "rng",
            "restart.never",
            "restart.backoff",
            "timeit",
            "fs.overlay",
        ];
        let bindings = ["det", "b", "t"];
        let mut entries: Vec<(String, Tag)> = programs
            .iter()
            .map(|p| (p.to_string(), Tag::Program))
            .collect();
        entries.extend(bindings.iter().map(|b| (b.to_string(), Tag::Binding)));
        // Hostile entries the constructor/filters must neutralize.
        entries.push(("only".to_string(), Tag::Program));
        entries.push(("help".to_string(), Tag::Program));
        entries.push(("--flag".to_string(), Tag::Program));
        entries.push(("[odd".to_string(), Tag::Program));
        let mut vocab = Vocab::new(entries);
        // Argument data for several corpus heads, so EVERY standing gate (the
        // differential superset, the fuzzed differential, the admissibility checker,
        // the looseness pin) also exercises the M3 argument layer — including
        // hostile pieces the build-time filters must neutralize.
        vocab.programs.insert(
            "hello".to_string(),
            ProgramArgs {
                flags: vec![
                    FlagSpec {
                        name: "name".to_string(),
                        ty: "string".to_string(),
                        doc: Some("who to greet".to_string()),
                        values: vec!["world".to_string()],
                        kind: None,
                    },
                    flag("verbose", "option<bool>"),
                ],
            },
        );
        vocab.programs.insert(
            "browser".to_string(),
            ProgramArgs {
                flags: vec![FlagSpec {
                    name: "url".to_string(),
                    ty: "string".to_string(),
                    doc: None,
                    values: Vec::new(),
                    kind: Some("url".to_string()),
                }],
            },
        );
        vocab.programs.insert(
            "cruncher".to_string(),
            ProgramArgs {
                flags: vec![
                    // Hostile: a flag named a reserved word (free flag names make it
                    // harmless), unlexable flag names (filtered at build), and a
                    // values: list full of words the value position must not start
                    // accepting (reserved, flag-shaped, compound-shaped, multi-word).
                    flag("as", "string"),
                    flag("--weird", "string"),
                    flag("two words", "string"),
                    flag("", "string"),
                    FlagSpec {
                        name: "rounds".to_string(),
                        ty: "u32".to_string(),
                        doc: Some("how many rounds".to_string()),
                        values: vec![
                            "only".to_string(),
                            "as".to_string(),
                            "--x".to_string(),
                            "[odd".to_string(),
                            "two words".to_string(),
                            String::new(),
                            "fast".to_string(),
                            "5".to_string(),
                        ],
                        kind: None,
                    },
                ],
            },
        );
        vocab.programs.insert(
            "fetcher".to_string(),
            ProgramArgs {
                flags: vec![FlagSpec {
                    name: "nic".to_string(),
                    ty: "option<string>".to_string(),
                    doc: None,
                    values: Vec::new(),
                    kind: Some("component-name".to_string()),
                }],
            },
        );
        // A name that is NOT in entries but carries args: harmless (the head bind
        // looks it up only when the captured word matches).
        vocab.programs.insert(
            "ghost".to_string(),
            ProgramArgs {
                flags: vec![flag("x", "bool")],
            },
        );
        vocab
    }

    fn inc_accepts(vocab: &Vocab, line: &str) -> bool {
        accepts(command_line(vocab), line)
    }

    /// The corpus: every command-line literal from eosh-core's lexer/parser/session
    /// tests and the builtin-card usage examples, plus edge lines from the grammar
    /// review. Positive or negative — the differential test asks eosh-core which.
    const CORPUS: &[&str] = &[
        // lex.rs tests
        "virtualfs --dir /tmp/sandbox $ browser --url https://example.com",
        "only eo9:time,eo9:fs$cruncher",
        "let det-env = (time.frozen & virtualnet)",
        r#"echo --text "a \"b\" \\ c\nd" "#,
        r#"fetch --url "https://example.com?a=b&c=d""#,
        "browser # composed, then run by the shell",
        "# a whole-line comment",
        "time.monotonic-stub eo9:fs/fs@0.1.0 virtualfs.create",
        "pci.admit-address --allow [{segment: 0, bus: 0, device: 1, function: 0}] $ lspci",
        "--pairs [[1, 2], [3, 4]] --opts {a: some(1), b: (5)}",
        r#"--names ["a]b", "c,{d", "e\"]f"]"#,
        r#"--allow "[{segment: 0, bus: 0}]""#,
        "only eo9:time,eo9:fs $ cruncher",
        "--allow [{segment: 0",
        r#"--names ["unclosed"#,
        r#"echo "unterminated"#,
        r#"echo "bad \q escape""#,
        "echo --",
        "",
        "   \t ",
        // parse.rs tests
        "(virtualfs --dir /tmp/sandbox) $ (browser --url https://example.com)",
        "virtualfs $ virtualnet $ browser",
        "virtualfs $ (virtualnet $ browser)",
        "(virtualnet $ virtualfs) $ browser",
        "time.monotonic-stub & virtualnet $ app",
        "x & y & z",
        "(x & y) & z",
        "posix-base & loopback-net --port 8080 $ app",
        "interpret (virtualnet $ browser)",
        "interpret virtualnet $ browser",
        r#"run --program (net.none $ browser) --label "my run" --retries 3"#,
        "only eo9:time,eo9:fs $ cruncher --input data.bin",
        "only sandbox.no-net $ only eo9:fs $ app",
        "only eo9:fs $ virtualnet $ browser",
        "realfs $ only eo9:fs $ app",
        "rename eo9:fs/fs scratch-fs $ tool",
        "with realfs as system-fs, memfs as scratch-fs $ backup-tool --src /home --dst /backups",
        "with (a, b) as (x, y) $ tool",
        "with a as x, b as y $ tool",
        "with (realnet & nat) as net, memfs & overlay as scratch $ app",
        "with (a, b, c) as (x, y) $ tool",
        "detach ticker = cruncher --rounds 50 restart restart.never",
        "detach worker = cruncher restart restart.backoff --max-restarts 5 --base-delay-ms 200",
        "detach worker = cruncher restart (restart.backoff --max-restarts 5 --base-delay-ms 200)",
        "detach greeter = time.frozen $ hello --name svc restart restart.always",
        "detach r = restart restart restart.never",
        "detach r = (restart --mode soft) restart restart.never",
        "svc",
        "svc list",
        "svc log ticker",
        "svc stop ticker",
        "svc clear ticker",
        "svc restart ticker",
        "only eo9:fs cruncher",
        "rename a b",
        "with memfs as scratch",
        "interpret (only eo9:fs $ cruncher)",
        "let det-env = time.monotonic-stub & virtualnet",
        "save mything = entropy.seeded $ rng",
        "save x rng",
        "let x memfs",
        "help",
        "env",
        "env readwrite",
        "env net.deny $ fetcher",
        "history",
        "exit",
        "quit",
        "poweroff",
        "describe net.none $ browser",
        "imports browser",
        "net.deny $ fetcher --url https://example.com",
        "describe eo9:pci",
        "describe eo9:pci/pci",
        "describe eo9:fs/fs@0.1.0",
        "describe (eo9:pci)",
        "describe eo9:pci $ hello",
        "describe describe",
        "describe hello",
        "interpret (virtualnet $ browser",
        "with",
        "echo --text as",
        r#"echo --text "as""#,
        "browser ) extra",
        "browser --url",
        "virtualfs.create",
        "fs.memfs $ time.frozen $ app",
        // session.rs tests
        "browser --url https://example.com",
        "det-env $ app",
        "detach .hidden = cruncher restart restart.never",
        "detach t = time.frozen restart restart.never",
        "detach t = timeit hello restart restart.never",
        "detach w = worker restart restart.never",
        "detach worker = cruncher --rounds 5 restart restart.never",
        "detach worker = cruncher --seed 1 --rounds 5 restart restart.never",
        "describe (help)",
        "describe eo9:fs",
        "describe eo9:fs/fs",
        "describe eo9:nope",
        "describe memfs",
        "env reader",
        "gpu.virtio  $  (draw)",
        "gpu.virtio $ draw",
        "hello --name a",
        "outcomes --mode fail",
        "outcomes --mode trap",
        "imports memfs",
        "let b = browser --url https://example.com",
        "let det = time.frozen & entropy.seeded",
        "let h = hello",
        "let t = time.frozen",
        "save ../escape = rng",
        "save mine = rng",
        "save x = y",
        "svc clear worker",
        "svc log ghost",
        "svc stop worker",
        "time.frozen $ a",
        "timeit hello",
        "# comment only",
        // builtin-card usage examples
        "let det = time.frozen & entropy.seeded --seed 7",
        "save frozen-hello = time.frozen --now-seconds 5 --monotonic-ns 0 $ hello",
        "detach worker = cruncher --rounds 100000 restart restart.never",
        "entropy.seeded --seed 7 $ rng --count 2",
        "time.frozen --now-seconds 0 --monotonic-ns 0 & entropy.seeded --seed 7",
        "only eo9:text,eo9:time $ hello",
        "rename eo9:fs/fs upper $ fs.overlay",
        "with fs.memfs as upper, fs.readonly as lower $ fs.overlay $ ls /",
        "describe entropy.seeded",
        "imports entropy.seeded $ rng",
        // man (the manuals builtin: exactly one token)
        "man telnetd",
        "man net.l4.over-l2",
        "man hello",
        "man describe",
        "man let",
        "man as",
        "man only",
        "man $",
        "man &",
        "man eo9:fs/fs",
        "man eo9:fs/fs@0.1.0",
        "man [a]",
        "man -x",
        "man hello # trailing comment",
        "man",
        "man a b",
        "man (hello)",
        "man hello --flag x",
        "man net.virtio $ l2check",
        "man \"quoted\"",
        "man --x",
        "manx",
        "man let = x",
        // edge lines from the grammar review
        "only(a)$x",
        "only[a] $ x",
        "letx=y",
        "lets go",
        "help x",
        "env x = y",
        "echo only",
        "echo ---x",
        "echo --- x",
        "echo -",
        "-x",
        "a--b",
        "describe only",
        "describe with",
        "describe let",
        "describe rename",
        "describe $",
        "describe &",
        "describe as",
        "describe $ x",
        "describe only extra",
        "describe",
        "[a] x",
        "[a][b]",
        "[]]",
        "a (b) c",
        "x $ env",
        "a&b$c",
        "a & only x $ y",
        "((((a))))",
        "()",
        "svc log only",
        "svc [x]",
        "svc log [a]",
        "svc log",
        "let [n] = x",
        "only [a] $ x",
        "with (a) as x $ t",
        "with (a) as (x) $ t",
        "with (a) & b as x $ t",
        "with a & b as x $ t",
        "with a $ b as x $ t",
        "with (a $ b, c) as (x, y) $ t",
        "detach n = x restart",
        "detach n = restart x",
        "imports",
        "let x = ",
        "a --url",
        "--url x",
        r#""quoted" command"#,
        "a $ ",
        "a & ",
        "a = b",
        "rename a b $ c",
        "only a , b $ c",
        "héllo --señor niño",
        "echo \"héllo\"",
        "x # trailing é comment",
        "browser#c",
        "let x#c",
        "a\tb",
        "a\u{b}b",
        "exit now",
        "history --all",
        "poweroff x",
        "env (x",
        "imports (a $ b)",
    ];

    /// THE SOUNDNESS GATE (the design's one invariant): everything parse_command
    /// accepts, the incremental grammar accepts — with the empty vocabulary AND a
    /// populated one (soundness must never depend on what is completable).
    #[test]
    fn differential_superset_on_corpus() {
        let empty = Vocab::default();
        let fake = fake_vocab();
        let mut failures = Vec::new();
        let mut positives = 0usize;
        for line in CORPUS {
            if parse_command(line).is_ok() {
                positives += 1;
                if !inc_accepts(&empty, line) {
                    failures.push(format!("FALSE RED (empty vocab): {line:?}"));
                }
                if !inc_accepts(&fake, line) {
                    failures.push(format!("FALSE RED (fake vocab): {line:?}"));
                }
            }
        }
        assert!(
            positives >= 100,
            "corpus shrank? only {positives} positive lines"
        );
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// The reverse direction is informational: where the incremental grammar is looser
    /// than eosh, the line must be on the documented-loose list — today exactly the
    /// `detach` restart clause (any flavor of its absence/garbling).
    #[test]
    fn looseness_is_exactly_the_documented_list() {
        let vocab = fake_vocab();
        let mut undocumented = Vec::new();
        let mut loose = 0usize;
        for line in CORPUS {
            if parse_command(line).is_err() && inc_accepts(&vocab, line) {
                loose += 1;
                if !line.trim_start().starts_with("detach") {
                    undocumented.push(*line);
                }
            }
        }
        println!("corpus looseness: {loose} lines (all detach)");
        assert!(
            undocumented.is_empty(),
            "undocumented looseness: {undocumented:?}"
        );
        // And the canonical loose line really is loose (so the doc stays honest).
        let no_restart = "detach ticker = cruncher --rounds 50";
        assert!(parse_command(no_restart).is_err());
        assert!(inc_accepts(&vocab, no_restart));
    }

    /// Exact agreement pinned on the corpus lines that must be RED: everything
    /// parse_command rejects except the documented-loose detach lines.
    #[test]
    fn tight_on_non_detach_negatives() {
        let vocab = fake_vocab();
        for line in CORPUS {
            if parse_command(line).is_err() && !line.trim_start().starts_with("detach") {
                assert!(
                    !inc_accepts(&vocab, line),
                    "must reject (eosh rejects): {line:?}"
                );
            }
        }
    }

    /// A deterministic xorshift64* generator — no Date/now, no rand dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Fuzzed differential: token-soup lines, raw byte lines, and corpus mutations.
    /// Same implication as the corpus test; reverse disagreements must be detach.
    #[test]
    fn differential_superset_fuzzed() {
        const TOKENS: &[&str] = &[
            "hello",
            "time.frozen",
            "eo9:fs/fs@0.1.0",
            "a",
            "b-c",
            "only",
            "rename",
            "with",
            "as",
            "let",
            "save",
            "detach",
            "svc",
            "restart",
            "restart.never",
            "env",
            "help",
            "describe",
            "man",
            "imports",
            "exit",
            "list",
            "log",
            "clear",
            "--flag",
            "--rounds",
            "5",
            "-",
            "--",
            "---x",
            "x",
            "é",
            "#c",
            r#""quoted $ & ( , = text""#,
            r#""bad \q""#,
            r#""unterminated"#,
            "[{a: 1, b: [2, \"s]s\"]}]",
            "[unclosed",
            "$",
            "&",
            "(",
            ")",
            ",",
            "=",
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let empty = Vocab::default();
        let fake = fake_vocab();
        let mut lines: Vec<String> = Vec::new();

        // Token soup.
        for _ in 0..5000 {
            let n = rng.below(8);
            let mut line = String::new();
            for i in 0..n {
                if i > 0 && rng.below(4) != 0 {
                    line.push(' ');
                }
                line.push_str(TOKENS[rng.below(TOKENS.len())]);
            }
            lines.push(line);
        }
        // Raw bytes (mostly printable, some controls and high bytes).
        for _ in 0..2000 {
            let n = rng.below(24);
            let mut bytes = Vec::new();
            for _ in 0..n {
                let b = match rng.below(10) {
                    0 => rng.below(256) as u8,
                    1 => b"$&(),=#\"[]{}\\-"[rng.below(14)],
                    _ => (0x20 + rng.below(0x5f)) as u8,
                };
                bytes.push(b);
            }
            lines.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        // Corpus mutations: replace, insert, or delete one byte.
        for _ in 0..5000 {
            let base = CORPUS[rng.below(CORPUS.len())];
            let mut bytes = base.as_bytes().to_vec();
            let mutation = rng.below(3);
            let printable = (0x20 + rng.below(0x5f)) as u8;
            match mutation {
                0 if !bytes.is_empty() => {
                    let i = rng.below(bytes.len());
                    bytes[i] = printable;
                }
                1 => {
                    let i = rng.below(bytes.len() + 1);
                    bytes.insert(i, printable);
                }
                _ if !bytes.is_empty() => {
                    bytes.remove(rng.below(bytes.len()));
                }
                _ => {}
            }
            lines.push(String::from_utf8_lossy(&bytes).into_owned());
        }

        let mut positives = 0usize;
        let mut loose = 0usize;
        let mut failures = Vec::new();
        for line in &lines {
            let core = parse_command(line);
            let inc = inc_accepts(&fake, line);
            match core {
                Ok(_) => {
                    positives += 1;
                    if !inc {
                        failures.push(format!("FALSE RED: {line:?}"));
                    }
                    if !inc_accepts(&empty, line) {
                        failures.push(format!("FALSE RED (empty vocab): {line:?}"));
                    }
                }
                Err(err) if inc => {
                    loose += 1;
                    if !line.trim_start().starts_with("detach") {
                        failures.push(format!("undocumented looseness: {line:?} (eosh: {err:?})"));
                    }
                }
                Err(_) => {}
            }
        }
        println!(
            "fuzz: {} lines, {positives} eosh-positive, {loose} loose (detach)",
            lines.len()
        );
        assert!(positives >= 300, "fuzz generator degenerated: {positives}");
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// THE MANUAL-FUZZING ARM (docs/design/component-manuals.md §3): adversarial
    /// argument data — lying manuals injected into the memo — must change ACCEPTANCE
    /// not at all. Stronger than "never shrinks": the added branches are built to be
    /// subsets of the free forms they alternate with, so admissibility is EQUAL with
    /// and without hints, on the corpus and on fuzzed lines. A lying manual can only
    /// mislead the candidate menu (false green at completion time), never the marker.
    #[test]
    fn adversarial_hints_never_change_acceptance() {
        let plain = {
            let mut vocab = fake_vocab();
            vocab.programs.clear();
            vocab
        };
        let mut lying = plain.clone();
        // Every vocabulary word (and a few corpus heads beyond it) gets hostile args:
        // reserved/flag/compound/multi-word/control-byte/non-ASCII flag names and
        // values, every kind tag including garbage.
        let hostile_values = [
            "as",
            "let",
            "only",
            "rename",
            "with",
            "--x",
            "-",
            "[{",
            "{",
            "a b c",
            "",
            " ",
            "\u{1b}[31m",
            "é",
            "\"q\"",
            "$",
            "5",
            "dhcp",
        ];
        let kinds = [
            None,
            Some("url"),
            Some("path"),
            Some("component-name"),
            Some("port"),
            Some("interface-name"),
            Some("banana"),
            Some(""),
        ];
        let heads: Vec<String> = plain
            .entries
            .iter()
            .map(|(word, _)| word.clone())
            .chain(
                ["echo", "app", "x", "a", "tool", "interpret"]
                    .iter()
                    .map(|s| s.to_string()),
            )
            .collect();
        for (index, name) in heads.iter().enumerate() {
            let mut flags: Vec<FlagSpec> = Vec::new();
            for (offset, hostile) in hostile_values.iter().enumerate() {
                flags.push(FlagSpec {
                    name: hostile.to_string(),
                    ty: "string".to_string(),
                    doc: Some(hostile.to_string()),
                    values: hostile_values.iter().map(|v| v.to_string()).collect(),
                    kind: kinds[(index + offset) % kinds.len()].map(String::from),
                });
            }
            // And ordinary-looking flags carrying the hostile values/kinds.
            for (offset, real) in ["url", "rounds", "text", "mode", "allow"]
                .iter()
                .enumerate()
            {
                flags.push(FlagSpec {
                    name: real.to_string(),
                    ty: ["bool", "option<bool>", "u32", "string", "list<u8>"][offset].to_string(),
                    doc: None,
                    values: hostile_values.iter().map(|v| v.to_string()).collect(),
                    kind: kinds[(index + offset + 3) % kinds.len()].map(String::from),
                });
            }
            lying.programs.insert(name.clone(), ProgramArgs { flags });
        }

        let mut checked = 0usize;
        let mut check = |line: &str| {
            let with = inc_accepts(&lying, line);
            let without = inc_accepts(&plain, line);
            assert_eq!(
                with, without,
                "hints changed acceptance for {line:?} (with hints: {with})"
            );
            checked += 1;
        };
        for line in CORPUS {
            check(line);
        }
        // Token soup biased toward flag/value shapes.
        const TOKENS: &[&str] = &[
            "hello",
            "browser",
            "cruncher",
            "echo",
            "--url",
            "--rounds",
            "--as",
            "--x",
            "as",
            "only",
            "dhcp",
            "5",
            "[{a: 1}]",
            "\"quoted\"",
            "$",
            "&",
            "(",
            ")",
            "=",
            "x",
            "--verbose",
            "true",
            "é",
        ];
        let mut rng = Rng(0x1357_9BDF_2468_ACE0);
        for _ in 0..3000 {
            let n = rng.below(8);
            let mut line = String::new();
            for i in 0..n {
                if i > 0 {
                    line.push(' ');
                }
                line.push_str(TOKENS[rng.below(TOKENS.len())]);
            }
            check(&line);
        }
        assert!(checked > 3000);
    }

    /// The M3 completion surfaces: flags from the provided signature (with the doc
    /// description and the Flag tag), typed/hinted value candidates (Value tag), the
    /// kind sources — and nothing for unknown names.
    #[test]
    fn argument_completions_surface_flags_and_typed_candidates() {
        let vocab = fake_vocab();
        let comps_at = |prefix: &str| {
            let state = feed_bytes(command_line(&vocab), prefix.as_bytes()).expect("viable");
            let mut out = Vec::new();
            state.completions(&mut out);
            out
        };

        // Flag names after `--`: from the signature, tagged Flag, doc as description;
        // unlexable hostile names are filtered.
        let flags = comps_at("cruncher --");
        let words: Vec<&str> = flags.iter().map(|c| c.word.as_str()).collect();
        assert!(words.contains(&"rounds"), "{words:?}");
        assert!(words.contains(&"as"), "{words:?}"); // free flag names: harmless
        assert!(!words.iter().any(|w| w.contains(' ')), "{words:?}");
        assert!(!words.contains(&"--weird"), "{words:?}");
        assert!(!words.contains(&""), "{words:?}");
        assert!(flags.iter().all(|c| c.tag == Tag::Flag));
        let rounds = flags.iter().find(|c| c.word == "rounds").expect("rounds");
        assert_eq!(rounds.desc.as_deref(), Some("how many rounds"));

        // A flag prefix narrows.
        let narrowed = comps_at("cruncher --ro");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].word, "rounds");
        assert_eq!(narrowed[0].matched, 2);

        // Value candidates: the manual's literals, filtered of everything the value
        // position could not lex as one plain non-reserved word — `only`/`as` are
        // RESERVED there (eosh rejects them as values), so offering them would widen
        // acceptance; they drop with the flag-shaped/compound/multi-word junk.
        let values = comps_at("cruncher --rounds ");
        let words: Vec<&str> = values.iter().map(|c| c.word.as_str()).collect();
        assert_eq!(words, vec!["fast", "5"]);
        assert!(values.iter().all(|c| c.tag == Tag::Value));

        // bool → true/false (under option<…> too).
        let bools = comps_at("hello --verbose ");
        let words: Vec<&str> = bools.iter().map(|c| c.word.as_str()).collect();
        assert_eq!(words, vec!["false", "true"]);

        // kind url → the canned glue prefix with its label.
        let url = comps_at("browser --url ");
        assert_eq!(url.len(), 1);
        assert_eq!(url[0].word, "http://");
        assert_eq!(url[0].desc.as_deref(), Some("url"));
        assert!(url[0].glue);
        assert_eq!(url[0].tag, Tag::Value);

        // kind component-name → the dynamic vocabulary, RETAGGED Value (the editor's
        // name-mark oracle must not see name evidence in a value position).
        let comp = comps_at("fetcher --nic ");
        let words: Vec<&str> = comp.iter().map(|c| c.word.as_str()).collect();
        assert!(words.contains(&"memfs"), "{words:?}");
        assert!(words.contains(&"det"), "{words:?}");
        assert!(comp.iter().all(|c| c.tag == Tag::Value), "{comp:?}");

        // Control bytes in hints never reach the menu (the printable backstop in
        // `Words::entry_is_word` — construction-side sanitization in eosh-core is the
        // first line of defense, this is the grammar-side one): an escape-opening or
        // BEL-carrying value is filtered; flag names likewise.
        let mut hostile = fake_vocab();
        hostile.programs.insert(
            "hello".to_string(),
            ProgramArgs {
                flags: vec![
                    FlagSpec {
                        name: "mode".to_string(),
                        ty: "string".to_string(),
                        doc: None,
                        values: vec![
                            "ok".to_string(),
                            "\u{1b}[31mred".to_string(),
                            "a\u{7}b".to_string(),
                        ],
                        kind: None,
                    },
                    flag("\u{1b}]0;title", "string"),
                ],
            },
        );
        let state = feed_bytes(command_line(&hostile), b"hello --mode ").expect("viable");
        let mut out = Vec::new();
        state.completions(&mut out);
        let words: Vec<&str> = out.iter().map(|c| c.word.as_str()).collect();
        assert_eq!(words, vec!["ok"], "control-byte hints leaked: {words:?}");
        let state = feed_bytes(command_line(&hostile), b"hello --").expect("viable");
        let mut out = Vec::new();
        state.completions(&mut out);
        assert!(
            out.iter()
                .all(|c| c.word.bytes().all(|b| (0x21..=0x7e).contains(&b))),
            "control bytes in the flag menu: {out:?}"
        );

        // Unknown head: the generic grammar — no flag or value candidates anywhere.
        assert!(comps_at("unknowntool --").is_empty());
        assert!(comps_at("rng --seed ").is_empty());

        // Argument data never leaks into completions before the `--` (positional
        // words offer nothing) or at the head.
        assert!(comps_at("cruncher ").iter().all(|c| c.tag != Tag::Flag));
    }

    /// Non-ASCII feed policy: lines eosh accepts with multi-byte words stay green.
    #[test]
    fn non_ascii_words_are_green() {
        let vocab = fake_vocab();
        for line in [
            "héllo",
            "héllo --señor niño",
            "let é = x",
            "echo \"héllo wörld\"",
            "x # café",
            "a --pairs [{k: \"vé\"}]",
        ] {
            assert!(parse_command(line).is_ok(), "corpus assumption: {line:?}");
            assert!(inc_accepts(&vocab, line), "false red: {line:?}");
        }
        // And the escape position rightly stays red (eosh: UnknownEscape).
        let line = "echo \"bad \\é\"";
        assert!(parse_command(line).is_err());
        assert!(!inc_accepts(&vocab, line));
    }

    /// The admissibility checker over reachable grammar states: every state visited
    /// while parsing the corpus satisfies the step/admissible contract; the residual
    /// of Bind's value-dependence approximation is ZERO for this grammar (its binds
    /// are value-independent — see comb.rs's Bind commentary).
    #[test]
    fn admissibility_agrees_on_reachable_states() {
        // Every state reached while feeding every viable corpus prefix: each is
        // checked over all 128 bytes + Eof. Non-ASCII bytes take the feed
        // substitution; a line is walked for as long as it stays viable (negatives
        // contribute their viable prefixes, which is exactly what the editor sees).
        let vocab = fake_vocab();
        let mut states = 0usize;
        let mut residual = 0usize;
        for line in CORPUS {
            let mut parser = command_line(&vocab);
            residual += sanity_check(&*parser);
            states += 1;
            'bytes: for &byte in line.as_bytes() {
                let input = match Input::byte(byte) {
                    Some(input) => input,
                    None if parser.admissible().non_ascii_ok => Input::byte(b'x').expect("ascii"),
                    None => break 'bytes,
                };
                parser = match parser.step(input) {
                    Some(crate::inc::Step::Continue(p))
                    | Some(crate::inc::Step::Both { cont: p, .. }) => p,
                    _ => break 'bytes,
                };
                residual += sanity_check(&*parser);
                states += 1;
            }
        }
        println!("admissibility: {states} states checked, residual {residual}");
        assert!(states > 4000);
        assert_eq!(
            residual, 0,
            "Bind approximation residual appeared; investigate before widening the bound"
        );
    }

    /// completions() over a fake vocabulary, at the states M2's TAB will ask.
    #[test]
    fn completions_surface_the_vocabulary() {
        let vocab = fake_vocab();
        let comps_at = |prefix: &str| {
            let state = feed_bytes(command_line(&vocab), prefix.as_bytes()).expect("viable");
            let mut out = Vec::new();
            state.completions(&mut out);
            out
        };
        let words_of = |comps: &[crate::inc::Completion]| -> Vec<String> {
            comps.iter().map(|c| c.word.clone()).collect()
        };

        // Line start: builtins, gate keywords, and the vocabulary all offer.
        let start = comps_at("");
        let start_words = words_of(&start);
        for expected in ["help", "describe", "svc", "only", "with", "time.frozen"] {
            assert!(start_words.contains(&expected.to_string()), "{expected}");
        }
        // The dispatch words are not offered twice: the vocab's hostile "help" entry
        // was filtered from head position (the Kw branch still offers it).
        assert_eq!(
            start_words.iter().filter(|w| w.as_str() == "help").count(),
            1
        );

        // Mid-word: only matching continuations, with the matched count.
        let ti = comps_at("ti");
        assert_eq!(
            words_of(&ti),
            vec!["time.frozen".to_string(), "timeit".to_string()]
        );
        assert!(ti.iter().all(|c| c.matched == 2 && c.tag == Tag::Program));

        // After `$`, name position again: vocabulary offers (including bindings).
        let after_compose = comps_at("time.frozen $ ");
        let after_words = words_of(&after_compose);
        assert!(after_words.contains(&"hello".to_string()));
        assert!(after_words.contains(&"det".to_string()));

        // `svc ` offers its subcommands.
        let svc = words_of(&comps_at("svc "));
        for sub in ["list", "log", "stop", "clear"] {
            assert!(svc.contains(&sub.to_string()), "{sub}");
        }

        // `describe ` offers the carded words alongside the vocabulary: reserved
        // cards, the builtins' own cards (the owner's `describe describe` report),
        // operator aliases, and the API card words.
        let desc = words_of(&comps_at("describe "));
        assert!(desc.contains(&"only".to_string()));
        assert!(desc.contains(&"browser".to_string()));
        assert!(desc.contains(&"describe".to_string()));
        assert!(desc.contains(&"help".to_string()));
        assert!(desc.contains(&"compose".to_string()));
        assert!(desc.contains(&"eo9:fs".to_string()));
        assert!(desc.contains(&"eo9:fs/fs".to_string()));

        // The owner's case: `describe d` keeps `describe`'s own card on offer, and
        // `describe descr` completes uniquely to it.
        let desc_d = words_of(&comps_at("describe d"));
        assert!(desc_d.contains(&"describe".to_string()), "{desc_d:?}");
        let descr = comps_at("describe descr");
        assert_eq!(descr.len(), 1);
        assert_eq!(descr[0].word, "describe");
        assert_eq!(descr[0].matched, 5);
        assert_eq!(descr[0].tag, Tag::Builtin);

        // `man ` offers the cards and the /bin programs — not the session bindings
        // (a binding has no manual).
        let man = words_of(&comps_at("man "));
        assert!(man.contains(&"describe".to_string()));
        assert!(man.contains(&"let".to_string()));
        assert!(man.contains(&"hello".to_string()));
        assert!(man.contains(&"eo9:fs/fs".to_string()));
        assert!(!man.contains(&"det".to_string()), "{man:?}");

        // `let ` binds a fresh name: nothing sensible to complete.
        assert!(comps_at("let ").is_empty());
    }

    /// The TAB walk on grammar states. Free-word positions never force (the grammar
    /// is word-open everywhere a name can appear, so single-byte admissible sets are
    /// rare); where only a keyword branch is alive, the rest of the keyword IS forced.
    #[test]
    fn forced_prefix_on_grammar_states() {
        let vocab = fake_vocab();
        let cases: &[(&str, &[u8])] = &[
            // Word-open positions: no forcing.
            ("", b""),
            ("des", b""), // "des…" might be a program name, not `describe`
            ("only eo9:fs ", b""),
            ("with (a, b) as (x", b""),
            // After `svc `, subcommands are the only word interpretation: `svc lo`
            // can only be heading to `svc log` — the `g` is forced (and the walk
            // stops at the boundary: the service name is free).
            ("svc lo", b"g"),
            ("svc cl", b"ear"),
        ];
        for (prefix, expected) in cases {
            let state = feed_bytes(command_line(&vocab), prefix.as_bytes()).expect("viable");
            let forced = forced_prefix(&*state);
            assert_eq!(&forced, expected, "forced prefix at {prefix:?}");
        }
    }

    /// Spot-pins for exact verdicts the differential implication alone would not pin
    /// (negatives whose rejection we promise, and the loose detach acceptance).
    #[test]
    fn verdict_spot_checks() {
        let vocab = fake_vocab();
        let red = [
            "only eo9:fs cruncher",
            "rename a b",
            "with memfs as scratch",
            "with (a, b, c) as (x, y) $ tool",
            "with (a) & b as x $ t",
            "svc restart ticker",
            "svc log",
            "browser --url",
            "echo --text as",
            "echo --",
            "help x",
            "exit now",
            "describe as",
            "describe $ x",
            "only(a)$x",
            "letx=y",
            r#"echo "bad \q escape""#,
            "--url x",
            "a $ ",
            "()",
            "browser ) extra",
            "man",
            "man a b",
            "man --x",
            "man (hello)",
            r#"man "quoted""#,
        ];
        for line in red {
            assert!(parse_command(line).is_err(), "corpus assumption: {line:?}");
            assert!(!inc_accepts(&vocab, line), "must be red: {line:?}");
        }
        let green = [
            "",
            "browser#c",
            "only[a] $ x",
            "echo --- x",
            "describe only",
            "describe $",
            "svc log [a]",
            "let [n] = x",
            "with (a) as x $ t",
            "x $ env",
            "[a][b]",
            "man let",
            "man as",
            "man $",
            "man [a]",
            "man eo9:fs/fs",
            "man -x",
            "manx",
        ];
        for line in green {
            assert!(parse_command(line).is_ok(), "corpus assumption: {line:?}");
            assert!(inc_accepts(&vocab, line), "must be green: {line:?}");
        }
        // The documented-loose detach forms (eosh red, incremental green).
        for line in [
            "detach ticker = cruncher --rounds 50",
            "detach n = x restart",
        ] {
            assert!(matches!(
                parse_command(line),
                Err(ParseError::DetachNeedsRestart) | Err(ParseError::UnexpectedEnd { .. })
            ));
            assert!(inc_accepts(&vocab, line), "documented loose: {line:?}");
        }
    }
}
