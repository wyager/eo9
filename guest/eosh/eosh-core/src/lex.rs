//! The lexer: command lines to tokens.
//!
//! Tokens are whitespace-separated words plus a small set of structural characters.
//! The characters `$ & ( ) , =` are always structural — they terminate the word before
//! them and stand alone — so values that contain them (URLs with query strings, text
//! with ampersands) must be quoted. `"…"` is a quoted string with the escapes `\"`,
//! `\\`, `\n`, `\t`, and `\r`; `#` starts a comment that runs to the end of the line.
//! A token *beginning* with `[` or `{` is a compound literal (a WAVE list or record
//! value, e.g. `--allow [{segment: 0, bus: 0, device: 1, function: 0}]`): it runs,
//! verbatim, to the position where its brackets and braces balance — commas,
//! whitespace, and the structural characters inside it do not split it, and embedded
//! quoted strings are opaque (their brackets do not count). The lexer only balances;
//! whether the text is a well-formed value of the parameter's declared type is the
//! type-directed argument machinery's call, with its own typed error.
//! A word beginning with `--` is a flag name (`--url` → flag `url`). Everything else —
//! dotted names, interface references like `eo9:fs/fs@0.1.0`, bare literal values —
//! is a plain word; which of those it is gets decided by the parser and, for argument
//! values, by the callee's declared argument types (type-directed arguments).

use alloc::string::String;
use alloc::vec::Vec;

use crate::parse::ParseError;

/// One lexical token of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A bare word: a (possibly dotted) name, an interface reference, or a literal value.
    Word(String),
    /// A flag name: `--url` lexes as `Flag("url")`.
    Flag(String),
    /// A quoted string literal, with escapes already processed.
    Quoted(String),
    /// `$` — composition.
    Dollar,
    /// `&` — environment extension.
    Amp,
    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `,`.
    Comma,
    /// `=` (used by `let`).
    Equals,
}

/// Is `c` one of the always-structural characters?
fn is_structural(c: char) -> bool {
    matches!(c, '$' | '&' | '(' | ')' | ',' | '=')
}

/// Does `c` end a bare word?
fn ends_word(c: char) -> bool {
    c.is_whitespace() || is_structural(c) || c == '"' || c == '#'
}

/// Tokenize one command line.
pub fn tokenize(line: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            // Comment: the rest of the line is ignored.
            break;
        } else if is_structural(c) {
            chars.next();
            tokens.push(match c {
                '$' => Token::Dollar,
                '&' => Token::Amp,
                '(' => Token::LParen,
                ')' => Token::RParen,
                ',' => Token::Comma,
                '=' => Token::Equals,
                _ => unreachable!(),
            });
        } else if c == '"' {
            chars.next();
            tokens.push(Token::Quoted(lex_quoted(&mut chars)?));
        } else if c == '[' || c == '{' {
            tokens.push(Token::Word(lex_compound(&mut chars)?));
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if ends_word(c) {
                    break;
                }
                word.push(c);
                chars.next();
            }
            tokens.push(word_token(word)?);
        }
    }

    Ok(tokens)
}

/// Turn a completed bare word into its token (distinguishing flags from plain words).
fn word_token(word: String) -> Result<Token, ParseError> {
    if let Some(name) = word.strip_prefix("--") {
        if name.is_empty() {
            return Err(ParseError::EmptyFlagName);
        }
        Ok(Token::Flag(String::from(name)))
    } else {
        Ok(Token::Word(word))
    }
}

