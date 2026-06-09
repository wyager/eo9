//! Plain-language cards for `describe` on the shell's own builtins and operators.
//!
//! `describe` works on anything you can type: store programs and providers go through the
//! backend (kind, args, imports, exports, wiring), and the words of the shell itself —
//! builtins like `let` and `svc`, operators like `$` and `only` — land here, on hand-written
//! cards in the same tone (user study 10's "the shell should explain itself" thread, and the
//! owner's ask that `describe describe` work). The card is data, not behavior: rendering one
//! never evaluates or runs anything.
//!
//! Every line is kept within the try-it page terminal's ~109-column budget; the
//! `every_builtin_and_operator_has_a_card` test pins both coverage and the budget, so a new
//! builtin cannot ship undescribed (the same recurrence guard the help examples have).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One `describe` card for a shell word.
pub struct BuiltinDoc {
    /// The word and its aliases (`exit` and `quit` share a card; `$` answers to `compose`).
    pub names: &'static [&'static str],
    /// What kind of thing this is — the card's `kind:` line.
    pub kind: &'static str,
    /// The plain-language paragraph, pre-wrapped.
    pub summary: &'static [&'static str],
    /// The usage shape followed by a concrete example.
    pub usage: &'static [&'static str],
    /// Where to look next.
    pub related: &'static str,
}

/// Builtin: part of the shell itself.
const BUILTIN: &str = "builtin (a word of the shell itself, not a program in /bin)";
/// Operator: part of the composition grammar.
const OPERATOR: &str = "operator (composition grammar, applied before anything runs)";

