//! The parser entry points: THE one grammar ([`crate::grammar`]), driven over a line.
//!
//! There is exactly one parser surface in the shell: the incremental grammar that the
//! per-keystroke editor steps for marking and completion is the same code that builds
//! the executed [`Command`] here. `parse_command` simply feeds a line's bytes through
//! [`crate::grammar::command_line`] and finishes it; on the editor path the
//! accumulated state IS the parse, and Enter hands the finished value over without a
//! second pass.
//!
//! Errors are positional: when a byte (or the end of the line) is not viable, the
//! error reports the 1-based display column and renders the admissible set of the
//! state reached — the same `admissible()`/`completions()` the editor consults for
//! the red marker and TAB. The curated per-construct error prose of the retired
//! recursive-descent parser is gone with it; what remains is uniformly honest:
//! "at column N: expected …, found `c`".

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::ast::{Command, Expr};
use crate::grammar::{Vocab, command_line, expr_line};
use crate::inc::{BoxP, Completion, IncParse, Step, feed_bytes};
use crate::input::Input;

/// A parse failure: where, what was found, and what the grammar admitted there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based display column (characters) of the offending character; one past the
    /// last column when the line ended too early.
    pub column: usize,
    /// The offending character; `None` when the failure is end-of-line.
    pub found: Option<char>,
    /// The rendered admissible set at the failure point.
    pub expected: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.found {
            Some(c) => write!(
                f,
                "at column {}: expected {}, found `{}`",
                self.column,
                self.expected,
                c.escape_debug()
            ),
            None => write!(f, "unexpected end of line: expected {}", self.expected),
        }
    }
}

/// Parse one command line.
pub fn parse_command(line: &str) -> Result<Command, ParseError> {
    let vocab = Vocab::default();
    run(command_line(&vocab), || command_line(&vocab), line)
}

/// Parse a program expression on its own (used by tests and by embedders).
pub fn parse_expr(src: &str) -> Result<Expr, ParseError> {
    let vocab = Vocab::default();
    run(expr_line(&vocab), || expr_line(&vocab), src)
}

/// Drive a parser over a whole line; `rebuild` re-creates it for the error path (the
/// failing character's column is known, so the expected-set is rendered from a fresh
/// parse of the viable prefix — the happy path never clones states).
fn run<T: 'static>(
    parser: BoxP<T>,
    rebuild: impl Fn() -> BoxP<T>,
    line: &str,
) -> Result<T, ParseError> {
    let mut state = parser;
    let mut column = 0usize;
    for (start, ch) in line.char_indices() {
        column += 1;
        let mut current = state;
        let mut ok = true;
        let mut buf = [0u8; 4];
        for &byte in ch.encode_utf8(&mut buf).as_bytes() {
            match current.step(Input::of_byte(byte)).and_then(Step::cont) {
                Some(next) => current = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            let expected = match feed_bytes(rebuild(), line[..start].as_bytes()) {
                Some(at) => render_expected(&*at),
                None => String::from("nothing (internal: prefix no longer viable)"),
            };
            return Err(ParseError {
                column,
                found: Some(ch),
                expected,
            });
        }
        state = current;
    }
    match state.step(Input::Eof).and_then(Step::value) {
        Some(value) => Ok(value),
        None => Err(ParseError {
            column: column + 1,
            found: None,
            expected: render_expected(&*state),
        }),
    }
}

/// Render a state's admissible set as plain words: the same `admissible()` and
/// `completions()` the editor consults, summarized — "a word", "a quoted string",
/// keyword candidates as "one of: …", structural bytes verbatim, "end of line" where
/// the state could finish.
fn render_expected<T: 'static>(state: &dyn IncParse<T>) -> String {
    let adm = state.admissible();
    let bytes: Vec<u8> = adm.charset.bytes().collect();
    let mut items: Vec<String> = Vec::new();

    if bytes.len() >= 128 {
        // Only the literal interiors admit every byte: an unterminated quoted string
        // or `[…]`/`{…}` compound.
        items.push(String::from(
            "more of the quoted string or `[…]`/`{…}` literal (it is not closed)",
        ));
    } else {
        let has = |b: u8| adm.charset.contains(b);
        // A free-word position admits (at least) all letters and digits.
        let has_word = has(b'a') && has(b'z') && has(b'0');
        let mut comps: Vec<Completion> = Vec::new();
        state.completions(&mut comps);
        if has_word {
            items.push(String::from("a word"));
        } else if !comps.is_empty() {
            // Keyword-only position: name the words themselves.
            let mut words: Vec<String> = comps.into_iter().map(|c| c.word).collect();
            words.sort();
            words.dedup();
            let shown = if words.len() > 8 {
                format!("one of: {}, …", words[..8].join(", "))
            } else {
                format!("one of: {}", words.join(", "))
            };
            items.push(shown);
        }
        if has(b'"') {
            items.push(String::from("a quoted string"));
        }
        for &(byte, label) in &[
            (b'$', "`$`"),
            (b'&', "`&`"),
            (b'(', "`(`"),
            (b')', "`)`"),
            (b',', "`,`"),
            (b'=', "`=`"),
        ] {
            if has(byte) {
                items.push(String::from(label));
            }
        }
        if items.is_empty() {
            // Small exact sets nothing above covered (e.g. the five escape bytes
            // inside a quoted string): list them verbatim, skipping whitespace and
            // the comment opener.
            for &b in &bytes {
                if b.is_ascii_whitespace() || b == b'#' {
                    continue;
                }
                items.push(format!("`{}`", char::from(b).escape_debug()));
            }
        }
    }
    if !adm.hard_required {
        items.push(String::from("end of line"));
    }
    if items.is_empty() {
        return String::from("nothing more");
    }
    join_or(&items)
}

