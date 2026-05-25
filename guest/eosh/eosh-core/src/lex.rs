//! The lexer: command lines to tokens.
//!
//! Tokens are whitespace-separated words plus a small set of structural characters.
//! The characters `$ & ( ) , =` are always structural — they terminate the word before
//! them and stand alone — so values that contain them (URLs with query strings, text
//! with ampersands) must be quoted. `"…"` is a quoted string with the escapes `\"`,
//! `\\`, `\n`, `\t`, and `\r`; `#` starts a comment that runs to the end of the line.
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
