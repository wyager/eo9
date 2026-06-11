//! THE eosh grammar — the shell's single parser surface.
//!
//! One grammar serves every consumer: the per-keystroke editor steps it for the red
//! marker, TAB completion, and the forced-prefix walk; `parse_command` drives it over
//! a whole line; and the VALUE it constructs is the executed [`crate::ast::Command`]
//! itself — acceptance, completion, marking, and execution are views of this one
//! module. (The separate recursive-descent parser and lexer this grammar once
//! mirrored as a superset are deleted; their language and ASTs were pinned equal by
//! an exact differential before retirement, and live on as the corpus pins below.)
//!
//! ```text
//! line         := ws command? ws ( "#" anything )? EOF
//! command      := "let"  name "=" expr | "save" name "=" expr
//!               | "detach" name "=" expr "restart" expr     (split at the LAST
//!                                                            top-level `restart`)
//!               | "svc" ( ε | "list" | ("log"|"stop"|"clear") name )
//!               | "help" | "history" | "exit" | "quit" | "poweroff"
//!               | "env" ( ε | expr ) | "describe" ("$"|"&"|carded-word|expr)
//!               | "man" ( "$" | "&" | word | compound )      (one token, then EOF)
//!               | "imports" expr
//!               | expr                  (head name not a dispatch word)
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
//! name         := bare word (reserved excluded) | compound
//! ```
//!
//! Reserved words (`let only rename with as`) are excluded exactly (typing past one
//! resumes normal word life). Name positions also alternate a completion OVERLAY
//! ([`crate::comb::Overlay`]) — a vocabulary view that can change neither the language
//! nor the value, only what TAB offers and what the editor's name-mark sees.
//!
//! AMBIGUITY DISCIPLINE: the breadth-first `Alt`/`Bind` fork keeps every viable parse
//! alive, and where several COMPLETE parses coexist at end of line the FIRST finisher
//! in branch order wins. Two places lean on that deliberately:
//!
//! * `describe <word>`: the card-routing branch (builtin cards via
//!   [`crate::builtins::builtin_doc`], API cards via the colon rule) is ordered before
//!   the expression branch, so a lone carded word is its card (`describe describe`)
//!   while anything longer falls to the expression parse — the old lone-token rule.
//! * `detach`: the program expression may consume a top-level `restart` as an
//!   ordinary word OR stop before it; `Bind` keeps the still-consuming branch first,
//!   so the first finisher is the parse whose program ran longest — the split lands
//!   at the LAST top-level `restart`, exactly the old rule.
//!
//! TIGHTENINGS at unification (the one-shot differential proved both exact):
//!
//! * `detach` now REQUIRES its `restart <policy>` clause (the old grammar's one
//!   documented looseness — green-until-Enter then a parse error — is gone; the
//!   missing clause is an end-of-line error here too, and never a mid-line red).
//! * Non-ASCII bytes step as themselves ([`crate::input::Input::Text`]) instead of a
//!   representative substitute: word interiors, quoted strings, compound literals,
//!   and comments take them (exactly the retired lexer's rule), and captured values
//!   round-trip the line byte-for-byte.
//!
//! ARGUMENT COMPLETION (M3, docs/design/component-manuals.md §3–4): when the embedder
//! has provided a resolved program's argument data ([`Vocab::programs`]), the
//! application grammar captures the head name and binds the matching argument layer
//! in: flag-name positions additionally overlay the program's `--flag` names (from
//! the WIT `describe` signature, with the manual's per-arg doc line as the menu
//! description), and each flag's value position additionally overlays its typed
//! candidates — `union(wit_grammar(ty), words(hint_literals))`. THE HARD RULE (the
//! manuals design's §3) is now structural: hints are overlays, and an overlay NEVER
//! finishes, so it cannot change acceptance or the constructed value — a lying manual
//! can only mislead the candidate menu, never the parse. The adversarial-hints gate
//! pins it end to end.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::{Arg, ArgValue, Command, Expr, WithBinding};
use crate::comb::{
    HintEntry, Words, alt, bind, cap_bare_word, cap_compound, cap_flag_name, cap_quoted,
    comment_rest, fail, keep_left, keep_right, kw, lazy, lit, lit_byte, map, overlay_words, pure,
    rep_vec, star_vec, ws,
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
/// argument positions (the carded shell words from `crate::builtins::card_words`
/// plus the API card words from `crate::apidocs::api_words` — the SAME tables the
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

/// Sequence two UNIT parsers, keeping the first's laziness discipline: the second is
/// built eagerly (cheap) and cloned per completion of the first. Recursive references
/// must NOT go through this — use [`expr_l`].
fn then(first: BoxP<()>, second: BoxP<()>) -> BoxP<()> {
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

/// A plain word: a non-reserved bare word — or a compound literal, which is one word
/// token to the boundary rules (yes, `let [a] = x` parses; the captured text is the
/// verbatim token).
fn t_plain_word() -> BoxP<String> {
    keep_right(ws(), alt(vec![cap_bare_word(RESERVED), cap_compound()]))
}

/// A lazy reference to `expr` — every recursive edge of the grammar goes through here.
fn expr_l(cx: &Cx, pos: Pos) -> BoxP<Expr> {
    let cx = cx.clone();
    lazy(move || expr(&cx, pos))
}

/// `"(" expr ")"` (no leading-whitespace handling: callers sit behind a `ws`).
/// Grouping leaves no trace in the tree (ast.rs's rule) — the value is the inner
/// expression.
fn paren_expr(cx: &Cx) -> BoxP<Expr> {
    keep_left(
        keep_right(lit_byte(b'('), expr_l(cx, Pos::Normal)),
        t_byte(b')'),
    )
}

// -- expressions -------------------------------------------------------------------

/// A name in `primary` position: free word (position-appropriate exclusions),
/// compound, parenthesized expression — plus the vocabulary OVERLAY (completions
/// only; it never finishes, so it can affect neither the language nor the value).
/// The value is the primary's [`Expr`] plus the head word when the primary IS a word
/// (the M3 argument layer binds on it to look up that program's completion data).
fn cap_primary(cx: &Cx, pos: Pos) -> BoxP<(Option<String>, Expr)> {
    let (excluded, vocab) = match pos {
        Pos::Head => (CMD_HEAD_EXCLUDED, cx.head_vocab.clone()),
        Pos::Normal => (RESERVED, cx.vocab.clone()),
    };
    keep_right(
        ws(),
        alt(vec![
            map(cap_bare_word(excluded), |word| {
                let expr = Expr::Name(word.clone());
                (Some(word), expr)
            }),
            overlay_words(vocab),
            map(cap_compound(), |raw| (None, Expr::Name(raw))),
            map(paren_expr(cx), |expr| (None, expr)),
        ]),
    )
}

/// A flag-or-positional application argument (the generic v1 layer — no program data).
fn arg(cx: &Cx) -> BoxP<Arg> {
    let value_cx = cx.clone();
    keep_right(
        ws(),
        alt(vec![
            // `--name value`: the flag name is any nonempty word-byte run; the value
            // is mandatory.
            keep_right(
                lit(b"--"),
                bind(cap_flag_name(), move |name| {
                    map(value(&value_cx), move |value| Arg::Flag {
                        name: name.clone(),
                        value,
                    })
                }),
            ),
            map(cap_bare_word(RESERVED), |word| {
                Arg::Positional(ArgValue::Word(word))
            }),
            map(cap_compound(), |raw| Arg::Positional(ArgValue::Word(raw))),
            map(cap_quoted(), |text| Arg::Positional(ArgValue::Quoted(text))),
            map(paren_expr(cx), |expr| {
                Arg::Positional(ArgValue::Expr(Box::new(expr)))
            }),
        ]),
    )
}

/// A flag value: word, quoted string, compound, or parenthesized expression.
fn value(cx: &Cx) -> BoxP<ArgValue> {
    value_hinted(cx, None)
}

/// [`value`] plus a flag's candidate words, when it has any (an overlay: candidates
/// never change what is admissible — the additive-hints hard rule, now structural).
fn value_hinted(cx: &Cx, hints: Option<Rc<Vec<HintEntry>>>) -> BoxP<ArgValue> {
    let mut branches: Vec<BoxP<ArgValue>> = vec![
        map(cap_bare_word(RESERVED), ArgValue::Word),
        map(cap_compound(), ArgValue::Word),
        map(cap_quoted(), ArgValue::Quoted),
        map(paren_expr(cx), |expr| ArgValue::Expr(Box::new(expr))),
    ];
    if let Some(hints) = hints {
        branches.push(overlay_words(hints));
    }
    keep_right(ws(), alt(branches))
}

fn star_args(cx: &Cx) -> BoxP<Vec<Arg>> {
    let cx = cx.clone();
    star_vec(move || arg(&cx))
}

/// The argument list for a captured head name (M3): the program's slots when the
/// embedder provided them, the generic layer otherwise. Identical acceptance either
/// way — the slots are overlays.
fn star_args_for(cx: &Cx, name: &str) -> BoxP<Vec<Arg>> {
    match cx.programs.get(name) {
        Some(program) => {
            let cx = cx.clone();
            let program = program.clone();
            star_vec(move || arg_known(&cx, &program))
        }
        None => star_args(cx),
    }
}

/// [`arg`] with one program's slots alternated in: the flag token captures its name
/// (offering the program's flags alongside), and the bound value position offers that
/// flag's typed candidates alongside the free forms.
fn arg_known(cx: &Cx, program: &ProgramSlots) -> BoxP<Arg> {
    let flag: BoxP<String> = alt(vec![cap_flag_name(), overlay_words(program.flags.clone())]);
    let value_cx = cx.clone();
    let values = program.values.clone();
    keep_right(
        ws(),
        alt(vec![
            keep_right(
                lit(b"--"),
                bind(flag, move |name| {
                    let hinted = value_hinted(&value_cx, values.get(&name).cloned());
                    map(hinted, move |value| Arg::Flag {
                        name: name.clone(),
                        value,
                    })
                }),
            ),
            map(cap_bare_word(RESERVED), |word| {
                Arg::Positional(ArgValue::Word(word))
            }),
            map(cap_compound(), |raw| Arg::Positional(ArgValue::Word(raw))),
            map(cap_quoted(), |text| Arg::Positional(ArgValue::Quoted(text))),
            map(paren_expr(cx), |expr| {
                Arg::Positional(ArgValue::Expr(Box::new(expr)))
            }),
        ]),
    )
}

/// `app-expr := primary arg*`, from a supplied head (the `with`-item path needs a
/// non-paren head). An argument-less application is just its callee (ast.rs).
fn app_from(head: BoxP<(Option<String>, Expr)>, cx: &Cx) -> BoxP<Expr> {
    let cx2 = cx.clone();
    bind(head, move |(name, callee)| {
        let args_p = match &name {
            Some(n) => star_args_for(&cx2, n),
            None => star_args(&cx2),
        };
        let callee2 = callee.clone();
        map(args_p, move |args| {
            if args.is_empty() {
                callee2.clone()
            } else {
                Expr::App {
                    callee: Box::new(callee2.clone()),
                    args,
                }
            }
        })
    })
}

fn app(cx: &Cx, pos: Pos) -> BoxP<Expr> {
    app_from(cap_primary(cx, pos), cx)
}

/// The left-associative `&` fold: `acc & layer & …`.
fn amp_rest(cx: &Cx, acc: Expr) -> BoxP<Expr> {
    let cx2 = cx.clone();
    let acc2 = acc.clone();
    alt(vec![
        pure(acc),
        bind(
            keep_right(t_byte(b'&'), app(cx, Pos::Normal)),
            move |layer| {
                amp_rest(
                    &cx2,
                    Expr::Extend {
                        base: Box::new(acc2.clone()),
                        layer: Box::new(layer),
                    },
                )
            },
        ),
    ])
}

fn amp(cx: &Cx, pos: Pos) -> BoxP<Expr> {
    let cx2 = cx.clone();
    bind(app(cx, pos), move |first| amp_rest(&cx2, first))
}

fn expr(cx: &Cx, pos: Pos) -> BoxP<Expr> {
    let cx2 = cx.clone();
    alt(vec![
        gate_only(cx),
        gate_rename(cx),
        gate_with(cx),
        bind(amp(cx, pos), move |left| {
            // `$` is right-associative: recurse for the consumer.
            let provider = left.clone();
            let cx3 = cx2.clone();
            alt(vec![
                pure(left),
                map(
                    keep_right(t_byte(b'$'), expr_l(&cx3, Pos::Normal)),
                    move |consumer| Expr::Compose {
                        provider: Box::new(provider.clone()),
                        consumer: Box::new(consumer),
                    },
                ),
            ])
        }),
    ])
}

// -- gates ---------------------------------------------------------------------------

/// The `$ <expr>` every gate requires.
fn gate_body(cx: &Cx) -> BoxP<Expr> {
    keep_right(t_byte(b'$'), expr_l(cx, Pos::Normal))
}

fn gate_only(cx: &Cx) -> BoxP<Expr> {
    let cx2 = cx.clone();
    keep_right(
        t_kw("only", Tag::Keyword),
        bind(word_list(), move |allow| {
            let allow2 = allow.clone();
            map(gate_body(&cx2), move |body| Expr::Only {
                allow: allow2.clone(),
                body: Box::new(body),
            })
        }),
    )
}

/// `name ("," name)*` — the only-gate's allow-list.
fn word_list() -> BoxP<Vec<String>> {
    bind(t_plain_word(), |first| word_list_rest(vec![first]))
}

fn word_list_rest(acc: Vec<String>) -> BoxP<Vec<String>> {
    let acc2 = acc.clone();
    alt(vec![
        pure(acc),
        bind(keep_right(t_byte(b','), t_plain_word()), move |word| {
            let mut next = acc2.clone();
            next.push(word);
            word_list_rest(next)
        }),
    ])
}

fn gate_rename(cx: &Cx) -> BoxP<Expr> {
    let cx2 = cx.clone();
    keep_right(
        t_kw("rename", Tag::Keyword),
        bind(t_plain_word(), move |from| {
            let cx3 = cx2.clone();
            bind(t_plain_word(), move |to| {
                let from2 = from.clone();
                let to2 = to.clone();
                map(gate_body(&cx3), move |body| Expr::Rename {
                    from: from2.clone(),
                    to: to2.clone(),
                    body: Box::new(body),
                })
            })
        }),
    )
}

fn gate_with(cx: &Cx) -> BoxP<Expr> {
    let cx2 = cx.clone();
    keep_right(
        t_kw("with", Tag::Keyword),
        bind(with_items(cx), move |bindings| {
            let bindings2 = bindings.clone();
            map(gate_body(&cx2), move |body| Expr::With {
                bindings: bindings2.clone(),
                body: Box::new(body),
            })
        }),
    )
}

fn with_items(cx: &Cx) -> BoxP<Vec<WithBinding>> {
    let cx2 = cx.clone();
    bind(with_item(cx), move |first| with_items_rest(&cx2, first))
}

fn with_items_rest(cx: &Cx, acc: Vec<WithBinding>) -> BoxP<Vec<WithBinding>> {
    let cx2 = cx.clone();
    let acc2 = acc.clone();
    alt(vec![
        pure(acc),
        bind(keep_right(t_byte(b','), with_item(cx)), move |more| {
            let mut next = acc2.clone();
            next.extend(more);
            with_items_rest(&cx2, next)
        }),
    ])
}

/// One `with` binding (or several: the tuple form expands positionally, like the old
/// parser did). eosh's `with_item` COMMITS on a leading `(`: after `( expr )` only
/// `as <slot>` may follow (`with (a) & b as x` is an error), and the tuple form
/// `( e1, e2, … ) as ( s1, s2, … )` must match arities — the provider count is bound
/// into the slot grammar (the monadic bind earning its keep), and the matched pairs
/// zip into individual bindings.
fn with_item(cx: &Cx) -> BoxP<Vec<WithBinding>> {
    let tuple_cx = cx.clone();
    let head_cx = cx.clone();
    alt(vec![
        keep_right(
            t_byte(b'('),
            bind(expr_l(cx, Pos::Normal), move |first| {
                let single = first.clone();
                let tuple_first = first.clone();
                let cx3 = tuple_cx.clone();
                alt(vec![
                    // Single parenthesized provider: `) as slot`.
                    map(
                        keep_right(seq!(t_byte(b')'), t_kw("as", Tag::Keyword)), t_plain_word()),
                        move |slot| {
                            vec![WithBinding {
                                provider: single.clone(),
                                slot,
                            }]
                        },
                    ),
                    // Tuple: `, e2 [, …] ) as ( s1 [, …] )` with matching arity.
                    bind(more_exprs(&cx3), move |rest| {
                        let mut providers = vec![tuple_first.clone()];
                        providers.extend(rest);
                        let providers2 = providers.clone();
                        map(slot_tuple(providers.len()), move |slots| {
                            providers2
                                .iter()
                                .cloned()
                                .zip(slots)
                                .map(|(provider, slot)| WithBinding { provider, slot })
                                .collect()
                        })
                    }),
                ])
            }),
        ),
        // Non-paren-headed amp-expr: `provider as slot`.
        bind(provider_no_paren(&head_cx), move |provider| {
            let provider2 = provider.clone();
            map(
                keep_right(t_kw("as", Tag::Keyword), t_plain_word()),
                move |slot| {
                    vec![WithBinding {
                        provider: provider2.clone(),
                        slot,
                    }]
                },
            )
        }),
    ])
}

/// `("," expr)+ ")"`, collecting the extra providers (the first sits before this).
fn more_exprs(cx: &Cx) -> BoxP<Vec<Expr>> {
    more_exprs_acc(cx, Vec::new())
}

fn more_exprs_acc(cx: &Cx, acc: Vec<Expr>) -> BoxP<Vec<Expr>> {
    let cx2 = cx.clone();
    bind(
        keep_right(t_byte(b','), expr_l(cx, Pos::Normal)),
        move |extra| {
            let mut next = acc.clone();
            next.push(extra);
            let done = next.clone();
            let cx3 = cx2.clone();
            alt(vec![
                map(t_byte(b')'), move |()| done.clone()),
                lazy(move || more_exprs_acc(&cx3, next.clone())),
            ])
        },
    )
}

/// `"as" "(" slot ("," slot){n-1} ")"` — exactly `n` slot names.
fn slot_tuple(n: usize) -> BoxP<Vec<String>> {
    keep_left(
        keep_right(
            seq!(t_kw("as", Tag::Keyword), t_byte(b'(')),
            bind(t_plain_word(), move |first| {
                map(
                    rep_vec(n - 1, || keep_right(t_byte(b','), t_plain_word())),
                    move |rest| {
                        let mut slots = vec![first.clone()];
                        slots.extend(rest);
                        slots
                    },
                )
            }),
        ),
        t_byte(b')'),
    )
}

/// The non-paren `with`-item provider: a word/compound-headed app-expr, `&`-foldable.
fn provider_no_paren(cx: &Cx) -> BoxP<Expr> {
    let head = keep_right(
        ws(),
        alt(vec![
            map(cap_bare_word(RESERVED), |word| {
                let expr = Expr::Name(word.clone());
                (Some(word), expr)
            }),
            overlay_words(cx.vocab.clone()),
            map(cap_compound(), |raw| (None, Expr::Name(raw))),
        ]),
    );
    let cx2 = cx.clone();
    bind(app_from(head, cx), move |first| amp_rest(&cx2, first))
}

// -- commands --------------------------------------------------------------------------

/// `<keyword> <name> = <expr>` (let and save).
fn name_eq_expr(cx: &Cx, word: &'static str, build: fn(String, Expr) -> Command) -> BoxP<Command> {
    let cx2 = cx.clone();
    keep_right(
        t_kw(word, Tag::Builtin),
        bind(t_plain_word(), move |name| {
            let cx3 = cx2.clone();
            map(
                keep_right(t_byte(b'='), expr_l(&cx3, Pos::Normal)),
                move |expr| build(name.clone(), expr),
            )
        }),
    )
}

/// `detach <name> = <expr> restart <policy>`. The split sits at the LAST top-level
/// `restart` word, exactly like the old parser: the program expression is finishable
/// before every top-level `restart` AND can consume it as an ordinary word, so the
/// breadth-first fork carries every split — and `Bind` keeps the still-consuming
/// program branch FIRST in the merged alternation, so the first finisher at end of
/// line is the parse whose program ran longest, i.e. the last split.
fn detach(cx: &Cx) -> BoxP<Command> {
    let cx2 = cx.clone();
    keep_right(
        t_kw("detach", Tag::Builtin),
        bind(t_plain_word(), move |name| {
            let cx3 = cx2.clone();
            keep_right(
                t_byte(b'='),
                bind(expr_l(&cx3, Pos::Normal), move |program| {
                    let name2 = name.clone();
                    let program2 = program.clone();
                    let cx4 = cx3.clone();
                    map(
                        keep_right(t_kw("restart", Tag::Keyword), expr_l(&cx4, Pos::Normal)),
                        move |policy| Command::Detach {
                            name: name2.clone(),
                            expr: program2.clone(),
                            policy,
                        },
                    )
                }),
            )
        }),
    )
}

fn svc(cx: &Cx) -> BoxP<Command> {
    let _ = cx;
    keep_right(
        t_kw("svc", Tag::Builtin),
        alt(vec![
            pure(Command::SvcList),
            map(t_kw("list", Tag::Builtin), |()| Command::SvcList),
            bind(
                alt(vec![
                    map(t_kw("log", Tag::Builtin), |()| 0u8),
                    map(t_kw("stop", Tag::Builtin), |()| 1u8),
                    map(t_kw("clear", Tag::Builtin), |()| 2u8),
                ]),
                |which| {
                    map(t_plain_word(), move |name| match which {
                        0 => Command::SvcLog(name),
                        1 => Command::SvcStop(name),
                        _ => Command::SvcClear(name),
                    })
                },
            ),
        ]),
    )
}

/// `describe`'s argument: a lone `$`/`&` operator card; a single word that ROUTES on
/// the card tables (a carded shell word → its builtin card, a colon word → the API
/// cards — `builtins::builtin_doc` and the colon rule, the same dispatch the session
/// renders); otherwise an expression. The routing branch sits BEFORE the expression
/// branch: where both finish (`describe describe`), the card wins — eosh's
/// lone-token rule. A routed word must END the line (more tokens fall through to the
/// expression branch, which is still alive in parallel).
fn describe(cx: &Cx) -> BoxP<Command> {
    keep_right(
        t_kw("describe", Tag::Builtin),
        alt(vec![
            map(t_byte(b'$'), |()| {
                Command::DescribeBuiltin(String::from("$"))
            }),
            map(t_byte(b'&'), |()| {
                Command::DescribeBuiltin(String::from("&"))
            }),
            keep_right(
                ws(),
                alt(vec![
                    bind(cap_bare_word(&[]), |word| {
                        if crate::builtins::builtin_doc(&word).is_some() {
                            pure(Command::DescribeBuiltin(word))
                        } else if word.contains(':') {
                            pure(Command::DescribeApi(word))
                        } else {
                            fail()
                        }
                    }),
                    overlay_words(cx.cards.clone()),
                ]),
            ),
            map(expr_l(cx, Pos::Normal), Command::Describe),
        ]),
    )
}

/// `man <one token>`: exactly one bare word (reserved words included — `man let` is a
/// card), a compound (one `Token::Word` to the lexer), or a lone `$`/`&`; nothing may
/// follow but trailing space/comment.
fn man(cx: &Cx) -> BoxP<Command> {
    keep_right(
        t_kw("man", Tag::Builtin),
        keep_right(
            ws(),
            alt(vec![
                map(lit_byte(b'$'), |()| Command::Man(String::from("$"))),
                map(lit_byte(b'&'), |()| Command::Man(String::from("&"))),
                map(cap_bare_word(&[]), Command::Man),
                map(cap_compound(), Command::Man),
                overlay_words(cx.man_vocab.clone()),
            ]),
        ),
    )
}

fn command(cx: &Cx) -> BoxP<Command> {
    fn make_let(name: String, expr: Expr) -> Command {
        Command::Let { name, expr }
    }
    fn make_save(name: String, expr: Expr) -> Command {
        Command::Save { name, expr }
    }
    alt(vec![
        name_eq_expr(cx, "let", make_let),
        name_eq_expr(cx, "save", make_save),
        detach(cx),
        svc(cx),
        map(t_kw("help", Tag::Builtin), |()| Command::Help),
        map(t_kw("history", Tag::Builtin), |()| Command::History),
        map(t_kw("exit", Tag::Builtin), |()| Command::Exit),
        map(t_kw("quit", Tag::Builtin), |()| Command::Exit),
        map(t_kw("poweroff", Tag::Builtin), |()| Command::Poweroff),
        keep_right(
            t_kw("env", Tag::Builtin),
            alt(vec![
                pure(Command::Env),
                map(expr_l(cx, Pos::Normal), Command::EnvOf),
            ]),
        ),
        describe(cx),
        man(cx),
        map(
            keep_right(t_kw("imports", Tag::Builtin), expr_l(cx, Pos::Normal)),
            Command::Imports,
        ),
        map(expr(cx, Pos::Head), Command::Run),
        pure(Command::Empty),
    ])
}

/// The card vocabulary: every word with a builtin/operator card and every API card
/// word, tagged Builtin. Built once per [`command_line`] (the tables are static).
fn card_entries() -> Vec<HintEntry> {
    let mut entries: Vec<HintEntry> = crate::builtins::card_words()
        .map(|word| HintEntry::plain(String::from(word), Tag::Builtin))
        .collect();
    entries.extend(
        crate::apidocs::api_words()
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

/// Build the grammar's shared context from a vocabulary snapshot.
fn build_cx(vocab: &Vocab) -> Cx {
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
    Cx {
        head_vocab: filter(CMD_HEAD_EXCLUDED),
        vocab: normal,
        programs: Rc::new(programs),
        cards: Rc::new(cards),
        man_vocab: Rc::new(man_vocab),
    }
}

/// THE whole-line parser: feed it the line's bytes (via [`crate::inc::feed_bytes`])
/// and `Eof`; it finishes exactly on the lines eosh executes, with the executed
/// [`Command`] as its value — the editor's accumulated parse IS the parse.
pub fn command_line(vocab: &Vocab) -> BoxP<Command> {
    let cx = build_cx(vocab);
    keep_left(command(&cx), trailing())
}

/// A standalone expression line (the `parse_expr` entry): one [`Expr`], then trailing
/// space/comment. Head-position dispatch words do not apply — `help x` is an
/// application here, as it always was for `parse_expr`.
pub fn expr_line(vocab: &Vocab) -> BoxP<Expr> {
    let cx = build_cx(vocab);
    keep_left(expr(&cx, Pos::Normal), trailing())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::sanity_check;
    use crate::inc::{accepts, feed_bytes, forced_prefix};
    use crate::input::Input;
    use crate::parse::parse_command;
    use alloc::format;
    use alloc::string::ToString;
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

    fn inc_parse(vocab: &Vocab, line: &str) -> Option<crate::ast::Command> {
        let state = feed_bytes(command_line(vocab), line.as_bytes())?;
        crate::inc::finish(&*state)
    }

    /// The corpus: every command-line literal from the shell's lexer/parser/session
    /// tests and the builtin-card usage examples, plus edge lines from the grammar
    /// review — each pinned with its verdict (`true` = parses). The verdicts were
    /// snapshotted from the retired recursive-descent parser at the moment of the
    /// one-parser unification, after an exact-equality differential (acceptance AND
    /// AST) ran clean over this corpus and ~12k fuzzed lines.
    const CORPUS: &[(&str, bool)] = &[
        // lex.rs tests
        (
            "virtualfs --dir /tmp/sandbox $ browser --url https://example.com",
            true,
        ),
        ("only eo9:time,eo9:fs$cruncher", true),
        ("let det-env = (time.frozen & virtualnet)", true),
        (r#"echo --text "a \"b\" \\ c\nd" "#, true),
        (r#"fetch --url "https://example.com?a=b&c=d""#, true),
        ("browser # composed, then run by the shell", true),
        ("# a whole-line comment", true),
        ("time.monotonic-stub eo9:fs/fs@0.1.0 virtualfs.create", true),
        (
            "pci.admit-address --allow [{segment: 0, bus: 0, device: 1, function: 0}] $ lspci",
            true,
        ),
        (
            "--pairs [[1, 2], [3, 4]] --opts {a: some(1), b: (5)}",
            false,
        ),
        (r#"--names ["a]b", "c,{d", "e\"]f"]"#, false),
        (r#"--allow "[{segment: 0, bus: 0}]""#, false),
        ("only eo9:time,eo9:fs $ cruncher", true),
        ("--allow [{segment: 0", false),
        (r#"--names ["unclosed"#, false),
        (r#"echo "unterminated"#, false),
        (r#"echo "bad \q escape""#, false),
        ("echo --", false),
        ("", true),
        ("   \t ", true),
        // parse.rs tests
        (
            "(virtualfs --dir /tmp/sandbox) $ (browser --url https://example.com)",
            true,
        ),
        ("virtualfs $ virtualnet $ browser", true),
        ("virtualfs $ (virtualnet $ browser)", true),
        ("(virtualnet $ virtualfs) $ browser", true),
        ("time.monotonic-stub & virtualnet $ app", true),
        ("x & y & z", true),
        ("(x & y) & z", true),
        ("posix-base & loopback-net --port 8080 $ app", true),
        ("interpret (virtualnet $ browser)", true),
        ("interpret virtualnet $ browser", true),
        (
            r#"run --program (net.none $ browser) --label "my run" --retries 3"#,
            true,
        ),
        ("only eo9:time,eo9:fs $ cruncher --input data.bin", true),
        ("only sandbox.no-net $ only eo9:fs $ app", true),
        ("only eo9:fs $ virtualnet $ browser", true),
        ("realfs $ only eo9:fs $ app", true),
        ("rename eo9:fs/fs scratch-fs $ tool", true),
        (
            "with realfs as system-fs, memfs as scratch-fs $ backup-tool --src /home --dst /backups",
            true,
        ),
        ("with (a, b) as (x, y) $ tool", true),
        ("with a as x, b as y $ tool", true),
        (
            "with (realnet & nat) as net, memfs & overlay as scratch $ app",
            true,
        ),
        ("with (a, b, c) as (x, y) $ tool", false),
        (
            "detach ticker = cruncher --rounds 50 restart restart.never",
            true,
        ),
        (
            "detach worker = cruncher restart restart.backoff --max-restarts 5 --base-delay-ms 200",
            true,
        ),
        (
            "detach worker = cruncher restart (restart.backoff --max-restarts 5 --base-delay-ms 200)",
            true,
        ),
        (
            "detach greeter = time.frozen $ hello --name svc restart restart.always",
            true,
        ),
        ("detach r = restart restart restart.never", true),
        (
            "detach r = (restart --mode soft) restart restart.never",
            true,
        ),
        ("svc", true),
        ("svc list", true),
        ("svc log ticker", true),
        ("svc stop ticker", true),
        ("svc clear ticker", true),
        ("svc restart ticker", false),
        ("only eo9:fs cruncher", false),
        ("rename a b", false),
        ("with memfs as scratch", false),
        ("interpret (only eo9:fs $ cruncher)", true),
        ("let det-env = time.monotonic-stub & virtualnet", true),
        ("save mything = entropy.seeded $ rng", true),
        ("save x rng", false),
        ("let x memfs", false),
        ("help", true),
        ("env", true),
        ("env readwrite", true),
        ("env net.deny $ fetcher", true),
        ("history", true),
        ("exit", true),
        ("quit", true),
        ("poweroff", true),
        ("describe net.none $ browser", true),
        ("imports browser", true),
        ("net.deny $ fetcher --url https://example.com", true),
        ("describe eo9:pci", true),
        ("describe eo9:pci/pci", true),
        ("describe eo9:fs/fs@0.1.0", true),
        ("describe (eo9:pci)", true),
        ("describe eo9:pci $ hello", true),
        ("describe describe", true),
        ("describe hello", true),
        ("interpret (virtualnet $ browser", false),
        ("with", false),
        ("echo --text as", false),
        (r#"echo --text "as""#, true),
        ("browser ) extra", false),
        ("browser --url", false),
        ("virtualfs.create", true),
        ("fs.memfs $ time.frozen $ app", true),
        // session.rs tests
        ("browser --url https://example.com", true),
        ("det-env $ app", true),
        ("detach .hidden = cruncher restart restart.never", true),
        ("detach t = time.frozen restart restart.never", true),
        ("detach t = timeit hello restart restart.never", true),
        ("detach w = worker restart restart.never", true),
        (
            "detach worker = cruncher --rounds 5 restart restart.never",
            true,
        ),
        (
            "detach worker = cruncher --seed 1 --rounds 5 restart restart.never",
            true,
        ),
        ("describe (help)", true),
        ("describe eo9:fs", true),
        ("describe eo9:fs/fs", true),
        ("describe eo9:nope", true),
        ("describe memfs", true),
        ("env reader", true),
        ("gpu.virtio  $  (draw)", true),
        ("gpu.virtio $ draw", true),
        ("hello --name a", true),
        ("outcomes --mode fail", true),
        ("outcomes --mode trap", true),
        ("imports memfs", true),
        ("let b = browser --url https://example.com", true),
        ("let det = time.frozen & entropy.seeded", true),
        ("let h = hello", true),
        ("let t = time.frozen", true),
        ("save ../escape = rng", true),
        ("save mine = rng", true),
        ("save x = y", true),
        ("svc clear worker", true),
        ("svc log ghost", true),
        ("svc stop worker", true),
        ("time.frozen $ a", true),
        ("timeit hello", true),
        ("# comment only", true),
        // builtin-card usage examples
        ("let det = time.frozen & entropy.seeded --seed 7", true),
        (
            "save frozen-hello = time.frozen --now-seconds 5 --monotonic-ns 0 $ hello",
            true,
        ),
        (
            "detach worker = cruncher --rounds 100000 restart restart.never",
            true,
        ),
        ("entropy.seeded --seed 7 $ rng --count 2", true),
        (
            "time.frozen --now-seconds 0 --monotonic-ns 0 & entropy.seeded --seed 7",
            true,
        ),
        ("only eo9:text,eo9:time $ hello", true),
        ("rename eo9:fs/fs upper $ fs.overlay", true),
        (
            "with fs.memfs as upper, fs.readonly as lower $ fs.overlay $ ls /",
            true,
        ),
        ("describe entropy.seeded", true),
        ("imports entropy.seeded $ rng", true),
        // man (the manuals builtin: exactly one token)
        ("man telnetd", true),
        ("man net.l4.over-l2", true),
        ("man hello", true),
        ("man describe", true),
        ("man let", true),
        ("man as", true),
        ("man only", true),
        ("man $", true),
        ("man &", true),
        ("man eo9:fs/fs", true),
        ("man eo9:fs/fs@0.1.0", true),
        ("man [a]", true),
        ("man -x", true),
        ("man hello # trailing comment", true),
        ("man", false),
        ("man a b", false),
        ("man (hello)", false),
        ("man hello --flag x", false),
        ("man net.virtio $ l2check", false),
        ("man \"quoted\"", false),
        ("man --x", false),
        ("manx", true),
        ("man let = x", false),
        // edge lines from the grammar review
        ("only(a)$x", false),
        ("only[a] $ x", true),
        ("letx=y", false),
        ("lets go", true),
        ("help x", false),
        ("env x = y", false),
        ("echo only", false),
        ("echo ---x", false),
        ("echo --- x", true),
        ("echo -", true),
        ("-x", true),
        ("a--b", true),
        ("describe only", true),
        ("describe with", true),
        ("describe let", true),
        ("describe rename", true),
        ("describe $", true),
        ("describe &", true),
        ("describe as", false),
        ("describe $ x", false),
        ("describe only extra", false),
        ("describe", false),
        ("[a] x", true),
        ("[a][b]", true),
        ("[]]", true),
        ("a (b) c", true),
        ("x $ env", true),
        ("a&b$c", true),
        ("a & only x $ y", false),
        ("((((a))))", true),
        ("()", false),
        ("svc log only", false),
        ("svc [x]", false),
        ("svc log [a]", true),
        ("svc log", false),
        ("let [n] = x", true),
        ("only [a] $ x", true),
        ("with (a) as x $ t", true),
        ("with (a) as (x) $ t", false),
        ("with (a) & b as x $ t", false),
        ("with a & b as x $ t", true),
        ("with a $ b as x $ t", false),
        ("with (a $ b, c) as (x, y) $ t", true),
        ("detach n = x restart", false),
        ("detach n = restart x", false),
        ("imports", false),
        ("let x = ", false),
        ("a --url", false),
        ("--url x", false),
        (r#""quoted" command"#, false),
        ("a $ ", false),
        ("a & ", false),
        ("a = b", false),
        ("rename a b $ c", true),
        ("only a , b $ c", true),
        ("héllo --señor niño", true),
        ("echo \"héllo\"", true),
        ("x # trailing é comment", true),
        ("browser#c", true),
        ("let x#c", false),
        ("a\tb", true),
        ("a\u{b}b", true),
        ("exit now", false),
        ("history --all", false),
        ("poweroff x", false),
        ("env (x", false),
        ("imports (a $ b)", true),
    ];

    /// THE ONE-PARSER GATE, as direct pins: every corpus line's accept/reject verdict
    /// holds — with the empty vocabulary AND a populated one (the language must never
    /// depend on what is completable) — and on green lines `parse_command` (the
    /// driver) and the grammar agree on the constructed Command regardless of
    /// vocabulary (hints can never change the value).
    #[test]
    fn corpus_verdicts_are_pinned() {
        let empty = Vocab::default();
        let fake = fake_vocab();
        let mut failures = Vec::new();
        let mut positives = 0usize;
        for &(line, ok) in CORPUS {
            if inc_accepts(&fake, line) != ok {
                failures.push(format!("verdict flip (fake vocab, want {ok}): {line:?}"));
                continue;
            }
            if inc_accepts(&empty, line) != ok {
                failures.push(format!("verdict flip (empty vocab, want {ok}): {line:?}"));
                continue;
            }
            if parse_command(line).is_ok() != ok {
                failures.push(format!("driver verdict flip (want {ok}): {line:?}"));
                continue;
            }
            if ok {
                positives += 1;
                let driver = parse_command(line).expect("checked");
                match inc_parse(&fake, line) {
                    Some(got) if got == driver => {}
                    other => failures.push(format!(
                        "value disagreement: {line:?}\n  driver: {driver:?}\n  grammar(fake vocab): {other:?}"
                    )),
                }
            }
        }
        assert!(
            positives >= 100,
            "corpus shrank? only {positives} positive lines"
        );
        assert!(failures.is_empty(), "{}", failures.join("\n"));
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

    /// Fuzzed self-differential: token-soup lines, raw byte lines, and corpus
    /// mutations — the driver (`parse_command`) and the raw grammar must agree on
    /// acceptance and value, with the empty and the populated vocabulary, on ~12k
    /// generated lines (also a no-panic/no-blowup sweep over hostile input).
    #[test]
    fn fuzzed_driver_grammar_agreement() {
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
            let base = CORPUS[rng.below(CORPUS.len())].0;
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
        let mut failures = Vec::new();
        for line in &lines {
            let driver = parse_command(line).ok();
            let grammar_fake = inc_parse(&fake, line);
            let grammar_empty = inc_parse(&empty, line);
            if driver != grammar_fake || driver != grammar_empty {
                failures.push(format!(
                    "disagreement: {line:?}\n  driver: {driver:?}\n  grammar(fake): {grammar_fake:?}\n  grammar(empty): {grammar_empty:?}"
                ));
            }
            if driver.is_some() {
                positives += 1;
            }
        }
        println!("fuzz: {} lines, {positives} positive", lines.len());
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
        for (line, _) in CORPUS {
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
        for (line, _) in CORPUS {
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
        // The once-loose detach forms: with the single parser the restart clause is
        // required for real — these are red here exactly as they failed in eosh.
        for line in [
            "detach ticker = cruncher --rounds 50",
            "detach n = x restart",
            "detach n = (a restart b)",
        ] {
            assert!(parse_command(line).is_err(), "corpus assumption: {line:?}");
            assert!(!inc_accepts(&vocab, line), "must be red now: {line:?}");
        }
    }
}