/// Lex a compound literal: `chars` is positioned at the opening `[` or `{`. The text is
/// taken verbatim — commas, whitespace, and structural characters included — until the
/// brackets and braces balance. Embedded quoted strings are opaque: their contents are
/// copied through (escape pairs skipped, so `\"` does not end the string) and never
/// counted toward the balance. Only balance is checked here; bracket *kind* mismatches
/// (`[}`) and everything else about well-formedness fall to the type-directed value
/// parser, which reports in terms of the parameter's declared type.
fn lex_compound(
    chars: &mut core::iter::Peekable<core::str::Chars<'_>>,
) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '[' | '{' => {
                depth += 1;
                out.push(c);
            }
            ']' | '}' => {
                depth = depth.saturating_sub(1);
                out.push(c);
                if depth == 0 {
                    return Ok(out);
                }
            }
            '"' => {
                out.push('"');
                loop {
                    match chars.next() {
                        None => return Err(ParseError::UnterminatedString),
                        Some('"') => {
                            out.push('"');
                            break;
                        }
                        Some('\\') => {
                            out.push('\\');
                            match chars.next() {
                                None => return Err(ParseError::UnterminatedString),
                                Some(escaped) => out.push(escaped),
                            }
                        }
                        Some(other) => out.push(other),
                    }
                }
            }
            other => out.push(other),
        }
    }
    Err(ParseError::UnterminatedCompound)
}

/// Lex the body of a quoted string; the opening `"` has already been consumed.
fn lex_quoted(
    chars: &mut core::iter::Peekable<core::str::Chars<'_>>,
) -> Result<String, ParseError> {
    let mut out = String::new();
    loop {
        match chars.next() {
            None => return Err(ParseError::UnterminatedString),
            Some('"') => return Ok(out),
            Some('\\') => match chars.next() {
                None => return Err(ParseError::UnterminatedString),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => return Err(ParseError::UnknownEscape(other)),
            },
            Some(other) => out.push(other),
        }
    }
}

impl Token {
    /// A short human-readable description, for error messages.
    pub fn describe(&self) -> String {
        match self {
            Token::Word(w) => alloc::format!("`{w}`"),
            Token::Flag(f) => alloc::format!("`--{f}`"),
            Token::Quoted(_) => String::from("a quoted string"),
            Token::Dollar => String::from("`$`"),
            Token::Amp => String::from("`&`"),
            Token::LParen => String::from("`(`"),
            Token::RParen => String::from("`)`"),
            Token::Comma => String::from("`,`"),
            Token::Equals => String::from("`=`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn word(s: &str) -> Token {
        Token::Word(s.to_string())
    }

    #[test]
    fn words_flags_and_structure() {
        let tokens = tokenize("virtualfs --dir /tmp/sandbox $ browser --url https://example.com")
            .expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("virtualfs"),
                Token::Flag("dir".to_string()),
                word("/tmp/sandbox"),
                Token::Dollar,
                word("browser"),
                Token::Flag("url".to_string()),
                word("https://example.com"),
            ]
        );
    }

