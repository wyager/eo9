//! The parser: tokens to [`Command`]s and [`Expr`]s, by hand-rolled recursive descent.
//!
//! Precedence, loosest to tightest (SPEC.md, "Environments and the `&` operator"):
//!
//! ```text
//! command      := "let" name "=" expr | builtin | expr
//! expr         := gate | amp-expr [ "$" expr ]                 ($ is right-associative)
//! gate         := "only" allow-list "$" expr
//!               | "rename" word word "$" expr
//!               | "with" with-bindings "$" expr
//! amp-expr     := app-expr { "&" app-expr }                    (& is left-associative)
//! app-expr     := primary { "--" name value | value }          (application binds tightest)
//! primary      := name | "(" expr ")"
//! value        := word | quoted-string | "(" expr ")"
//! allow-list   := word { "," word }
//! with-bindings:= with-item { "," with-item }
//! with-item    := amp-expr "as" word
//!               | "(" expr { "," expr } ")" "as" "(" word { "," word } ")"
//! ```
//!
//! Keyword-first forms (`let`, `only`, `rename`, `with`, `… as …`) are parsed from the
//! left, as the spec requires; `let`, `only`, `rename`, `with`, and `as` are reserved
//! words and must be quoted to be used as literal argument values. Builtin names
//! (`help`, `env`, `history`, `exit`, `quit`, `describe`, `imports`) are only special as
//! the first word of a command.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::ast::{Arg, ArgValue, Command, Expr, WithBinding};
use crate::lex::{Token, tokenize};

/// A lexing or parsing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A quoted string was not closed before the end of the line.
    UnterminatedString,
    /// An unknown escape sequence inside a quoted string.
    UnknownEscape(char),
    /// `--` with no flag name after it.
    EmptyFlagName,
    /// The line ended where something else was expected.
    UnexpectedEnd { expected: &'static str },
    /// A token appeared where something else was expected.
    UnexpectedToken {
        found: String,
        expected: &'static str,
    },
    /// A reserved word (`let`, `only`, `rename`, `with`, `as`) in name or value position.
    ReservedWord { word: String },
    /// Tokens were left over after a complete command.
    TrailingTokens { found: String },
    /// A gate term (`only`, `rename`, `with`) was not followed by `$`.
    GateNeedsDollar { gate: &'static str },
    /// The two sides of a `with (…) as (…)` tuple have different lengths.
    TupleArityMismatch { providers: usize, slots: usize },
    /// `detach` without its required `restart <policy>` clause.
    DetachNeedsRestart,
    /// `svc` with an unknown subcommand.
    UnknownSvcCommand { found: String },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnterminatedString => write!(f, "unterminated string literal"),
            ParseError::UnknownEscape(c) => write!(f, "unknown escape `\\{c}` in string literal"),
            ParseError::EmptyFlagName => write!(f, "`--` must be followed by a flag name"),
            ParseError::UnexpectedEnd { expected } => {
                write!(f, "unexpected end of line; expected {expected}")
            }
            ParseError::UnexpectedToken { found, expected } => {
                write!(f, "unexpected {found}; expected {expected}")
            }
            ParseError::ReservedWord { word } => write!(
                f,
                "`{word}` is a reserved word here; quote it to use it as a value"
            ),
            ParseError::TrailingTokens { found } => {
                write!(f, "unexpected {found} after a complete command")
            }
            ParseError::GateNeedsDollar { gate } => write!(
                f,
                "`{gate}` is a gate term and must be followed by `$` and the expression it applies to"
            ),
            ParseError::TupleArityMismatch { providers, slots } => write!(
                f,
                "`with` tuple arity mismatch: {providers} provider(s) but {slots} slot name(s)"
            ),
            ParseError::DetachNeedsRestart => write!(
                f,
                "`detach` needs a restart policy: `detach <name> = <program> restart <policy>` \
                 (e.g. `restart restart.never`; the standard policies are restart.never, \
                 restart.always, and restart.backoff)"
            ),
            ParseError::UnknownSvcCommand { found } => write!(
                f,
                "unknown `svc` subcommand `{found}`; expected `svc` (or `svc list`), \
                 `svc log <name>`, `svc stop <name>`, or `svc clear <name>`"
            ),
        }
    }
}

