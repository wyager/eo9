//! The shell session: `let` bindings, history, the granted environment, the builtins,
//! and the top-level rule.
//!
//! The top-level rule is the spec's, verbatim (SPEC.md, "Execution APIs"): *compose my
//! environment onto the command, compile, spawn* — then await the outcome and print it
//! as WAVE. Naming or composing a program never runs it; only a complete command line
//! in command position is run, and only here.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::{Command, Expr};
use crate::backend::{Backend, ComponentKind, Outcome};
use crate::eval::{EvalError, Evaluator, complete_args};
use crate::parse::parse_command;
use crate::render::{render_imports, render_info, render_outcome};

/// What a line of input amounted to, for the embedding `main` (interactive loop or
/// one-shot `--command` mode) to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineResult {
    /// The line was handled (including a program run that succeeded).
    Ok,
    /// A program ran but reported failure or ended abnormally (rendered text included).
    ProgramFailed(String),
    /// The line could not be parsed or evaluated (rendered error included).
    Error(String),
    /// The user asked to leave the shell.
    Exit,
}

/// One shell session: the backend plus everything the user has built up in it.
pub struct Session<B: Backend> {
    backend: B,
    bindings: BTreeMap<String, B::Component>,
    environment: Option<B::Component>,
    history: Vec<String>,
}

impl<B: Backend> Session<B> {
    pub fn new(backend: B) -> Self {
        Session {
            backend,
            bindings: BTreeMap::new(),
            environment: None,
            history: Vec::new(),
        }
    }

    /// Hand the shell its granted environment (an environment value possessed by the
    /// shell's parent and passed down). Composed onto every top-level command.
    pub fn grant_environment(&mut self, environment: B::Component) {
        self.environment = Some(environment);
    }

    /// Borrow the backend (the embedding `main` uses this for its prompt and read loop).
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Execute one line of input: parse, dispatch, print, and report what happened.
    pub async fn execute_line(&mut self, line: &str) -> LineResult {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            self.history.push(trimmed.to_string());
        }

        let command = match parse_command(line) {
            Ok(command) => command,
            Err(err) => {
                let message = format!("parse error: {err}");
                self.backend.print_error(&message);
                return LineResult::Error(message);
            }
        };

        match command {
            Command::Empty => LineResult::Ok,
            Command::Help => {
                for line in help_lines() {
                    self.backend.print(line);
                }
                LineResult::Ok
            }
            Command::History => {
                for (index, entry) in self.history.iter().enumerate() {
                    self.backend.print(&format!("{:4}  {entry}", index + 1));
                }
                LineResult::Ok
            }
            Command::Env => {
                match &self.environment {
                    None => self.backend.print("no environment granted"),
                    Some(environment) => {
                        let info = self.backend.describe(environment);
                        self.backend.print("granted environment:");
                        for line in render_info(&info) {
                            self.backend.print(&format!("  {line}"));
                        }
                    }
                }
                if !self.bindings.is_empty() {
                    self.backend.print("bindings:");
                    let names: Vec<String> = self.bindings.keys().cloned().collect();
                    for name in names {
                        self.backend.print(&format!("  {name}"));
                    }
                }
                LineResult::Ok
            }
            Command::Exit => LineResult::Exit,
            Command::Let { name, expr } => self.run_let(name, &expr).await,
            Command::Describe(expr) => self.run_describe(&expr, false).await,
            Command::Imports(expr) => self.run_describe(&expr, true).await,
            Command::Run(expr) => self.run_program(&expr).await,
        }
    }

    /// `let name = expr`: evaluate to a component value and remember it.
    async fn run_let(&mut self, name: String, expr: &Expr) -> LineResult {
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        match evaluator
            .eval_plain(expr, "a `let` binding (arguments are bound at run time)")
            .await
        {
            Ok(component) => {
                self.bindings.insert(name, component);
                LineResult::Ok
            }
            Err(err) => self.report(err),
        }
    }

    /// `describe expr` / `imports expr`.
    async fn run_describe(&mut self, expr: &Expr, imports_only: bool) -> LineResult {
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        let component = match evaluator.eval(expr).await {
            Ok(output) => output.component,
            Err(err) => return self.report(err),
        };
        let info = self.backend.describe(&component);
        let lines = if imports_only {
            render_imports(&info)
        } else {
            render_info(&info)
        };
        for line in lines {
            self.backend.print(&line);
        }
        LineResult::Ok
    }

    /// The top-level rule: compose the granted environment onto the command, compile,
    /// spawn with the bound arguments, await the outcome, print it.
    async fn run_program(&mut self, expr: &Expr) -> LineResult {
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        let output = match evaluator.eval(expr).await {
            Ok(output) => output,
            Err(err) => return self.report(err),
        };
        let mut component = output.component;
        let mut args = output.args;

        let info = self.backend.describe(&component);
        if info.kind == ComponentKind::Provider {
            return self.report(EvalError::TopLevelProvider);
        }
        if let Err(err) = complete_args(&mut args, &info.args) {
            return self.report(err);
        }

        if let Some(environment) = &self.environment {
            let environment = match self.backend.duplicate(environment) {
                Ok(environment) => environment,
                Err(err) => return self.report(EvalError::Backend(err)),
            };
            component = match self.backend.compose(environment, component) {
                Ok(component) => component,
                Err(err) => return self.report(EvalError::Backend(err)),
            };
        }

        let image = match self.backend.compile(component) {
            Ok(image) => image,
            Err(err) => return self.report(EvalError::Backend(err)),
        };
        let task = match self.backend.spawn(&image, &args) {
            Ok(task) => task,
            Err(err) => return self.report(EvalError::Backend(err)),
        };
        let outcome = self.backend.wait(task).await;

        let rendered = render_outcome(&outcome);
        self.backend.print(&rendered);
        match outcome {
            Outcome::Success(_) => LineResult::Ok,
            Outcome::Failure(_) | Outcome::Abnormal(_) => LineResult::ProgramFailed(rendered),
        }
    }

    /// Print an error and turn it into a [`LineResult`].
    fn report(&mut self, err: EvalError) -> LineResult {
        let message = format!("error: {err}");
        self.backend.print_error(&message);
        LineResult::Error(message)
    }
}