/// The cards. Coverage is pinned by test against the parser's dispatch list and the
/// `builtins:` line of `help`.
pub const BUILTIN_DOCS: &[BuiltinDoc] = &[
    BuiltinDoc {
        names: &["help"],
        kind: BUILTIN,
        summary: &[
            "Prints the shell's one-page guide: the command shape, the composition operators with examples,",
            "and how to explore the sandbox. Start here, then `describe` anything you meet.",
        ],
        usage: &["help", "e.g. help"],
        related: "describe, env",
    },
    BuiltinDoc {
        names: &["describe"],
        kind: BUILTIN,
        summary: &[
            "Tells you what something is without running it. For a program or provider: its kind, arguments,",
            "imports, exports, and the wiring tree of a composition. For a shell word — a builtin or an",
            "operator, including `describe` itself — a card like this one.",
        ],
        usage: &[
            "describe <name, expr, builtin, or operator>",
            "e.g. describe entropy.seeded",
        ],
        related: "imports, env, help",
    },
    BuiltinDoc {
        names: &["man"],
        kind: BUILTIN,
        summary: &[
            "Shows a program's own manual — the user-facing page its author embedded in the component",
            "(synopsis, what it does, each argument with its values, examples). Falls back to `describe`",
            "when there is no manual. The manual is the author's prose; `describe` stays the mechanical",
            "truth, and `man` flags any disagreement. Shell words and OS APIs get their describe cards.",
        ],
        usage: &["man <name>", "e.g. man telnetd"],
        related: "describe, help",
    },
    BuiltinDoc {
        names: &["imports"],
        kind: BUILTIN,
        summary: &[
            "Prints just the residual imports of an expression: what it still needs from outside — its",
            "capability set. `describe` shows the same list along with everything else.",
        ],
        usage: &["imports <expr>", "e.g. imports entropy.seeded $ rng"],
        related: "describe, env",
    },
    BuiltinDoc {
        names: &["env"],
        kind: BUILTIN,
        summary: &[
            "Bare `env` shows this session's capability picture: what the shell holds and what programs run",
            "from it receive. `env <expr>` shows how the session would treat one expression's imports —",
            "satisfied, optional-and-absent, or refused at spawn — without running anything.",
        ],
        usage: &["env  |  env <expr>", "e.g. env"],
        related: "describe, imports",
    },
    BuiltinDoc {
        names: &["history"],
        kind: BUILTIN,
        summary: &["Lists the lines you have entered this session, numbered."],
        usage: &["history", "e.g. history"],
        related: "help",
    },
    BuiltinDoc {
        names: &["let"],
        kind: BUILTIN,
        summary: &[
            "Names a component or environment value for this session: the expression is evaluated to a",
            "value and bound, not run. Use the name anywhere an expression goes. Bindings live only in",
            "this session; `save` is the persistent form.",
        ],
        usage: &[
            "let <name> = <expr>",
            "e.g. let det = time.frozen & entropy.seeded --seed 7",
        ],
        related: "save, describe",
    },
    BuiltinDoc {
        names: &["save"],
        kind: BUILTIN,
        summary: &[
            "Persists a program or composition to the store as /bin/<name>.wasm, where the store is",
            "writable (a metal boot with the storedisk grant; refused on read-only stores). The saved",
            "program keeps exactly the capabilities the expression composed in.",
        ],
        usage: &[
            "save <name> = <expr>",
            "e.g. save frozen-hello = time.frozen --now-seconds 5 --monotonic-ns 0 $ hello",
        ],
        related: "let, describe",
    },
    BuiltinDoc {
        names: &["detach"],
        kind: BUILTIN,
        summary: &[
            "Hands a composed program to the service registry to run in the background under a restart",
            "policy — a tiny pure policy program: restart.never, restart.always, or restart.backoff",
            "--max-restarts N --base-delay-ms MS. The service runs with exactly what you composed into it,",
            "never the registry's authority. Needs the svc capability (eo9 --svc); backgrounding is a grant.",
        ],
        usage: &[
            "detach <name> = <expr> restart <policy>",
            "e.g. detach worker = cruncher --rounds 100000 restart restart.never",
        ],
        related: "svc, describe",
    },
    BuiltinDoc {
        names: &["svc"],
        kind: BUILTIN,
        summary: &[
            "Inspects background services: bare `svc` (or `svc list`) prints the table — name, state,",
            "restarts, last outcome; `svc log <name>` prints a service's captured output; `svc stop <name>`",
            "stops one (final: a stopped service never restarts); `svc clear <name>` drops a finished record.",
        ],
        usage: &[
            "svc [list | log <name> | stop <name> | clear <name>]",
            "e.g. svc list",
        ],
        related: "detach",
    },
    BuiltinDoc {
        names: &["exit", "quit"],
        kind: BUILTIN,
        summary: &[
            "Ends the session. What that means belongs to whoever runs the shell: the usermode CLI returns",
            "to your terminal, the bare-metal console powers off (until init owns the console), the browser",
            "page asks for a reload. Under init, leaving the shell restarts the console while services are",
            "still running — halting the machine is `poweroff`, its own intent.",
        ],
        usage: &["exit", "e.g. exit"],
        related: "poweroff, help",
    },
    BuiltinDoc {
        names: &["poweroff"],
        kind: BUILTIN,
        summary: &[
            "Ends the session AND asks the embedder to halt: the intent flows up as the shell's own typed",
            "outcome, so under init the machine powers off even while services are running (plain `exit`",
            "would restart the console instead). Without init it behaves like `exit`. A session whose",
            "supervisor withheld the power capability (a telnet session, unless telnetd was started with",
            "--allow-poweroff) gets a typed refusal naming the missing capability, and stays open.",
        ],
        usage: &["poweroff", "e.g. poweroff"],
        related: "exit, svc",
    },
    BuiltinDoc {
        names: &["$", "compose"],
        kind: OPERATOR,
        summary: &[
            "Composition: `provider $ program` satisfies the program's imports from the provider's exports",
            "and seals them — no outer layer can re-supply what an inner provider already answered. It is",
            "right-associative, the rightmost term is what runs, and the whole chain is fused and compiled",
            "before it runs.",
        ],
        usage: &[
            "provider $ program",
            "e.g. entropy.seeded --seed 7 $ rng --count 2",
        ],
        related: "&, only, describe",
    },
    BuiltinDoc {
        names: &["&", "extend"],
        kind: OPERATOR,
        summary: &[
            "Environment extension: `base & layer` bundles providers into one environment value, later",
            "layers overriding earlier ones for the same interface. An environment is composed onto a",
            "program with `$` — the action law (x & y) $ c is exactly x $ y $ c.",
        ],
        usage: &[
            "base & layer",
            "e.g. time.frozen --now-seconds 0 --monotonic-ns 0 & entropy.seeded --seed 7",
        ],
        related: "$, let, describe",
    },
    BuiltinDoc {
        names: &["only"],
        kind: OPERATOR,
        summary: &[
            "Restriction: `only <list> $ expr` bounds everything to its right to an allow-list of",
            "interfaces. A required import outside the list is refused before anything runs; optional ones",
            "are sealed absent. An entry naming a package (eo9:text) admits all its interfaces. Attenuation",
            "only narrows: an outer `only` can never widen an inner one.",
        ],
        usage: &[
            "only <iface,…> $ <expr>",
            "e.g. only eo9:text,eo9:time $ hello",
        ],
        related: "$, env, describe",
    },
    BuiltinDoc {
        names: &["rename"],
        kind: OPERATOR,
        summary: &[
            "Relabels a capability slot so a provider can be aimed at a differently-named import:",
            "`rename <from> <to> $ expr` rewires the slot before composition. Most compositions never",
            "need it; multi-instance consumers (two disks, two filesystems) do.",
        ],
        usage: &[
            "rename <from> <to> $ <expr>",
            "e.g. rename eo9:fs/fs upper $ fs.overlay",
        ],
        related: "with, $, describe",
    },
    BuiltinDoc {
        names: &["with"],
        kind: OPERATOR,
        summary: &[
            "Binds providers to named slots in one step: `with <provider> as <slot>, … $ expr` is the",
            "several-at-once form of rename — the way to fill a multi-slot consumer like an overlay",
            "filesystem (upper and lower) without spelling each rename separately.",
        ],
        usage: &[
            "with <provider> as <slot>, <provider> as <slot> $ <expr>",
            "e.g. with fs.memfs as upper, fs.readonly as lower $ fs.overlay $ ls /",
        ],
        related: "rename, $, describe",
    },
];

