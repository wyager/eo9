//! The abstract syntax of eosh command lines.
//!
//! Parenthesized grouping leaves no trace in the tree — `a $ (b $ c)` and `a $ b $ c`
//! parse to the same [`Expr`], which is exactly the spec's right-associativity claim —
//! except in argument position, where a parenthesized expression is its own
//! [`ArgValue`] variant because type-directed argument handling must distinguish a
//! program expression from literal text.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A program expression: the part of a command that evaluates to a component value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A (possibly dotted) program or binding name: `browser`, `virtualfs.create`,
    /// `time.frozen`, or a `let`-bound name.
    Name(String),
    /// Argument application: `callee --flag value positional …`. Binds tightest.
    App { callee: Box<Expr>, args: Vec<Arg> },
    /// `provider $ consumer` — composition (right-associative).
    Compose {
        provider: Box<Expr>,
        consumer: Box<Expr>,
    },
    /// `base & layer` — environment extension (left-associative).
    Extend { base: Box<Expr>, layer: Box<Expr> },
    /// `only <allow-list> $ body` — restrict everything to the right to the allow-list.
    Only { allow: Vec<String>, body: Box<Expr> },
    /// `rename <from> <to> $ body` — relabel a slot on everything to the right.
    Rename {
        from: String,
        to: String,
        body: Box<Expr>,
    },
    /// `with p as n, … $ body` — bind providers to named slots of everything to the
    /// right. The tuple form `with (a, b) as (x, y)` has already been expanded into
    /// its individual bindings by the parser.
    With {
        bindings: Vec<WithBinding>,
        body: Box<Expr>,
    },
}

/// One `provider as slot-name` binding of a `with` gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithBinding {
    pub provider: Expr,
    pub slot: String,
}

/// One argument of an application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// `--name value`.
    Flag { name: String, value: ArgValue },
    /// A positional argument, filled against the callee's parameters in declared order.
    Positional(ArgValue),
}

/// The surface form of an argument value. Which forms are admissible — and whether a
/// bare word is literal text or a program name — is decided by the declared type of the
/// parameter it fills (SPEC.md, "Type-directed arguments").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValue {
    /// A bare word: literal text for a data-typed parameter, a program name for a
    /// component-typed one.
    Word(String),
    /// A quoted string literal: always literal text.
    Quoted(String),
    /// A parenthesized program expression: only admissible for component-typed
    /// parameters.
    Expr(Box<Expr>),
}

/// One parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A blank line or a pure comment.
    Empty,
    /// `let name = expr` — bind a component/environment value for the session.
    Let { name: String, expr: Expr },
    /// `save name = expr` — persist a component value to the session's store
    /// (`/bin/<name>.wasm`), where the store is writable.
    Save { name: String, expr: Expr },
    /// A program expression to be composed with the shell's environment and run.
    Run(Expr),
    /// `describe <expr>` — print kind, arguments, imports, and exports.
    Describe(Expr),
    /// `describe <builtin-or-operator>` — print the shell word's own card
    /// (`describe describe`, `describe $`, …). Carries the word as typed.
    DescribeBuiltin(String),
    /// `describe <OS API name>` — the card of a WIT package (`describe eo9:pci`) or
    /// interface (`describe eo9:pci/pci`). Any single colon-bearing word routes here:
    /// store program names cannot contain `:`, so the spelling is unambiguous.
    DescribeApi(String),
    /// `imports <expr>` — print the residual imports.
    Imports(Expr),
    /// `env` — show the session's capability picture (what the shell holds, what
    /// children receive) and the granted environment, if any.
    Env,
    /// `env <expr>` — show how this session would treat the expression's imports
    /// (satisfied, optional-and-absent, or refused at spawn).
    EnvOf(Expr),
    /// `detach <name> = <expr> restart <policy>` — compose a program and hand it to the
    /// service registry to run in the background under a restart policy.
    Detach {
        name: String,
        expr: Expr,
        policy: Expr,
    },
    /// `svc` / `svc list` — list the registry's services.
    SvcList,
    /// `svc log <name>` — print a service's captured output.
    SvcLog(String),
    /// `svc stop <name>` — stop a service.
    SvcStop(String),
    /// `svc clear <name>` — remove a finished service's record.
    SvcClear(String),
    /// `history` — show the lines entered this session.
    History,
    /// `help`.
    Help,
    /// `exit` / `quit`.
    Exit,
    /// `poweroff` — end the session AND ask the embedder to halt: under init, leaving
    /// the shell restarts the console while services live (ruling D), so halting the
    /// machine is its own, explicit intent.
    Poweroff,
}

impl Expr {
    /// Convenience constructor used by the parser and tests.
    pub fn name(s: &str) -> Expr {
        Expr::Name(String::from(s))
    }
}