/// The `help` builtin's text.
pub fn help_lines() -> &'static [&'static str] {
    &[
        "eosh — the Eo9 shell. A command composes programs and runs the result.",
        "",
        "  program --flag value …        run a program with named, typed arguments",
        "  provider $ program            compose: satisfy the program's imports (right-assoc)",
        "  base & layer                  extend an environment (later layers override)",
        "  only <iface,…> $ …            restrict everything to the right to an allow-list",
        "  rename <from> <to> $ …        relabel a capability slot",
        "  with <provider> as <slot>, …  bind providers to named slots (tuples bind positionally)",
        "  let <name> = <expr>           name a component or environment value",
        "  (…)                           grouping; a parenthesized argument is passed open, not run",
        "",
        "builtins: help, env, history, describe <expr>, imports <expr>, exit",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    use crate::backend::{AbnormalExit, WaveValue};
    use crate::testutil::{MockBackend, binary, block_on_ready, provider};

    fn session_with(programs: &[(&str, crate::backend::ComponentInfo)]) -> Session<MockBackend> {
        let mut backend = MockBackend::new();
        for (name, info) in programs {
            backend.program(name, info.clone());
        }
        Session::new(backend)
    }

    fn run(session: &mut Session<MockBackend>, line: &str) -> LineResult {
        block_on_ready(session.execute_line(line))
    }

    #[test]
    fn the_top_level_rule_compiles_spawns_waits_and_prints() {
        let mut session = session_with(&[
            ("net.deny", provider(&["eo9:net/net"])),
            ("fetcher", binary(&[("url", "string")])),
        ]);
        let result = run(&mut session, "net.deny $ fetcher --url https://example.com");
        assert_eq!(result, LineResult::Ok);
        assert_eq!(
            session.backend.log,
            vec![
                "resolve(net.deny) -> c1",
                "resolve(fetcher) -> c2",
                "describe(c2)",
                "compose(c1, c2) -> c3",
                "describe(c3)",
                "compile(c3) -> i1",
                "spawn(i1, [url=\"https://example.com\"]) -> t1",
                "wait(t1)",
            ]
        );
        assert_eq!(session.backend.out, vec!["ok: done"]);
        assert!(session.backend.err.is_empty());
    }

    #[test]
    fn failure_and_abnormal_outcomes_are_reported_as_program_failures() {
        let mut session = session_with(&[("outcomes", binary(&[("mode", "string")]))]);
        session.backend.outcome = Outcome::Failure(WaveValue {
            ty: "program-failure".to_string(),
            value: "requested-failure(\"went wrong\")".to_string(),
        });
        let result = run(&mut session, "outcomes --mode fail");
        assert_eq!(
            result,
            LineResult::ProgramFailed("error: requested-failure(\"went wrong\")".to_string())
        );
        assert_eq!(
            session.backend.out,
            vec!["error: requested-failure(\"went wrong\")"]
        );

        session.backend.outcome =
            Outcome::Abnormal(AbnormalExit::Trapped("unreachable".to_string()));
        let result = run(&mut session, "outcomes --mode trap");
        assert_eq!(
            result,
            LineResult::ProgramFailed("abnormal: trapped: unreachable".to_string())
        );
    }

    #[test]
    fn running_a_provider_at_top_level_is_an_error() {
        let mut session = session_with(&[("memfs", provider(&["eo9:fs/fs"]))]);
        let result = run(&mut session, "memfs");
        assert!(matches!(result, LineResult::Error(_)));
        assert_eq!(session.backend.err.len(), 1);
        assert!(session.backend.err[0].contains("provider"));
        // Nothing was compiled or spawned.
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compile"))
        );
    }

    #[test]
    fn missing_required_arguments_stop_before_compile() {
        let mut session = session_with(&[(
            "browser",
            binary(&[("url", "string"), ("proxy", "option<string>")]),
        )]);
        let result = run(&mut session, "browser");
        assert_eq!(
            result,
            LineResult::Error("error: missing argument `--url` (a string)".to_string())
        );
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compile"))
        );

        // With the required one given, the optional one is auto-filled with `none`.
        let result = run(&mut session, "browser --url https://example.com");
        assert_eq!(result, LineResult::Ok);
        assert!(
            session
                .backend
                .log
                .iter()
                .any(|line| line.contains("proxy=none")),
            "expected spawn args to include proxy=none, log: {:?}",
            session.backend.log
        );
    }

    #[test]
    fn let_bindings_are_stored_and_reusable() {
        let mut session = session_with(&[
            ("time.frozen", provider(&["eo9:time/time"])),
            ("virtualnet", provider(&["eo9:net/net"])),
            ("app", binary(&[])),
        ]);
        assert_eq!(
            run(&mut session, "let det-env = time.frozen & virtualnet"),
            LineResult::Ok
        );
        // Use it twice: each use duplicates the stored value rather than consuming it.
        assert_eq!(run(&mut session, "det-env $ app"), LineResult::Ok);
        assert_eq!(run(&mut session, "det-env $ app"), LineResult::Ok);
        let duplicates = session
            .backend
            .log
            .iter()
            .filter(|line| line.starts_with("duplicate"))
            .count();
        assert_eq!(duplicates, 2);
    }

    #[test]
    fn let_rejects_run_time_arguments() {
        let mut session = session_with(&[("browser", binary(&[("url", "string")]))]);
        let result = run(&mut session, "let b = browser --url https://example.com");
        assert!(matches!(result, LineResult::Error(_)));
    }

    #[test]
    fn granted_environment_is_composed_onto_every_run() {
        let mut session = session_with(&[("app", binary(&[]))]);
        let environment = session.backend.insert(provider(&["eo9:time/time"]));
        session.grant_environment(environment);
        assert_eq!(run(&mut session, "app"), LineResult::Ok);
        assert!(
            session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("duplicate(c1)")),
            "the environment is duplicated, not consumed: {:?}",
            session.backend.log
        );
        assert!(
            session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compose"))
        );
        // And it is still there for the next command.
        assert_eq!(run(&mut session, "app"), LineResult::Ok);
    }

    #[test]
    fn describe_and_imports_builtins_print_without_running() {
        let mut session = session_with(&[("memfs", provider(&["eo9:fs/fs"]))]);
        assert_eq!(run(&mut session, "describe memfs"), LineResult::Ok);
        assert!(session.backend.out.iter().any(|l| l == "kind: provider"));
        assert!(session.backend.out.iter().any(|l| l.contains("eo9:fs/fs")));
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compile"))
        );

        session.backend.out.clear();
        assert_eq!(run(&mut session, "imports memfs"), LineResult::Ok);
        assert_eq!(session.backend.out, vec!["imports: (none)"]);
    }

    #[test]
    fn env_help_history_exit_and_empty_lines() {
        let mut session = session_with(&[]);
        assert_eq!(run(&mut session, ""), LineResult::Ok);
        assert_eq!(run(&mut session, "   # comment only"), LineResult::Ok);
        assert_eq!(run(&mut session, "env"), LineResult::Ok);
        assert_eq!(session.backend.out, vec!["no environment granted"]);
        assert_eq!(run(&mut session, "help"), LineResult::Ok);
        assert!(session.backend.out.iter().any(|l| l.contains("builtins")));
        assert_eq!(run(&mut session, "history"), LineResult::Ok);
        assert!(
            session
                .backend
                .out
                .iter()
                .any(|l| l.contains("# comment only"))
        );
        assert_eq!(run(&mut session, "exit"), LineResult::Exit);
        assert_eq!(run(&mut session, "quit"), LineResult::Exit);
    }

    #[test]
    fn parse_and_resolution_errors_are_printed_to_stderr() {
        let mut session = session_with(&[]);
        let result = run(&mut session, "interpret (virtualnet $ browser");
        assert!(matches!(result, LineResult::Error(_)));
        assert_eq!(session.backend.err.len(), 1);
        assert!(session.backend.err[0].starts_with("parse error:"));

        let result = run(&mut session, "no-such-program");
        assert_eq!(
            result,
            LineResult::Error(
                "error: cannot resolve `no-such-program`: no such module".to_string()
            )
        );
    }
}