/// Look a shell word up (exact match over names and aliases).
pub fn builtin_doc(word: &str) -> Option<&'static BuiltinDoc> {
    BUILTIN_DOCS.iter().find(|doc| doc.names.contains(&word))
}

/// Render a card in `describe`'s voice.
pub fn render_builtin_doc(doc: &BuiltinDoc) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("kind: {}", doc.kind));
    for line in doc.summary {
        lines.push(String::from(*line));
    }
    lines.push(String::from("usage:"));
    for line in doc.usage {
        lines.push(format!("  {line}"));
    }
    lines.push(format!("related: {}", doc.related));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The try-it page terminal's column budget (www/site/vm — `help` is sized to it too).
    const COLUMN_BUDGET: usize = 109;

    /// The words the parser dispatches as builtins/operators. Keep in sync with
    /// `parse.rs`'s command match and the expression grammar — the point of this list is
    /// that adding a word THERE without a card HERE fails the test below.
    pub const SHELL_WORDS: &[&str] = &[
        "help", "describe", "man", "imports", "env", "history", "let", "save", "detach", "svc",
        "exit", "quit", "poweroff", "$", "&", "only", "rename", "with", "compose", "extend",
    ];

    #[test]
    fn every_builtin_and_operator_has_a_card() {
        for word in SHELL_WORDS {
            let doc = builtin_doc(word).unwrap_or_else(|| {
                panic!("`{word}` has no describe card — add one to BUILTIN_DOCS")
            });
            let lines = render_builtin_doc(doc);
            assert!(
                lines.first().is_some_and(|l| l.starts_with("kind: ")),
                "`{word}`'s card must open with its kind"
            );
            assert!(
                lines.iter().any(|l| l.trim_start().starts_with("e.g. ")),
                "`{word}`'s card must end its usage with a concrete example"
            );
            assert!(
                lines.iter().any(|l| l.starts_with("related: ")),
                "`{word}`'s card must point somewhere next"
            );
            for line in &lines {
                assert!(
                    line.chars().count() <= COLUMN_BUDGET,
                    "`{word}`'s card line exceeds the {COLUMN_BUDGET}-column page budget: {line:?}"
                );
            }
        }
    }

    #[test]
    fn every_builtin_the_help_text_lists_has_a_card() {
        // The `builtins:` line of help is the user-facing inventory; every word on it must
        // describe itself. (Names are listed with argument hints — strip them.)
        let line = crate::session::help_lines()
            .iter()
            .find(|l| l.starts_with("builtins:"))
            .expect("help has a `builtins:` line");
        for word in line.trim_start_matches("builtins:").split(',') {
            let word = word.trim().split([' ', '[']).next().unwrap_or_default();
            if word.is_empty() {
                continue;
            }
            assert!(
                builtin_doc(word).is_some(),
                "help lists `{word}` but it has no describe card"
            );
        }
    }

    #[test]
    fn aliases_share_their_card() {
        assert!(core::ptr::eq(
            builtin_doc("exit").unwrap(),
            builtin_doc("quit").unwrap()
        ));
        assert!(core::ptr::eq(
            builtin_doc("$").unwrap(),
            builtin_doc("compose").unwrap()
        ));
    }

    #[test]
    fn unknown_words_have_no_card() {
        assert!(builtin_doc("hello").is_none());
        assert!(builtin_doc("ls").is_none());
        assert!(builtin_doc("").is_none());
    }
}