    #[test]
    fn structural_characters_break_words_without_whitespace() {
        let tokens = tokenize("only eo9:time,eo9:fs$cruncher").expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("only"),
                word("eo9:time"),
                Token::Comma,
                word("eo9:fs"),
                Token::Dollar,
                word("cruncher"),
            ]
        );
    }

    #[test]
    fn parentheses_ampersand_and_equals() {
        let tokens = tokenize("let det-env = (time.frozen & virtualnet)").expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("let"),
                word("det-env"),
                Token::Equals,
                Token::LParen,
                word("time.frozen"),
                Token::Amp,
                word("virtualnet"),
                Token::RParen,
            ]
        );
    }

    #[test]
    fn quoted_strings_and_escapes() {
        let tokens = tokenize(r#"echo --text "a \"b\" \\ c\nd" "#).expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("echo"),
                Token::Flag("text".to_string()),
                Token::Quoted("a \"b\" \\ c\nd".to_string()),
            ]
        );
    }

    #[test]
    fn quoted_strings_keep_structural_characters() {
        let tokens = tokenize(r#"fetch --url "https://example.com?a=b&c=d""#).expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("fetch"),
                Token::Flag("url".to_string()),
                Token::Quoted("https://example.com?a=b&c=d".to_string()),
            ]
        );
    }

    #[test]
    fn comments_run_to_end_of_line() {
        let tokens = tokenize("browser # composed, then run by the shell").expect("lexes");
        assert_eq!(tokens, vec![word("browser")]);
        assert_eq!(tokenize("# a whole-line comment").expect("lexes"), vec![]);
    }

    #[test]
    fn dotted_names_and_interface_refs_are_single_words() {
        let tokens =
            tokenize("time.monotonic-stub eo9:fs/fs@0.1.0 virtualfs.create").expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("time.monotonic-stub"),
                word("eo9:fs/fs@0.1.0"),
                word("virtualfs.create"),
            ]
        );
    }

    #[test]
    fn compound_literals_lex_as_single_words() {
        // The pci.admit form, unquoted: commas and whitespace inside `[…]`/`{…}` do
        // not split the value (plan/03 D23's recorded tokenizer follow-up).
        let tokens = tokenize(
            "pci.admit-address --allow [{segment: 0, bus: 0, device: 1, function: 0}] $ lspci",
        )
        .expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("pci.admit-address"),
                Token::Flag("allow".to_string()),
                word("[{segment: 0, bus: 0, device: 1, function: 0}]"),
                Token::Dollar,
                word("lspci"),
            ]
        );
    }

    #[test]
    fn compound_literals_nest_and_keep_structural_characters() {
        let tokens =
            tokenize("--pairs [[1, 2], [3, 4]] --opts {a: some(1), b: (5)}").expect("lexes");
        assert_eq!(
            tokens,
            vec![
                Token::Flag("pairs".to_string()),
                word("[[1, 2], [3, 4]]"),
                Token::Flag("opts".to_string()),
                word("{a: some(1), b: (5)}"),
            ]
        );
    }

    #[test]
    fn strings_inside_compound_literals_are_opaque() {
        // Brackets, commas, and escaped quotes inside an embedded string neither
        // split the literal nor count toward the balance.
        let tokens = tokenize(r#"--names ["a]b", "c,{d", "e\"]f"]"#).expect("lexes");
        assert_eq!(
            tokens,
            vec![
                Token::Flag("names".to_string()),
                word(r#"["a]b", "c,{d", "e\"]f"]"#),
            ]
        );
    }

    #[test]
    fn quoted_compound_literals_still_lex_as_strings() {
        let tokens = tokenize(r#"--allow "[{segment: 0, bus: 0}]""#).expect("lexes");
        assert_eq!(
            tokens,
            vec![
                Token::Flag("allow".to_string()),
                Token::Quoted("[{segment: 0, bus: 0}]".to_string()),
            ]
        );
    }

    #[test]
    fn top_level_commas_stay_structural() {
        // The `only` list shorthand is unchanged: commas outside brackets still split.
        let tokens = tokenize("only eo9:time,eo9:fs $ cruncher").expect("lexes");
        assert_eq!(
            tokens,
            vec![
                word("only"),
                word("eo9:time"),
                Token::Comma,
                word("eo9:fs"),
                Token::Dollar,
                word("cruncher"),
            ]
        );
    }

    #[test]
    fn unterminated_compound_literals_are_errors() {
        assert_eq!(
            tokenize("--allow [{segment: 0"),
            Err(ParseError::UnterminatedCompound)
        );
        assert_eq!(
            tokenize(r#"--names ["unclosed"#),
            Err(ParseError::UnterminatedString)
        );
    }

    #[test]
    fn lex_errors() {
        assert_eq!(
            tokenize(r#"echo "unterminated"#),
            Err(ParseError::UnterminatedString)
        );
        assert_eq!(
            tokenize(r#"echo "bad \q escape""#),
            Err(ParseError::UnknownEscape('q'))
        );
        assert_eq!(tokenize("echo --"), Err(ParseError::EmptyFlagName));
    }

    #[test]
    fn empty_and_whitespace_lines_lex_to_nothing() {
        assert_eq!(tokenize("").expect("lexes"), vec![]);
        assert_eq!(tokenize("   \t ").expect("lexes"), vec![]);
    }
}
