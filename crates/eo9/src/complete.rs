//! Candidate generation for tab completion in the interactive shell (`eo9 shell`).
//!
//! The line editor (`editor.rs`) asks this module what the word under the cursor could
//! become. Candidate sources:
//!
//! * eosh builtins and reserved words — the lists here mirror eosh-core's lexer/parser
//!   (plan/10-eosh.md Decision 3) and must be kept in sync with them;
//! * program names resolvable in the session: exactly the names the shell start placed
//!   into the session bin view, i.e. what eosh's `resolve` will find;
//! * the standard `eo9:*` interface references (for `only` allow-lists), when the word
//!   being completed contains a `:`;
//! * paths on the filesystem the shell's children are actually granted (`--fs-root`),
//!   when the word contains a `/` or fills a flag value — a path-valued argument refers
//!   to the child's filesystem, never to the host's working directory, so that is the
//!   only filesystem worth completing against (and without the grant, none is offered).
//!
//! Completion never widens anything: it only suggests text the user could have typed.

use std::path::{Component, Path, PathBuf};

/// Builtins and keywords that may start a command (eosh-core `parse.rs::command`).
const COMMAND_WORDS: &[&str] = &[
    "describe", "env", "exit", "help", "history", "imports", "let", "quit",
];

/// Gate keywords that may appear inside an expression (eosh-core reserved words).
const EXPRESSION_WORDS: &[&str] = &["as", "only", "rename", "with"];

/// The standard interface references offered for interface-shaped words (`only`
/// allow-lists). Exec is deliberately absent: children never receive it.
const INTERFACES: &[&str] = &[
    "eo9:disk/disk",
    "eo9:entropy/entropy",
    "eo9:fs/fs",
    "eo9:net/l2",
    "eo9:net/l3",
    "eo9:net/l4",
    "eo9:pci/pci",
    "eo9:perf/perf",
    "eo9:text/text",
    "eo9:time/time",
];

/// Characters that end a word: the shell's structural characters plus whitespace and
/// the string delimiter (mirrors eosh-core `lex.rs`).
fn ends_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '$' | '&' | '(' | ')' | ',' | '=' | '"')
}

/// One completion request's answer: the byte offset where the word being completed
/// starts, and the candidate replacements for that word (already filtered by its
/// prefix, sorted, deduplicated). Candidates ending in `/` are directories; the editor
/// completes them without a trailing space so the user can keep typing into them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub start: usize,
    pub candidates: Vec<String>,
}

/// The completion sources of one shell session.
pub struct ShellCompleter {
    /// Program names resolvable in the session (the bin view), sorted.
    names: Vec<String>,
    /// The filesystem children are granted (`--fs-root`), if any.
    fs_root: Option<PathBuf>,
}

impl ShellCompleter {
    pub fn new(mut names: Vec<String>, fs_root: Option<PathBuf>) -> Self {
        names.sort();
        names.dedup();
        ShellCompleter { names, fs_root }
    }

    /// Complete the word ending at byte offset `cursor` of `line`.
    pub fn complete(&self, line: &str, cursor: usize) -> Completion {
        let cursor = cursor.min(line.len());
        let before = &line[..cursor];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| ends_word(*c))
            .map(|(index, c)| index + c.len_utf8())
            .unwrap_or(0);
        let word = &before[start..];
        let context = &before[..start];

        let mut candidates = if word.starts_with("--") {
            // Flag names depend on the program's own argument signature, which the host
            // does not know without describing it; offer nothing rather than guesses.
            Vec::new()
        } else if word.contains(':') {
            complete_from(INTERFACES.iter().copied(), word)
        } else if word.contains('/') {
            self.complete_path(word)
        } else {
            let mut candidates = Vec::new();
            if at_command_start(context) {
                candidates.extend(complete_from(COMMAND_WORDS.iter().copied(), word));
            }
            candidates.extend(complete_from(EXPRESSION_WORDS.iter().copied(), word));
            candidates.extend(complete_from(self.names.iter().map(String::as_str), word));
            if in_flag_value_position(context) {
                candidates.extend(self.complete_path(word));
            }
            candidates
        };
        candidates.sort();
        candidates.dedup();
        Completion { start, candidates }
    }

    /// Complete `word` as a path on the filesystem the shell's children are granted.
    fn complete_path(&self, word: &str) -> Vec<String> {
        let Some(root) = &self.fs_root else {
            return Vec::new();
        };
        // Keep the directory part of the word verbatim in every candidate; complete the
        // final component against the granted root.
        let (dir_part, prefix) = match word.rfind('/') {
            Some(slash) => (&word[..=slash], &word[slash + 1..]),
            None => ("", word),
        };
        // Refuse to look outside the root (the provider would refuse such a path at run
        // time anyway).
        let relative = dir_part.trim_start_matches('/');
        if Path::new(relative)
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(root.join(relative)) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            let suffix = if is_dir { "/" } else { "" };
            candidates.push(format!("{dir_part}{name}{suffix}"));
        }
        candidates
    }
}

/// Is the word being completed the first word of the command?
fn at_command_start(context: &str) -> bool {
    context.trim().is_empty()
}