/// Words that may not appear as bare names or bare argument values.
fn is_reserved(word: &str) -> bool {
    matches!(word, "let" | "only" | "rename" | "with" | "as")
}

/// Parse one command line.
pub fn parse_command(line: &str) -> Result<Command, ParseError> {
    let tokens = tokenize(line)?;
    let mut parser = Parser::new(tokens);
    let command = parser.command()?;
    parser.expect_end()?;
    Ok(command)
}

/// Parse a program expression on its own (used by tests and by embedders).
pub fn parse_expr(src: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(src)?;
    let mut parser = Parser::new(tokens);
    let expr = parser.expr()?;
    parser.expect_end()?;
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn expect_end(&mut self) -> Result<(), ParseError> {
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(ParseError::TrailingTokens {
                found: token.describe(),
            }),
        }
    }

    /// Consume a plain (non-reserved) word.
    fn expect_word(&mut self, expected: &'static str) -> Result<String, ParseError> {
        match self.next() {
            Some(Token::Word(w)) if !is_reserved(&w) => Ok(w),
            Some(Token::Word(w)) => Err(ParseError::ReservedWord { word: w }),
            Some(other) => Err(ParseError::UnexpectedToken {
                found: other.describe(),
                expected,
            }),
            None => Err(ParseError::UnexpectedEnd { expected }),
        }
    }

    fn expect_token(&mut self, token: Token, expected: &'static str) -> Result<(), ParseError> {
        match self.next() {
            Some(t) if t == token => Ok(()),
            Some(other) => Err(ParseError::UnexpectedToken {
                found: other.describe(),
                expected,
            }),
            None => Err(ParseError::UnexpectedEnd { expected }),
        }
    }

    /// Consume the keyword `word` if it is next.
    fn eat_keyword(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(w)) if w == word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // -- commands ----------------------------------------------------------------

    fn command(&mut self) -> Result<Command, ParseError> {
        let Some(first) = self.peek() else {
            return Ok(Command::Empty);
        };

        if let Token::Word(word) = first {
            match word.as_str() {
                "let" => {
                    self.next();
                    let name = self.expect_word("a name to bind")?;
                    self.expect_token(Token::Equals, "`=` after the binding name")?;
                    let expr = self.expr()?;
                    return Ok(Command::Let { name, expr });
                }
                "save" => {
                    self.next();
                    let name = self.expect_word("a name to save under")?;
                    self.expect_token(Token::Equals, "`=` after the saved name")?;
                    let expr = self.expr()?;
                    return Ok(Command::Save { name, expr });
                }
                "detach" => {
                    self.next();
                    let name = self.expect_word("a name for the service")?;
                    self.expect_token(Token::Equals, "`=` after the service name")?;
                    return self.detach_body(name);
                }
                "svc" => {
                    self.next();
                    return self.svc_command();
                }
                "help" => return self.builtin_no_args(Command::Help),
                "env" => {
                    // Bare `env` is the session view; `env <expr>` is the capability
                    // view of one expression.
                    self.next();
                    if self.peek().is_none() {
                        return Ok(Command::Env);
                    }
                    return Ok(Command::EnvOf(self.expr()?));
                }
                "history" => return self.builtin_no_args(Command::History),
                "exit" | "quit" => return self.builtin_no_args(Command::Exit),
                "poweroff" => return self.builtin_no_args(Command::Poweroff),
                "describe" => {
                    self.next();
                    // A single shell word — a builtin or an operator — gets its own card
                    // instead of name resolution: `describe describe`, `describe $`,
                    // `describe only`. Anything longer (or any non-shell word) is an
                    // expression as before; parentheses force the expression path.
                    if self.tokens.len() - self.pos == 1 {
                        let word = match &self.tokens[self.pos] {
                            Token::Word(word) => Some(word.clone()),
                            Token::Dollar => Some(String::from("$")),
                            Token::Amp => Some(String::from("&")),
                            _ => None,
                        };
                        if let Some(word) = word
                            && crate::builtins::builtin_doc(&word).is_some()
                        {
                            self.next();
                            return Ok(Command::DescribeBuiltin(word));
                        }
                    }
                    return Ok(Command::Describe(self.expr()?));
                }
                "imports" => {
                    self.next();
                    return Ok(Command::Imports(self.expr()?));
                }
                _ => {}
            }
        }

        Ok(Command::Run(self.expr()?))
    }

    /// The body of a `detach` command after `detach <name> =`: a program expression,
    /// the keyword `restart`, and a restart-policy expression. The split happens at the
    /// *last* top-level (paren-depth-0) `restart` word, so a program that itself is
    /// named `restart` still parses (the policy clause is the last one on the line).
    fn detach_body(&mut self, name: String) -> Result<Command, ParseError> {
        let rest: Vec<Token> = self.tokens[self.pos..].to_vec();
        self.pos = self.tokens.len();

        let mut depth: usize = 0;
        let mut split: Option<usize> = None;
        for (index, token) in rest.iter().enumerate() {
            match token {
                Token::LParen => depth += 1,
                Token::RParen => depth = depth.saturating_sub(1),
                Token::Word(word) if depth == 0 && word == "restart" => split = Some(index),
                _ => {}
            }
        }
        let Some(split) = split else {
            return Err(ParseError::DetachNeedsRestart);
        };

        let mut program = Parser::new(rest[..split].to_vec());
        let expr = program.expr()?;
        program.expect_end()?;

        let mut policy_parser = Parser::new(rest[split + 1..].to_vec());
        let policy = policy_parser.expr()?;
        policy_parser.expect_end()?;

        Ok(Command::Detach { name, expr, policy })
    }

    /// `svc` subcommands: bare `svc` / `svc list` (the table), and `log`/`stop`/`clear`
    /// with a service name.
    fn svc_command(&mut self) -> Result<Command, ParseError> {
        let Some(token) = self.peek() else {
            return Ok(Command::SvcList);
        };
        let Token::Word(word) = token else {
            return Err(ParseError::UnexpectedToken {
                found: token.describe(),
                expected: "an `svc` subcommand (list, log, stop, clear)",
            });
        };
        let sub = word.clone();
        match sub.as_str() {
            "list" => {
                self.next();
                Ok(Command::SvcList)
            }
            "log" | "stop" | "clear" => {
                self.next();
                let name = self.expect_word("a service name")?;
                Ok(match sub.as_str() {
                    "log" => Command::SvcLog(name),
                    "stop" => Command::SvcStop(name),
                    _ => Command::SvcClear(name),
                })
            }
            _ => Err(ParseError::UnknownSvcCommand { found: sub }),
        }
    }

    fn builtin_no_args(&mut self, command: Command) -> Result<Command, ParseError> {
        self.next();
        Ok(command)
    }

    // -- expressions -------------------------------------------------------------

    fn expr(&mut self) -> Result<Expr, ParseError> {
        if let Some(Token::Word(word)) = self.peek() {
            match word.as_str() {
                "only" => return self.only_gate(),
                "rename" => return self.rename_gate(),
                "with" => return self.with_gate(),
                _ => {}
            }
        }

        let left = self.amp_expr()?;
        if matches!(self.peek(), Some(Token::Dollar)) {
            self.next();
            let right = self.expr()?;
            Ok(Expr::Compose {
                provider: Box::new(left),
                consumer: Box::new(right),
            })
        } else {
            Ok(left)
        }
    }

    fn gate_body(&mut self, gate: &'static str) -> Result<Expr, ParseError> {
        if !matches!(self.peek(), Some(Token::Dollar)) {
            return Err(ParseError::GateNeedsDollar { gate });
        }
        self.next();
        self.expr()
    }

    fn only_gate(&mut self) -> Result<Expr, ParseError> {
        self.next(); // `only`
        let mut allow = Vec::new();
        allow.push(self.expect_word("an interface or world name after `only`")?);
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            allow.push(self.expect_word("an interface or world name after `,`")?);
        }
        let body = self.gate_body("only")?;
        Ok(Expr::Only {
            allow,
            body: Box::new(body),
        })
    }

    fn rename_gate(&mut self) -> Result<Expr, ParseError> {
        self.next(); // `rename`
        let from = self.expect_word("the slot to rename")?;
        let to = self.expect_word("the new slot name")?;
        let body = self.gate_body("rename")?;
        Ok(Expr::Rename {
            from,
            to,
            body: Box::new(body),
        })
    }

    fn with_gate(&mut self) -> Result<Expr, ParseError> {
        self.next(); // `with`
        let mut bindings = Vec::new();
        self.with_item(&mut bindings)?;
        while matches!(self.peek(), Some(Token::Comma)) {
            self.next();
            self.with_item(&mut bindings)?;
        }
        let body = self.gate_body("with")?;
        Ok(Expr::With {
            bindings,
            body: Box::new(body),
        })
    }

    /// One `with` item: either `provider as slot` or the positional tuple form
    /// `(p1, p2, …) as (s1, s2, …)`, which expands to one binding per pair.
    fn with_item(&mut self, bindings: &mut Vec<WithBinding>) -> Result<(), ParseError> {
        if matches!(self.peek(), Some(Token::LParen)) {
            self.next();
            let first = self.expr()?;
            if matches!(self.peek(), Some(Token::Comma)) {
                // Tuple form.
                let mut providers = Vec::new();
                providers.push(first);
                while matches!(self.peek(), Some(Token::Comma)) {
                    self.next();
                    providers.push(self.expr()?);
                }
                self.expect_token(Token::RParen, "`)` closing the provider tuple")?;
                self.expect_keyword_as()?;
                self.expect_token(Token::LParen, "`(` opening the slot-name tuple")?;
                let mut slots = Vec::new();
                slots.push(self.expect_word("a slot name")?);
                while matches!(self.peek(), Some(Token::Comma)) {
                    self.next();
                    slots.push(self.expect_word("a slot name")?);
                }
                self.expect_token(Token::RParen, "`)` closing the slot-name tuple")?;
                if providers.len() != slots.len() {
                    return Err(ParseError::TupleArityMismatch {
                        providers: providers.len(),
                        slots: slots.len(),
                    });
                }
                for (provider, slot) in providers.into_iter().zip(slots) {
                    bindings.push(WithBinding { provider, slot });
                }
                return Ok(());
            }
            // A parenthesized provider expression.
            self.expect_token(Token::RParen, "`)` closing the provider expression")?;
            self.expect_keyword_as()?;
            let slot = self.expect_word("a slot name after `as`")?;
            bindings.push(WithBinding {
                provider: first,
                slot,
            });
            return Ok(());
        }

        let provider = self.amp_expr()?;
        self.expect_keyword_as()?;
        let slot = self.expect_word("a slot name after `as`")?;
        bindings.push(WithBinding { provider, slot });
        Ok(())
    }

    fn expect_keyword_as(&mut self) -> Result<(), ParseError> {
        if self.eat_keyword("as") {
            Ok(())
        } else {
            match self.peek() {
                Some(token) => Err(ParseError::UnexpectedToken {
                    found: token.describe(),
                    expected: "`as`",
                }),
                None => Err(ParseError::UnexpectedEnd { expected: "`as`" }),
            }
        }
    }

    fn amp_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.app_expr()?;
        while matches!(self.peek(), Some(Token::Amp)) {
            self.next();
            let right = self.app_expr()?;
            left = Expr::Extend {
                base: Box::new(left),
                layer: Box::new(right),
            };
        }
        Ok(left)
    }

    fn app_expr(&mut self) -> Result<Expr, ParseError> {
        let callee = self.primary()?;
        let mut args = Vec::new();
        loop {
            match self.peek() {
                Some(Token::Flag(_)) => {
                    let Some(Token::Flag(name)) = self.next() else {
                        unreachable!()
                    };
                    let value = self.arg_value("a value after the flag")?;
                    args.push(Arg::Flag { name, value });
                }
                Some(Token::Word(w)) if !is_reserved(w) => {
                    let Some(Token::Word(word)) = self.next() else {
                        unreachable!()
                    };
                    args.push(Arg::Positional(ArgValue::Word(word)));
                }
                Some(Token::Quoted(_)) => {
                    let Some(Token::Quoted(text)) = self.next() else {
                        unreachable!()
                    };
                    args.push(Arg::Positional(ArgValue::Quoted(text)));
                }
                Some(Token::LParen) => {
                    self.next();
                    let expr = self.expr()?;
                    self.expect_token(Token::RParen, "`)` closing the argument expression")?;
                    args.push(Arg::Positional(ArgValue::Expr(Box::new(expr))));
                }
                _ => break,
            }
        }
        if args.is_empty() {
            Ok(callee)
        } else {
            Ok(Expr::App {
                callee: Box::new(callee),
                args,
            })
        }
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        match self.next() {
            Some(Token::Word(w)) if !is_reserved(&w) => Ok(Expr::Name(w)),
            Some(Token::Word(w)) => Err(ParseError::ReservedWord { word: w }),
            Some(Token::LParen) => {
                let expr = self.expr()?;
                self.expect_token(Token::RParen, "`)` closing the group")?;
                Ok(expr)
            }
            Some(other) => Err(ParseError::UnexpectedToken {
                found: other.describe(),
                expected: "a program name or `(`",
            }),
            None => Err(ParseError::UnexpectedEnd {
                expected: "a program name or `(`",
            }),
        }
    }

    fn arg_value(&mut self, expected: &'static str) -> Result<ArgValue, ParseError> {
        match self.next() {
            Some(Token::Word(w)) if !is_reserved(&w) => Ok(ArgValue::Word(w)),
            Some(Token::Word(w)) => Err(ParseError::ReservedWord { word: w }),
            Some(Token::Quoted(s)) => Ok(ArgValue::Quoted(s)),
            Some(Token::LParen) => {
                let expr = self.expr()?;
                self.expect_token(Token::RParen, "`)` closing the argument expression")?;
                Ok(ArgValue::Expr(Box::new(expr)))
            }
            Some(other) => Err(ParseError::UnexpectedToken {
                found: other.describe(),
                expected,
            }),
            None => Err(ParseError::UnexpectedEnd { expected }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(
            parse_expr("with (a, b, c) as (x, y) $ tool"),
            Err(ParseError::TupleArityMismatch {
                providers: 3,
                slots: 2
            })
        );
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
        assert_eq!(
            parse_command("detach ticker = cruncher --rounds 50"),
            Err(ParseError::DetachNeedsRestart)
        );
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
        assert_eq!(
            parse_command("svc restart ticker"),
            Err(ParseError::UnknownSvcCommand {
                found: "restart".to_string()
            })
        );
    }

    #[test]
    fn gates_require_a_dollar() {
        assert_eq!(
            parse_expr("only eo9:fs cruncher"),
            Err(ParseError::GateNeedsDollar { gate: "only" })
        );
        assert_eq!(
            parse_expr("rename a b"),
            Err(ParseError::GateNeedsDollar { gate: "rename" })
        );
        assert_eq!(
            parse_expr("with memfs as scratch"),
            Err(ParseError::GateNeedsDollar { gate: "with" })
        );
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
        assert_eq!(
            parse_command("save x rng"),
            Err(ParseError::UnexpectedToken {
                found: "`rng`".to_string(),
                expected: "`=` after the saved name",
            })
        );
    }

    #[test]
    fn let_requires_an_equals_sign() {
        assert_eq!(
            parse_command("let x memfs"),
            Err(ParseError::UnexpectedToken {
                found: "`memfs`".to_string(),
                expected: "`=` after the binding name",
            })
        );
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

    // -- errors ---------------------------------------------------------------------

    #[test]
    fn unclosed_group_is_an_error() {
        assert_eq!(
            parse_expr("interpret (virtualnet $ browser"),
            Err(ParseError::UnexpectedEnd {
                expected: "`)` closing the argument expression"
            })
        );
    }

    #[test]
    fn reserved_words_cannot_be_names_or_bare_values() {
        assert_eq!(
            parse_expr("with"),
            Err(ParseError::UnexpectedEnd {
                expected: "a program name or `(`"
            })
        );
        assert_eq!(
            parse_expr("echo --text as"),
            Err(ParseError::ReservedWord {
                word: "as".to_string()
            })
        );
        // ... but a quoted reserved word is fine as a value.
        assert!(parse_expr(r#"echo --text "as""#).is_ok());
    }

    #[test]
    fn trailing_tokens_are_an_error() {
        assert_eq!(
            parse_command("browser ) extra"),
            Err(ParseError::TrailingTokens {
                found: "`)`".to_string()
            })
        );
    }

    #[test]
    fn missing_flag_value_is_an_error() {
        assert_eq!(
            parse_expr("browser --url"),
            Err(ParseError::UnexpectedEnd {
                expected: "a value after the flag"
            })
        );
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