/// `a`, `a or b`, `a, b, or c`.
fn join_or(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} or {}", items[0], items[1]),
        _ => {
            let head = items[..items.len() - 1].join(", ");
            format!("{}, or {}", head, items[items.len() - 1])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Arg, ArgValue, WithBinding};
    use alloc::boxed::Box;
    use alloc::string::ToString;
    use alloc::vec;

    fn name(s: &str) -> Expr {
        Expr::name(s)
    }

    fn compose(provider: Expr, consumer: Expr) -> Expr {
        Expr::Compose {
            provider: Box::new(provider),
            consumer: Box::new(consumer),
        }
    }

    fn extend(base: Expr, layer: Expr) -> Expr {
        Expr::Extend {
            base: Box::new(base),
            layer: Box::new(layer),
        }
    }

    fn app(callee: Expr, args: Vec<Arg>) -> Expr {
        Expr::App {
            callee: Box::new(callee),
            args,
        }
    }

    fn flag(n: &str, v: ArgValue) -> Arg {
        Arg::Flag {
            name: n.to_string(),
            value: v,
        }
    }

    fn word(v: &str) -> ArgValue {
        ArgValue::Word(v.to_string())
    }

    // -- compound literal argument values -----------------------------------------

    #[test]
    fn unquoted_compound_literals_are_single_argument_values() {
        // plan/03 D23's recorded follow-up: the pci.admit form needs no quotes.
        let parsed = parse_expr(
            "pci.admit-address --allow [{segment: 0, bus: 0, device: 1, function: 0}] $ lspci",
        )
        .expect("parses");
        assert_eq!(
            parsed,
            compose(
                app(
                    name("pci.admit-address"),
                    vec![flag(
                        "allow",
                        word("[{segment: 0, bus: 0, device: 1, function: 0}]"),
                    )],
                ),
                name("lspci"),
            )
        );
    }

    // -- precedence and associativity --------------------------------------------

    #[test]
    fn application_binds_tighter_than_compose() {
        // SPEC "Precedence": flags attach to their module before composition.
        let implicit =
            parse_expr("virtualfs --dir /tmp/sandbox $ browser --url https://example.com")
                .expect("parses");
        let explicit =
            parse_expr("(virtualfs --dir /tmp/sandbox) $ (browser --url https://example.com)")
                .expect("parses");
        assert_eq!(implicit, explicit);
        assert_eq!(
            implicit,
            compose(
                app(name("virtualfs"), vec![flag("dir", word("/tmp/sandbox"))]),
                app(
                    name("browser"),
                    vec![flag("url", word("https://example.com"))]
                ),
            )
        );
    }

    #[test]
    fn dollar_is_right_associative() {
        // SPEC "Composition and the `$` operator".
        let bare = parse_expr("virtualfs $ virtualnet $ browser").expect("parses");
        let explicit = parse_expr("virtualfs $ (virtualnet $ browser)").expect("parses");
        assert_eq!(bare, explicit);
        assert_eq!(
            bare,
            compose(
                name("virtualfs"),
                compose(name("virtualnet"), name("browser"))
            )
        );
    }

    #[test]
    fn reassociation_changes_the_tree() {
        // SPEC's re-association example: `(virtualnet $ virtualfs) $ browser` wires
        // virtualnet into virtualfs only — a different tree from the bare chain.
        let reassociated = parse_expr("(virtualnet $ virtualfs) $ browser").expect("parses");
        let bare = parse_expr("virtualnet $ virtualfs $ browser").expect("parses");
        assert_eq!(
            reassociated,
            compose(
                compose(name("virtualnet"), name("virtualfs")),
                name("browser")
            )
        );
        assert_ne!(reassociated, bare);
    }

    #[test]
    fn amp_binds_tighter_than_dollar() {
        // SPEC "Environments and the `&` operator": precedence is application > & > $.
        let expr = parse_expr("time.monotonic-stub & virtualnet $ app").expect("parses");
        assert_eq!(
            expr,
            compose(
                extend(name("time.monotonic-stub"), name("virtualnet")),
                name("app")
            )
        );
    }

    #[test]
    fn amp_chains_left_associatively() {
        let chain = parse_expr("x & y & z").expect("parses");
        let explicit = parse_expr("(x & y) & z").expect("parses");
        assert_eq!(chain, explicit);
        assert_eq!(chain, extend(extend(name("x"), name("y")), name("z")));
    }

    #[test]
    fn application_binds_tighter_than_amp() {
        let expr = parse_expr("posix-base & loopback-net --port 8080 $ app").expect("parses");
        assert_eq!(
            expr,
            compose(
                extend(
                    name("posix-base"),
                    app(name("loopback-net"), vec![flag("port", word("8080"))])
                ),
                name("app")
            )
        );
    }

    // -- grouping and argument position -------------------------------------------

    #[test]
    fn grouped_argument_stays_an_argument() {
        // SPEC "Grouping": `interpret (virtualnet $ browser)` passes the composition
        // open; without parentheses the same words parse as a composition whose
        // provider is `interpret virtualnet`.
        let grouped = parse_expr("interpret (virtualnet $ browser)").expect("parses");
        assert_eq!(
            grouped,
            app(
                name("interpret"),
                vec![Arg::Positional(ArgValue::Expr(Box::new(compose(
                    name("virtualnet"),
                    name("browser")
                ))))]
            )
        );

        let ungrouped = parse_expr("interpret virtualnet $ browser").expect("parses");
        assert_eq!(
            ungrouped,
            compose(
                app(name("interpret"), vec![Arg::Positional(word("virtualnet"))]),
                name("browser")
            )
        );
    }

    #[test]
    fn flag_values_can_be_words_strings_or_expressions() {
        let expr = parse_expr(r#"run --program (net.none $ browser) --label "my run" --retries 3"#)
            .expect("parses");
        assert_eq!(
            expr,
            app(
                name("run"),
                vec![
                    flag(
                        "program",
                        ArgValue::Expr(Box::new(compose(name("net.none"), name("browser"))))
                    ),
                    flag("label", ArgValue::Quoted("my run".to_string())),
                    flag("retries", word("3")),
                ]
            )
        );
    }

    // -- gate terms ----------------------------------------------------------------

    #[test]
    fn only_with_interface_list() {
        let expr = parse_expr("only eo9:time,eo9:fs $ cruncher --input data.bin").expect("parses");
        assert_eq!(
            expr,
            Expr::Only {
                allow: vec!["eo9:time".to_string(), "eo9:fs".to_string()],
                body: Box::new(app(name("cruncher"), vec![flag("input", word("data.bin"))])),
            }
        );
    }

    #[test]
    fn only_with_named_world_and_nesting() {
        let expr = parse_expr("only sandbox.no-net $ only eo9:fs $ app").expect("parses");
        assert_eq!(
            expr,
            Expr::Only {
                allow: vec!["sandbox.no-net".to_string()],
                body: Box::new(Expr::Only {
                    allow: vec!["eo9:fs".to_string()],
                    body: Box::new(name("app")),
                }),
            }
        );
    }

    #[test]
    fn only_gates_whole_composition_to_its_right() {
        // SPEC: `only eo9:fs $ virtualnet $ browser` — net satisfied inside the gate.
        let expr = parse_expr("only eo9:fs $ virtualnet $ browser").expect("parses");
        assert_eq!(
            expr,
            Expr::Only {
                allow: vec!["eo9:fs".to_string()],
                body: Box::new(compose(name("virtualnet"), name("browser"))),
            }
        );
    }

    #[test]
    fn providers_can_sit_left_of_a_gate() {
        let expr = parse_expr("realfs $ only eo9:fs $ app").expect("parses");
        assert_eq!(
            expr,
            compose(
                name("realfs"),
                Expr::Only {
                    allow: vec!["eo9:fs".to_string()],
                    body: Box::new(name("app")),
                }
            )
        );
    }

    #[test]
    fn rename_gate() {
        let expr = parse_expr("rename eo9:fs/fs scratch-fs $ tool").expect("parses");
        assert_eq!(
            expr,
            Expr::Rename {
                from: "eo9:fs/fs".to_string(),
                to: "scratch-fs".to_string(),
                body: Box::new(name("tool")),
            }
        );
    }

    #[test]
    fn with_comma_separated_bindings() {
        // The spec's backup-tool example.
        let expr = parse_expr(
            "with realfs as system-fs, memfs as scratch-fs $ backup-tool --src /home --dst /backups",
        )
        .expect("parses");
        assert_eq!(
            expr,
            Expr::With {
                bindings: vec![
                    WithBinding {
                        provider: name("realfs"),
                        slot: "system-fs".to_string(),
                    },
                    WithBinding {
                        provider: name("memfs"),
                        slot: "scratch-fs".to_string(),
                    },
                ],
                body: Box::new(app(
                    name("backup-tool"),
                    vec![flag("src", word("/home")), flag("dst", word("/backups"))]
                )),
            }
        );
    }

    #[test]
    fn with_tuple_form_expands_positionally() {
        // SPEC: `with (a, b) as (x, y)` means `a as x, b as y`.
        let tuple = parse_expr("with (a, b) as (x, y) $ tool").expect("parses");
        let spelled = parse_expr("with a as x, b as y $ tool").expect("parses");
        assert_eq!(tuple, spelled);
    }

    #[test]
    fn with_accepts_parenthesized_and_extended_providers() {
        let expr = parse_expr("with (realnet & nat) as net, memfs & overlay as scratch $ app")
            .expect("parses");
        assert_eq!(
            expr,
            Expr::With {
                bindings: vec![
                    WithBinding {
                        provider: extend(name("realnet"), name("nat")),
                        slot: "net".to_string(),
                    },
                    WithBinding {
                        provider: extend(name("memfs"), name("overlay")),
                        slot: "scratch".to_string(),
                    },
                ],
                body: Box::new(name("app")),
            }
        );
    }

    #[test]
    fn with_tuple_arity_mismatch_is_an_error() {
        // Arity is enforced grammatically now: the slot tuple admits exactly as many
        // names as providers, so the error lands at the `)` that closes too early.
        let err = parse_expr("with (a, b, c) as (x, y) $ tool").expect_err("arity");
        assert_eq!(err.found, Some(')'));
        assert!(err.expected.contains("`,`"), "{err}");
    }

    // -- detach and svc -----------------------------------------------------------

    #[test]
    fn detach_parses_program_arguments_and_policy() {
        let command = parse_command("detach ticker = cruncher --rounds 50 restart restart.never")
            .expect("parses");
        assert_eq!(
            command,
            Command::Detach {
                name: "ticker".to_string(),
                expr: app(name("cruncher"), vec![flag("rounds", word("50"))]),
                policy: name("restart.never"),
            }
        );
    }

    #[test]
    fn detach_policy_can_be_configured_and_parenthesized() {
        // The policy clause is itself a full expression: configure flags apply.
        let command = parse_command(
            "detach worker = cruncher restart restart.backoff --max-restarts 5 --base-delay-ms 200",
        )
        .expect("parses");
        assert_eq!(
            command,
            Command::Detach {
                name: "worker".to_string(),
                expr: name("cruncher"),
                policy: app(
                    name("restart.backoff"),
                    vec![
                        flag("max-restarts", word("5")),
                        flag("base-delay-ms", word("200"))
                    ]
                ),
            }
        );
        // Parenthesized policy: identical meaning.
        let parenthesized = parse_command(
            "detach worker = cruncher restart (restart.backoff --max-restarts 5 --base-delay-ms 200)",
        )
        .expect("parses");
        assert_eq!(command, parenthesized);
    }

    #[test]
    fn detach_program_can_be_a_composition() {
        let command =
            parse_command("detach greeter = time.frozen $ hello --name svc restart restart.always")
                .expect("parses");
        assert_eq!(
            command,
            Command::Detach {
                name: "greeter".to_string(),
                expr: compose(
                    name("time.frozen"),
                    app(name("hello"), vec![flag("name", word("svc"))])
                ),
                policy: name("restart.always"),
            }
        );
    }

    #[test]
    fn detach_without_a_restart_clause_is_an_error() {
        let err = parse_command("detach ticker = cruncher --rounds 50").expect_err("needs restart");
        assert!(err.found.is_none(), "{err}");
        // The admissible set names the one keyword that can rescue the line.
        assert!(err.expected.contains("a word"), "{err}");
    }

    #[test]
    fn detach_splits_at_the_last_top_level_restart_keyword() {
        // A program named `restart` (uncommon but legal) still parses: the *last*
        // top-level `restart` is the clause separator.
        let command = parse_command("detach r = restart restart restart.never").expect("parses");
        assert_eq!(
            command,
            Command::Detach {
                name: "r".to_string(),
                expr: name("restart"),
                policy: name("restart.never"),
            }
        );
        // A `restart` inside parentheses is not a separator.
        let command = parse_command("detach r = (restart --mode soft) restart restart.never")
            .expect("parses");
        assert_eq!(
            command,
            Command::Detach {
                name: "r".to_string(),
                expr: app(name("restart"), vec![flag("mode", word("soft"))]),
                policy: name("restart.never"),
            }
        );
    }

    #[test]
    fn svc_subcommands_parse() {
        assert_eq!(parse_command("svc"), Ok(Command::SvcList));
        assert_eq!(parse_command("svc list"), Ok(Command::SvcList));
        assert_eq!(
            parse_command("svc log ticker"),
            Ok(Command::SvcLog("ticker".to_string()))
        );
        assert_eq!(
            parse_command("svc stop ticker"),
            Ok(Command::SvcStop("ticker".to_string()))
        );
        assert_eq!(
            parse_command("svc clear ticker"),
            Ok(Command::SvcClear("ticker".to_string()))
        );
    }

    #[test]
    fn svc_unknown_subcommand_is_an_error() {
        let err = parse_command("svc restart ticker").expect_err("unknown subcommand");
        assert!(err.expected.contains("clear"), "{err}");
        assert!(err.expected.contains("log"), "{err}");
    }

    #[test]
    fn gates_require_a_dollar() {
        let err = parse_expr("only eo9:fs cruncher").expect_err("gate needs $");
        assert!(err.expected.contains("`$`"), "{err}");
        assert_eq!(err.found, Some('c'));
        let err = parse_expr("rename a b").expect_err("gate needs $");
        assert!(err.expected.contains("`$`"), "{err}");
        assert!(err.found.is_none(), "{err}");
        let err = parse_expr("with memfs as scratch").expect_err("gate needs $");
        assert!(err.expected.contains("`$`"), "{err}");
    }

    #[test]
    fn gates_can_appear_inside_argument_groups() {
        let expr = parse_expr("interpret (only eo9:fs $ cruncher)").expect("parses");
        assert_eq!(
            expr,
            app(
                name("interpret"),
                vec![Arg::Positional(ArgValue::Expr(Box::new(Expr::Only {
                    allow: vec!["eo9:fs".to_string()],
                    body: Box::new(name("cruncher")),
                })))]
            )
        );
    }

    // -- commands ------------------------------------------------------------------

    #[test]
    fn let_binds_an_environment_expression() {
        // The spec's example: `let det-env = time.monotonic-stub & virtualnet`.
        let command =
            parse_command("let det-env = time.monotonic-stub & virtualnet").expect("parses");
        assert_eq!(
            command,
            Command::Let {
                name: "det-env".to_string(),
                expr: extend(name("time.monotonic-stub"), name("virtualnet")),
            }
        );
    }

    #[test]
    fn save_persists_a_named_expression() {
        let command = parse_command("save mything = entropy.seeded $ rng").expect("parses");
        assert_eq!(
            command,
            Command::Save {
                name: "mything".to_string(),
                expr: compose(name("entropy.seeded"), name("rng")),
            }
        );
        // Same `<name> = <expr>` shape as `let`.
        let err = parse_command("save x rng").expect_err("save needs =");
        assert!(err.expected.contains("`=`"), "{err}");
        assert_eq!(err.found, Some('r'));
    }

    #[test]
    fn let_requires_an_equals_sign() {
        let err = parse_command("let x memfs").expect_err("let needs =");
        assert!(err.expected.contains("`=`"), "{err}");
        assert_eq!(err.found, Some('m'));
    }

    #[test]
    fn builtins_and_top_level_runs() {
        assert_eq!(parse_command("").expect("parses"), Command::Empty);
        assert_eq!(
            parse_command("  # just a comment").expect("parses"),
            Command::Empty
        );
        assert_eq!(parse_command("help").expect("parses"), Command::Help);
        assert_eq!(parse_command("env").expect("parses"), Command::Env);
        assert_eq!(
            parse_command("env readwrite").expect("parses"),
            Command::EnvOf(name("readwrite"))
        );
        assert_eq!(
            parse_command("env net.deny $ fetcher").expect("parses"),
            Command::EnvOf(compose(name("net.deny"), name("fetcher")))
        );
        assert_eq!(parse_command("history").expect("parses"), Command::History);
        assert_eq!(parse_command("exit").expect("parses"), Command::Exit);
        assert_eq!(parse_command("quit").expect("parses"), Command::Exit);
        assert_eq!(
            parse_command("poweroff").expect("parses"),
            Command::Poweroff
        );
        assert_eq!(
            parse_command("describe net.none $ browser").expect("parses"),
            Command::Describe(compose(name("net.none"), name("browser")))
        );
        assert_eq!(
            parse_command("imports browser").expect("parses"),
            Command::Imports(name("browser"))
        );
        assert_eq!(
            parse_command("net.deny $ fetcher --url https://example.com").expect("parses"),
            Command::Run(compose(
                name("net.deny"),
                app(
                    name("fetcher"),
                    vec![flag("url", word("https://example.com"))]
                )
            ))
        );
    }

    #[test]
    fn man_takes_exactly_one_bare_word() {
        assert_eq!(
            parse_command("man telnetd").expect("parses"),
            Command::Man("telnetd".to_string())
        );
        assert_eq!(
            parse_command("man net.l4.over-l2").expect("parses"),
            Command::Man("net.l4.over-l2".to_string())
        );
        // Shell words — reserved ones and operators included — are fine: they have
        // builtin cards, same as `describe`.
        assert_eq!(
            parse_command("man let").expect("parses"),
            Command::Man("let".to_string())
        );
        assert_eq!(
            parse_command("man $").expect("parses"),
            Command::Man("$".to_string())
        );
        // OS API names route through the same word (the session dispatches on the colon).
        assert_eq!(
            parse_command("man eo9:fs/fs").expect("parses"),
            Command::Man("eo9:fs/fs".to_string())
        );
    }

    #[test]
    fn man_refuses_expressions_and_emptiness() {
        // Expressions have no manual of their own; the message points at `describe`.
        assert!(parse_command("man net.virtio $ l2check").is_err());
        assert!(parse_command("man a b").is_err());
        assert!(parse_command("man (hello)").is_err());
        assert!(parse_command("man hello --flag x").is_err());
        let err = parse_command("man").expect_err("man needs a word");
        assert!(err.found.is_none(), "{err}");
        assert!(err.expected.contains("a word"), "{err}");
        // The positional error names where the one-token rule broke.
        let err = parse_command("man a b").expect_err("one token only");
        assert_eq!(err.column, 7, "{err}");
    }

    #[test]
    fn describe_of_a_colon_word_routes_to_the_api_cards() {
        assert_eq!(
            parse_command("describe eo9:pci").expect("parses"),
            Command::DescribeApi(String::from("eo9:pci"))
        );
        assert_eq!(
            parse_command("describe eo9:pci/pci").expect("parses"),
            Command::DescribeApi(String::from("eo9:pci/pci"))
        );
        assert_eq!(
            parse_command("describe eo9:fs/fs@0.1.0").expect("parses"),
            Command::DescribeApi(String::from("eo9:fs/fs@0.1.0"))
        );
        // Parentheses force the expression path, exactly as for builtins.
        assert!(matches!(
            parse_command("describe (eo9:pci)").expect("parses"),
            Command::Describe(_)
        ));
        // A colon word that is not the lone argument stays an expression.
        assert!(matches!(
            parse_command("describe eo9:pci $ hello").expect("parses"),
            Command::Describe(_)
        ));
        // Plain words are untouched: builtins and programs as before.
        assert!(matches!(
            parse_command("describe describe").expect("parses"),
            Command::DescribeBuiltin(_)
        ));
        assert!(matches!(
            parse_command("describe hello").expect("parses"),
            Command::Describe(_)
        ));
    }

    // -- errors ---------------------------------------------------------------------

    #[test]
    fn unclosed_group_is_an_error() {
        let err = parse_expr("interpret (virtualnet $ browser").expect_err("unclosed group");
        assert!(err.found.is_none(), "{err}");
        assert!(err.expected.contains("`)`"), "{err}");
    }

    #[test]
    fn reserved_words_cannot_be_names_or_bare_values() {
        let err = parse_expr("with").expect_err("with alone");
        assert!(err.found.is_none(), "{err}");
        assert!(parse_expr("echo --text as").is_err());
        // ... but a quoted reserved word is fine as a value.
        assert!(parse_expr(r#"echo --text "as""#).is_ok());
    }

    #[test]
    fn trailing_tokens_are_an_error() {
        let err = parse_command("browser ) extra").expect_err("trailing )");
        assert_eq!(err.found, Some(')'));
        assert!(err.expected.contains("end of line"), "{err}");
    }

    #[test]
    fn missing_flag_value_is_an_error() {
        let err = parse_expr("browser --url").expect_err("flag needs a value");
        assert!(err.found.is_none(), "{err}");
        assert!(err.expected.contains("a word"), "{err}");
    }

    #[test]
    fn dotted_names_parse_as_single_names() {
        assert_eq!(
            parse_expr("virtualfs.create").expect("parses"),
            name("virtualfs.create")
        );
        assert_eq!(
            parse_command("fs.memfs $ time.frozen $ app").expect("parses"),
            Command::Run(compose(
                name("fs.memfs"),
                compose(name("time.frozen"), name("app"))
            ))
        );
    }
}