/// Is the word being completed the value of a `--flag`?
fn in_flag_value_position(context: &str) -> bool {
    context
        .trim_end()
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .is_some_and(|token| token.starts_with("--") && token.len() > 2)
}

/// All entries of `source` that start with `word`.
fn complete_from<'a>(source: impl Iterator<Item = &'a str>, word: &str) -> Vec<String> {
    source
        .filter(|entry| entry.starts_with(word))
        .map(str::to_string)
        .collect()
}

/// The longest common prefix of the candidates (the editor extends the word to it
/// before listing alternatives).
pub fn longest_common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = first.as_str();
    for candidate in &candidates[1..] {
        while !candidate.starts_with(prefix) {
            let Some((last, _)) = prefix.char_indices().next_back() else {
                return String::new();
            };
            prefix = &prefix[..last];
        }
    }
    prefix.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names() -> Vec<String> {
        ["hello", "outcomes", "time.frozen", "time.fuzzy", "eosh"]
            .iter()
            .map(|name| name.to_string())
            .collect()
    }

    fn completer() -> ShellCompleter {
        ShellCompleter::new(names(), None)
    }

    fn candidates(completer: &ShellCompleter, line: &str) -> Vec<String> {
        completer.complete(line, line.len()).candidates
    }

    #[test]
    fn command_start_offers_builtins_keywords_and_names() {
        let all = candidates(&completer(), "");
        assert!(all.contains(&"help".to_string()));
        assert!(all.contains(&"only".to_string()));
        assert!(all.contains(&"hello".to_string()));

        let he = candidates(&completer(), "he");
        assert_eq!(he, vec!["hello".to_string(), "help".to_string()]);
    }

    #[test]
    fn mid_expression_offers_names_and_keywords_but_not_builtins() {
        let after_compose = candidates(&completer(), "time.frozen $ hel");
        assert_eq!(after_compose, vec!["hello".to_string()]);
        let no_builtins = candidates(&completer(), "time.frozen $ des");
        assert!(no_builtins.is_empty(), "{no_builtins:?}");
        let keyword = candidates(&completer(), "let env2 = on");
        assert_eq!(keyword, vec!["only".to_string()]);
    }

    #[test]
    fn dotted_names_complete_by_prefix() {
        let completion = completer().complete("time.", 5);
        assert_eq!(completion.start, 0);
        assert_eq!(
            completion.candidates,
            vec!["time.frozen".to_string(), "time.fuzzy".to_string()]
        );
    }

    #[test]
    fn word_boundaries_follow_the_shell_lexer() {
        // The `$` ends the previous word even without whitespace.
        let completion = completer().complete("time.frozen$hel", 15);
        assert_eq!(completion.start, 12);
        assert_eq!(completion.candidates, vec!["hello".to_string()]);
    }

    #[test]
    fn interface_words_complete_against_the_standard_set() {
        let only = candidates(&completer(), "only eo9:f");
        assert_eq!(only, vec!["eo9:fs/fs".to_string()]);
        let many = candidates(&completer(), "only eo9:");
        assert!(many.len() >= 5, "{many:?}");
        // The layered net interfaces and pci are offered; the retired eo9:net/net is not.
        let net = candidates(&completer(), "only eo9:net/");
        assert_eq!(
            net,
            vec![
                "eo9:net/l2".to_string(),
                "eo9:net/l3".to_string(),
                "eo9:net/l4".to_string()
            ]
        );
        assert_eq!(
            candidates(&completer(), "only eo9:p"),
            vec!["eo9:pci/pci".to_string(), "eo9:perf/perf".to_string()]
        );
    }

    #[test]
    fn flag_names_are_not_guessed() {
        assert!(candidates(&completer(), "hello --na").is_empty());
    }

    #[test]
    fn paths_complete_against_the_child_fs_root_only() {
        // Without --fs-root there is nothing to offer.
        assert!(candidates(&completer(), "readwrite --path no").is_empty());
        assert!(candidates(&completer(), "readwrite --path docs/no").is_empty());

        let root = std::env::temp_dir().join(format!("eo9-complete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("notes.txt"), b"x").unwrap();
        fs::write(root.join("docs/readme.md"), b"x").unwrap();

        let completer = ShellCompleter::new(names(), Some(root.clone()));
        // A flag value completes against the root's entries (directories keep a `/`).
        let top = candidates(&completer, "readwrite --path no");
        assert_eq!(top, vec!["notes.txt".to_string()]);
        let dirs = candidates(&completer, "readwrite --path d");
        assert_eq!(dirs, vec!["docs/".to_string()]);
        // A word with a slash completes inside that directory, keeping the prefix.
        let nested = candidates(&completer, "readwrite --path docs/re");
        assert_eq!(nested, vec!["docs/readme.md".to_string()]);
        // Escaping the root is never offered.
        assert!(candidates(&completer, "readwrite --path ../").is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn longest_common_prefix_extends_ambiguous_words() {
        let candidates = vec!["time.frozen".to_string(), "time.fuzzy".to_string()];
        assert_eq!(longest_common_prefix(&candidates), "time.f");
        assert_eq!(longest_common_prefix(&[]), "");
        assert_eq!(
            longest_common_prefix(&["hello".to_string(), "outcomes".to_string()]),
            ""
        );
    }
}
