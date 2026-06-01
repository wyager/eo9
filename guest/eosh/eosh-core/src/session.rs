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
use crate::backend::{AbnormalExit, Backend, ComponentKind, Outcome, ServiceInfo};
use crate::envinfo::{self, SessionManifest};
use crate::eval::{EvalError, Evaluator, complete_args};
use crate::parse::parse_command;
use crate::render::{render_imports, render_info, render_outcome};

/// How a program that ran went wrong: the executor's three-way view minus success.
/// Carried by [`LineResult::ProgramFailed`] so the one-shot embedder can report an
/// honest class (and exit code) for the *inner* command instead of collapsing failure
/// and abnormal endings into one case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    /// The program ran and reported failure in its own vocabulary.
    Failed,
    /// The program trapped.
    Trapped,
    /// The program was killed before producing an outcome.
    Killed,
}

/// What a line of input amounted to, for the embedding `main` (interactive loop or
/// one-shot `--command` mode) to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineResult {
    /// The line was handled (including a program run that succeeded).
    Ok,
    /// A program ran but reported failure or ended abnormally (the class says which;
    /// rendered text included).
    ProgramFailed(CommandClass, String),
    /// The line could not be parsed or evaluated — no program ran (rendered error
    /// included).
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
    /// Where the per-command outcome line (`ok: …`/`error: …`) goes: standard output in
    /// an interactive REPL (the default), standard error in one-shot (`--command`) mode so
    /// a `-c` invocation's standard output carries only the program's own output — matching
    /// `eo9 run`, whose outcome line is on stderr by default.
    outcome_on_stderr: bool,
}

impl<B: Backend> Session<B> {
    pub fn new(backend: B) -> Self {
        Session {
            backend,
            bindings: BTreeMap::new(),
            environment: None,
            history: Vec::new(),
            outcome_on_stderr: false,
        }
    }

    /// Route the per-command outcome line to standard error instead of standard output
    /// (used by one-shot `--command` mode so pipes carry only program output).
    pub fn route_outcome_to_stderr(&mut self) {
        self.outcome_on_stderr = true;
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
            Command::Env => self.run_env().await,
            Command::EnvOf(expr) => self.run_env_of(&expr).await,
            Command::Exit => LineResult::Exit,
            Command::Let { name, expr } => self.run_let(name, &expr).await,
            Command::Save { name, expr } => self.run_save(name, &expr).await,
            Command::Detach { name, expr, policy } => self.run_detach(name, &expr, &policy).await,
            Command::SvcList => self.run_svc_list(),
            Command::SvcLog(name) => self.run_svc_log(&name),
            Command::SvcStop(name) => self.run_svc_stop(&name),
            Command::SvcClear(name) => self.run_svc_clear(&name),
            Command::Describe(expr) => self.run_describe(&expr, false).await,
            Command::Imports(expr) => self.run_describe(&expr, true).await,
            Command::Run(expr) => self.run_program(&expr).await,
        }
    }

    /// `env`: the session's capability picture (from the embedder's manifest), plus the
    /// granted environment and `let` bindings the session has built up.
    async fn run_env(&mut self) -> LineResult {
        match self.manifest().await {
            Some(manifest) => {
                for line in envinfo::render_session(&manifest) {
                    self.backend.print(&line);
                }
            }
            None => self
                .backend
                .print("no session capability information available"),
        }

        if let Some(environment) = &self.environment {
            let info = self.backend.describe(environment);
            self.backend.print("granted environment:");
            for line in render_info(&info) {
                self.backend.print(&format!("  {line}"));
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

    /// `env <expr>`: how this session would treat the expression's imports if it were
    /// run — without running (or even compiling) anything.
    async fn run_env_of(&mut self, expr: &Expr) -> LineResult {
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        let component = match evaluator.eval(expr).await {
            Ok(output) => output.component,
            Err(err) => return self.report(err),
        };
        let info = self.backend.describe(&component);
        let manifest = self.manifest().await;
        for line in envinfo::render_capability_view(&info, manifest.as_ref()) {
            self.backend.print(&line);
        }
        LineResult::Ok
    }

    /// The parsed session manifest, if the embedder left one where the backend can
    /// read it.
    async fn manifest(&mut self) -> Option<SessionManifest> {
        let text = self.backend.session_manifest().await?;
        SessionManifest::parse(&text)
    }

    /// `let name = expr`: evaluate to a component value and remember it.
    async fn run_let(&mut self, name: String, expr: &Expr) -> LineResult {
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        match evaluator
            .eval_plain(expr, "a `let` binding (arguments are bound at run time)")
            .await
        {
            Ok(component) => {
                // Confirm what was bound — a silent success reads as nothing having
                // happened, and the user cannot tell the binding exists until they use
                // it (user study 10, finding 6). One line: the name, its kind, and (for
                // a provider) what it offers.
                let info = self.backend.describe(&component);
                let confirmation = match info.kind {
                    ComponentKind::Binary => format!("{name}: bound (a program)"),
                    ComponentKind::Provider => {
                        let exports: Vec<&str> = info
                            .exports
                            .iter()
                            .map(|export| export.interface.as_str())
                            .collect();
                        if exports.is_empty() {
                            format!("{name}: bound (a provider)")
                        } else {
                            format!("{name}: bound (a provider of {})", exports.join(", "))
                        }
                    }
                };
                self.backend.print(&confirmation);
                self.bindings.insert(name, component);
                LineResult::Ok
            }
            Err(err) => self.report(err),
        }
    }

    /// `save name = expr`: evaluate to a component value and persist it to the
    /// session's store as `/bin/<name>.wasm` (where the store is writable).
    async fn run_save(&mut self, name: String, expr: &Expr) -> LineResult {
        // The name becomes a store entry resolvable like any installed program: keep it
        // to the dotted shell-name shape so it round-trips through `/bin/<name>.wasm`.
        let name_ok = !name.is_empty()
            && !name.starts_with('.')
            && !name.ends_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_');
        if !name_ok {
            let message = format!(
                "error: `save` refused: `{name}` is not a usable program name (letters, \
                 digits, `-`, `_`, and interior `.` only)"
            );
            self.backend.print_error(&message);
            return LineResult::Error(message);
        }
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        let component = match evaluator
            .eval_plain(expr, "a `save` command (arguments are bound at run time)")
            .await
        {
            Ok(component) => component,
            Err(err) => return self.report(err),
        };
        match self.backend.persist(&name, &component).await {
            Ok(()) => {
                self.backend
                    .print(&format!("saved: /bin/{name}.wasm (run it as `{name}`)"));
                LineResult::Ok
            }
            Err(err) => {
                let message = format!("error: `save {name}` failed: {err}");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
        }
    }

    /// `detach <name> = <expr> restart <policy>`: compose the program (with the same
    /// environment rule as a foreground run), evaluate the restart policy, and hand both
    /// to the service registry. The service then runs in the background, outliving this
    /// command — and, on hosts whose registry outlives the shell, this shell.
    async fn run_detach(&mut self, name: String, expr: &Expr, policy: &Expr) -> LineResult {
        let (can_detach, _) = self.backend.svc_grants();
        if !can_detach {
            return self.refuse_no_svc("detach");
        }

        // The same name rules as `save` (the registry enforces them too; checking here
        // gives the friendlier, earlier message).
        let name_ok = !name.is_empty()
            && !name.starts_with('.')
            && !name.ends_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_');
        if !name_ok {
            let message = format!(
                "error: `detach` refused: `{name}` is not a usable service name (letters, \
                 digits, `-`, `_`, and interior `.` only)"
            );
            self.backend.print_error(&message);
            return LineResult::Error(message);
        }

        // The program: evaluated exactly like a foreground command — applied arguments
        // become the service's `main` arguments, the kind must be a binary, missing
        // arguments are completed/refused against the signature, and the session's
        // granted environment (when there is one) is composed in. A detached service
        // runs with what *this session* could have given a foreground run, never more.
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

        // The restart policy: an expression evaluating to a policy component (its
        // configure arguments, e.g. `restart.backoff --max-restarts 5`, were applied by
        // the evaluator as compose-time configuration).
        let mut evaluator = Evaluator::new(&mut self.backend, &self.bindings);
        let policy_component = match evaluator
            .eval_plain(
                policy,
                "a restart policy (a policy takes no run-time arguments)",
            )
            .await
        {
            Ok(component) => component,
            Err(err) => return self.report(err),
        };

        match self
            .backend
            .svc_detach(component, policy_component, &name, &args)
        {
            Ok(service) => {
                self.backend.print(&format!(
                    "detached: {service} is running in the background (`svc list` to \
                     inspect, `svc log {service}` for its output, `svc stop {service}` \
                     to stop it)"
                ));
                LineResult::Ok
            }
            Err(err) => {
                let message = format!("error: `detach {name}` failed: {err}");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
        }
    }

    /// `svc` / `svc list`: the service table.
    fn run_svc_list(&mut self) -> LineResult {
        let (_, can_inspect) = self.backend.svc_grants();
        if !can_inspect {
            return self.refuse_no_svc("svc");
        }
        match self.backend.svc_list() {
            Ok(services) if services.is_empty() => {
                self.backend.print(
                    "no services (start one with `detach <name> = <program> restart <policy>`)",
                );
                LineResult::Ok
            }
            Ok(services) => {
                for line in render_service_table(&services) {
                    self.backend.print(&line);
                }
                LineResult::Ok
            }
            Err(err) => {
                let message = format!("error: `svc list` failed: {err}");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
        }
    }

    /// `svc log <name>`: the captured output.
    fn run_svc_log(&mut self, name: &str) -> LineResult {
        let (_, can_inspect) = self.backend.svc_grants();
        if !can_inspect {
            return self.refuse_no_svc("svc");
        }
        match self.backend.svc_log(name) {
            Ok(Some(log)) if log.is_empty() => {
                self.backend
                    .print(&format!("{name}: no output captured yet"));
                LineResult::Ok
            }
            Ok(Some(log)) => {
                for line in log.lines() {
                    self.backend.print(line);
                }
                LineResult::Ok
            }
            Ok(None) => {
                let message = format!(
                    "error: no captured log for `{name}` (no such service, or it was \
                     detached with logs discarded)"
                );
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
            Err(err) => {
                let message = format!("error: `svc log {name}` failed: {err}");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
        }
    }

    /// `svc stop <name>`.
    fn run_svc_stop(&mut self, name: &str) -> LineResult {
        let (_, can_inspect) = self.backend.svc_grants();
        if !can_inspect {
            return self.refuse_no_svc("svc");
        }
        match self.backend.svc_stop(name) {
            Ok(Some(outcome)) => {
                self.backend.print(&format!("stopped: {name} ({outcome})"));
                LineResult::Ok
            }
            Ok(None) => {
                let message =
                    format!("error: no service named `{name}` (`svc list` shows what exists)");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
            Err(err) => {
                let message = format!("error: `svc stop {name}` failed: {err}");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
        }
    }

    /// `svc clear <name>`.
    fn run_svc_clear(&mut self, name: &str) -> LineResult {
        let (_, can_inspect) = self.backend.svc_grants();
        if !can_inspect {
            return self.refuse_no_svc("svc");
        }
        match self.backend.svc_clear(name) {
            Ok(true) => {
                self.backend.print(&format!("cleared: {name}"));
                LineResult::Ok
            }
            Ok(false) => {
                let message = format!(
                    "error: cannot clear `{name}`: no such service, or it is still running \
                     (`svc stop {name}` first)"
                );
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
            Err(err) => {
                let message = format!("error: `svc clear {name}` failed: {err}");
                self.backend.print_error(&message);
                LineResult::Error(message)
            }
        }
    }

    /// The shared refusal when a builtin needs the svc capability and the session does
    /// not hold it.
    fn refuse_no_svc(&mut self, what: &str) -> LineResult {
        let message = format!(
            "error: `{what}` needs the eo9:svc capability, which this session does not hold \
             (services are an explicit grant — in usermode, relaunch with `eo9 --svc`, or run \
             a service config with `eo9 init`)"
        );
        self.backend.print_error(&message);
        LineResult::Error(message)
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
        if !imports_only {
            // The composition tree: how the expression was wired together (each provider
            // layer, what it satisfies or seals). `describe` of the residual surface
            // alone cannot show interposed attenuators; the wiring view does.
            let wiring = self.backend.wiring(&component);
            self.backend.print("wiring:");
            for line in wiring.lines() {
                self.backend.print(&format!("  {line}"));
            }
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
        if self.outcome_on_stderr {
            self.backend.print_error(&rendered);
        } else {
            self.backend.print(&rendered);
        }
        match outcome {
            Outcome::Success(_) => LineResult::Ok,
            Outcome::Failure(_) => LineResult::ProgramFailed(CommandClass::Failed, rendered),
            Outcome::Abnormal(AbnormalExit::Trapped(_)) => {
                LineResult::ProgramFailed(CommandClass::Trapped, rendered)
            }
            Outcome::Abnormal(AbnormalExit::Killed) => {
                LineResult::ProgramFailed(CommandClass::Killed, rendered)
            }
        }
    }

    /// Print an error and turn it into a [`LineResult`].
    fn report(&mut self, err: EvalError) -> LineResult {
        let message = format!("error: {err}");
        self.backend.print_error(&message);
        LineResult::Error(message)
    }
}

/// Render the `svc list` table: fixed-width columns, one service per line.
fn render_service_table(services: &[ServiceInfo]) -> Vec<String> {
    let mut lines = Vec::with_capacity(services.len() + 1);
    lines.push(format!(
        "{:<18} {:<16} {:>8}  {}",
        "NAME", "STATE", "RESTARTS", "OUTCOME"
    ));
    for service in services {
        lines.push(format!(
            "{:<18} {:<16} {:>8}  {}",
            service.name,
            service.state,
            service.restarts,
            service.outcome.as_deref().unwrap_or("-"),
        ));
    }
    lines
}

/// The `help` builtin's text.
pub fn help_lines() -> &'static [&'static str] {
    &[
        "eosh — the Eo9 shell. A command composes programs and runs the result.",
        "",
        "  program --flag value …        run a program with named, typed arguments",
        "                                  e.g. hello --name you",
        "  provider $ program            compose: satisfy the program's imports (right-assoc)",
        "                                  e.g. entropy.seeded --seed 7 $ rng --count 2",
        "  base & layer                  extend an environment (later layers override)",
        "                                  e.g. time.frozen & entropy.seeded --seed 7",
        "  only <iface,…> $ …            restrict everything to the right to an allow-list",
        "                                  e.g. only eo9:text,eo9:time $ hello",
        "  rename <from> <to> $ …        relabel a capability slot",
        "  with <provider> as <slot>, …  bind providers to named slots (tuples bind positionally)",
        "  let <name> = <expr>           name a component or environment value",
        "  save <name> = <expr>          persist a program or composition to /bin (where the store is writable)",
        "  detach <name> = <expr> restart <policy>",
        "                                run a program in the background as a named service, under a",
        "                                  restart policy (restart.never, restart.always, or",
        "                                  restart.backoff --max-restarts N --base-delay-ms MS);",
        "                                  needs the svc capability (eo9 --svc)",
        "  svc [list]                    list background services; svc log/stop/clear <name>",
        "  (…)                           grouping; the inner expression is passed as a value, not run",
        "",
        "explore the sandbox:",
        "  ls /bin                       list what is installed (programs and providers)",
        "  describe <name or expr>       its kind, arguments, imports, exports, and wiring",
        "  imports <expr>                just the residual imports of an expression",
        "  env                           what this session holds and what programs run from it receive",
        "  env <expr>                    how this session treats the expression's imports, without running it",
        "",
        "builtins: help, env [<expr>], history, let, save, detach, svc, describe <expr>, imports <expr>, exit",
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

    // -- detach and svc -----------------------------------------------------------

    #[test]
    fn detach_without_the_grant_is_refused_with_advice() {
        let mut session = session_with(&[("cruncher", binary(&[("rounds", "u64")]))]);
        // Default mock: no svc grants at all.
        let result = run(
            &mut session,
            "detach worker = cruncher --rounds 5 restart restart.never",
        );
        assert!(matches!(result, LineResult::Error(_)));
        let error = session.backend.err.join("\n");
        assert!(
            error.contains("eo9:svc") && error.contains("--svc"),
            "the refusal names the capability and how to get it: {error}"
        );
        // Nothing was evaluated or detached.
        assert!(session.backend.log.is_empty());
    }

    #[test]
    fn svc_builtins_without_the_grant_are_refused_with_advice() {
        let mut session = session_with(&[]);
        for line in ["svc", "svc list", "svc log x", "svc stop x", "svc clear x"] {
            let result = run(&mut session, line);
            assert!(
                matches!(result, LineResult::Error(_)),
                "`{line}` must be refused without the services grant"
            );
        }
        assert!(
            session
                .backend
                .err
                .iter()
                .all(|line| line.contains("eo9:svc")),
            "every refusal names the capability: {:?}",
            session.backend.err
        );
    }

    #[test]
    fn detach_with_the_grant_evaluates_and_hands_off() {
        let mut session = session_with(&[
            ("cruncher", binary(&[("seed", "u64"), ("rounds", "u64")])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        let result = run(
            &mut session,
            "detach worker = cruncher --seed 1 --rounds 5 restart restart.never",
        );
        assert_eq!(result, LineResult::Ok);
        // The backend saw: resolve program, describe, resolve policy, detach.
        assert!(
            session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("svc_detach(") && line.contains("worker")),
            "the handoff happened: {:?}",
            session.backend.log
        );
        // The confirmation tells the user what they can do next.
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("detached: worker") && out.contains("svc list"),
            "confirmation printed: {out}"
        );
        // The service is now visible to `svc list`.
        let result = run(&mut session, "svc list");
        assert_eq!(result, LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("worker") && out.contains("running"),
            "the table shows the service: {out}"
        );
    }

    #[test]
    fn detach_arguments_are_completed_against_the_signature() {
        // A required u64 argument that nothing fills is refused before the handoff.
        let mut session = session_with(&[
            ("cruncher", binary(&[("seed", "u64"), ("rounds", "u64")])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        let result = run(
            &mut session,
            "detach worker = cruncher restart restart.never",
        );
        assert!(matches!(result, LineResult::Error(_)));
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("svc_detach(")),
            "an incomplete invocation never reaches the registry: {:?}",
            session.backend.log
        );
    }

    #[test]
    fn detach_refuses_a_provider() {
        let mut session = session_with(&[
            ("time.frozen", provider(&["eo9:time/time"])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        let result = run(&mut session, "detach t = time.frozen restart restart.never");
        assert!(matches!(result, LineResult::Error(_)));
    }

    #[test]
    fn detach_invalid_service_names_are_refused_early() {
        let mut session = session_with(&[
            ("cruncher", binary(&[])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        let result = run(
            &mut session,
            "detach .hidden = cruncher restart restart.never",
        );
        assert!(matches!(result, LineResult::Error(_)));
        let error = session.backend.err.join("\n");
        assert!(
            error.contains("not a usable service name"),
            "the refusal explains the name rules: {error}"
        );
    }

    #[test]
    fn svc_stop_and_clear_lifecycle() {
        let mut session = session_with(&[
            ("cruncher", binary(&[])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        run(
            &mut session,
            "detach worker = cruncher restart restart.never",
        );

        // Clearing a running service is refused.
        let result = run(&mut session, "svc clear worker");
        assert!(matches!(result, LineResult::Error(_)));

        // Stop, then clear.
        assert_eq!(run(&mut session, "svc stop worker"), LineResult::Ok);
        assert_eq!(run(&mut session, "svc clear worker"), LineResult::Ok);

        // Unknown afterwards.
        let result = run(&mut session, "svc stop worker");
        assert!(matches!(result, LineResult::Error(_)));
        let error = session.backend.err.last().unwrap().clone();
        assert!(
            error.contains("no service named"),
            "stop of an unknown service says so: {error}"
        );
    }

    #[test]
    fn svc_log_reads_captured_output() {
        let mut session = session_with(&[
            ("cruncher", binary(&[])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        run(
            &mut session,
            "detach worker = cruncher restart restart.never",
        );
        assert_eq!(run(&mut session, "svc log worker"), LineResult::Ok);
        // Unknown service: a clear error.
        let result = run(&mut session, "svc log ghost");
        assert!(matches!(result, LineResult::Error(_)));
    }

    #[test]
    fn help_mentions_detach_and_svc() {
        let lines = help_lines().join("\n");
        assert!(lines.contains("detach <name>"));
        assert!(lines.contains("svc"));
        assert!(lines.contains("restart"));
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
            LineResult::ProgramFailed(
                CommandClass::Failed,
                "error: requested-failure(\"went wrong\")".to_string()
            )
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
            LineResult::ProgramFailed(
                CommandClass::Trapped,
                "abnormal: trapped: unreachable".to_string()
            )
        );

        session.backend.outcome = Outcome::Abnormal(AbnormalExit::Killed);
        let result = run(&mut session, "outcomes --mode trap");
        assert_eq!(
            result,
            LineResult::ProgramFailed(CommandClass::Killed, "abnormal: killed".to_string())
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
    fn let_confirms_what_was_bound() {
        // A successful `let` says what it bound — a silent success means the user
        // cannot tell the binding exists until they try to use it (study 10, finding 6).
        let mut session = session_with(&[
            ("time.frozen", provider(&["eo9:time/time"])),
            ("entropy.seeded", provider(&["eo9:entropy/entropy"])),
            ("hello", binary(&[])),
        ]);
        assert_eq!(run(&mut session, "let t = time.frozen"), LineResult::Ok);
        assert!(
            session
                .backend
                .out
                .iter()
                .any(|l| l == "t: bound (a provider of eo9:time/time)"),
            "out: {:?}",
            session.backend.out
        );

        // An environment built with `&` reports everything it provides.
        assert_eq!(
            run(&mut session, "let det = time.frozen & entropy.seeded"),
            LineResult::Ok
        );
        assert!(
            session
                .backend
                .out
                .iter()
                .any(|l| l.starts_with("det: bound (a provider of ")
                    && l.contains("eo9:time/time")
                    && l.contains("eo9:entropy/entropy")),
            "out: {:?}",
            session.backend.out
        );

        // Binding a program is confirmed too.
        assert_eq!(run(&mut session, "let h = hello"), LineResult::Ok);
        assert!(
            session
                .backend
                .out
                .iter()
                .any(|l| l == "h: bound (a program)"),
            "out: {:?}",
            session.backend.out
        );
    }

    #[test]
    fn let_rejects_run_time_arguments() {
        let mut session = session_with(&[("browser", binary(&[("url", "string")]))]);
        let result = run(&mut session, "let b = browser --url https://example.com");
        assert!(matches!(result, LineResult::Error(_)));
    }

    #[test]
    fn save_persists_through_the_backend_and_reports_the_path() {
        let mut session = session_with(&[
            ("entropy.seeded", provider(&["eo9:entropy/entropy"])),
            ("rng", binary(&[])),
        ]);
        assert_eq!(
            run(&mut session, "save mything = entropy.seeded $ rng"),
            LineResult::Ok
        );
        // The evaluated composition is what gets persisted, under the given name.
        assert!(
            session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("persist(mything, c")),
            "log: {:?}",
            session.backend.log
        );
        assert!(
            session
                .backend
                .out
                .iter()
                .any(|line| line.contains("/bin/mything.wasm")),
            "out: {:?}",
            session.backend.out
        );
    }

    #[test]
    fn save_reports_the_embedders_refusal() {
        let mut session = session_with(&[("rng", binary(&[]))]);
        session.backend.persist_refusal = Some("this session's store is read-only".to_string());
        let result = run(&mut session, "save mine = rng");
        assert!(matches!(result, LineResult::Error(_)));
        assert!(
            session
                .backend
                .err
                .iter()
                .any(|line| line.contains("read-only")),
            "err: {:?}",
            session.backend.err
        );
    }

    #[test]
    fn save_refuses_unusable_names_before_evaluating() {
        let mut session = session_with(&[("rng", binary(&[]))]);
        let result = run(&mut session, "save ../escape = rng");
        assert!(matches!(result, LineResult::Error(_)));
        // Refused before anything was evaluated or persisted.
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("persist(")),
            "log: {:?}",
            session.backend.log
        );
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
        // The full describe view ends with the composition tree (a single leaf here).
        assert!(session.backend.out.iter().any(|l| l == "wiring:"));
        assert!(
            session.backend.out.iter().any(|l| l == "  c1 [provider]"),
            "out: {:?}",
            session.backend.out
        );
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compile"))
        );

        session.backend.out.clear();
        assert_eq!(run(&mut session, "imports memfs"), LineResult::Ok);
        // The imports-only view stays exactly the import list (no wiring section).
        assert_eq!(session.backend.out, vec!["imports: (none)"]);
    }

    #[test]
    fn env_help_history_exit_and_empty_lines() {
        let mut session = session_with(&[]);
        assert_eq!(run(&mut session, ""), LineResult::Ok);
        assert_eq!(run(&mut session, "   # comment only"), LineResult::Ok);
        assert_eq!(run(&mut session, "env"), LineResult::Ok);
        assert_eq!(
            session.backend.out,
            vec!["no session capability information available"]
        );
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
    fn env_renders_the_session_manifest_and_bindings() {
        let mut session = session_with(&[("time.frozen", provider(&["eo9:time/time"]))]);
        session.backend.manifest = Some(
            "eo9-session 1\n\
             shell text terminal standard streams\n\
             shell exec spawn programs as children\n\
             child text terminal standard streams\n\
             note children never receive the exec capability\n"
                .to_string(),
        );
        assert_eq!(run(&mut session, "let t = time.frozen"), LineResult::Ok);
        assert_eq!(run(&mut session, "env"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(out.contains("capabilities granted to this shell:"), "{out}");
        assert!(out.contains("exec"), "{out}");
        assert!(
            out.contains("programs started from this shell receive:"),
            "{out}"
        );
        assert!(out.contains("note: children never receive"), "{out}");
        assert!(out.contains("bindings:") && out.contains("  t"), "{out}");
    }

    #[test]
    fn env_of_an_expression_marks_imports_against_the_session() {
        let mut session = session_with(&[(
            "reader",
            crate::backend::ComponentInfo {
                kind: ComponentKind::Binary,
                imports: vec![
                    crate::backend::ImportNeed {
                        slot: "eo9:text/text".to_string(),
                        interface: "eo9:text/text".to_string(),
                        version: "0.1.0".to_string(),
                        required: true,
                    },
                    crate::backend::ImportNeed {
                        slot: "eo9:fs/fs".to_string(),
                        interface: "eo9:fs/fs".to_string(),
                        version: "0.1.0".to_string(),
                        required: true,
                    },
                ],
                exports: vec![],
                args: vec![],
            },
        )]);
        session.backend.manifest = Some(
            "eo9-session 1\n\
             child text terminal standard streams\n\
             child time host clocks\n"
                .to_string(),
        );
        assert_eq!(run(&mut session, "env reader"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(out.contains("satisfied by the session (text)"), "{out}");
        assert!(out.contains("missing — would be refused at spawn"), "{out}");
        // Nothing was compiled or spawned.
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compile") || line.starts_with("spawn"))
        );
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

    /// Every example the shell's own `help` text shows must parse and evaluate cleanly.
    ///
    /// The mock programs mirror the *real* argument signatures of the components the
    /// examples name (the standard stubs' and coreutils' WIT): `hello` takes two optional
    /// arguments, `rng` a required `count`, `entropy.seeded` is configured by a `seed`,
    /// and `time.frozen` requires *both* `now-seconds` and `monotonic-ns` when configured
    /// at all (partial configuration is refused). A user study found the shipped `&`
    /// example failing exactly that rule — this test makes the class of bug (an example
    /// that the shell itself refuses) impossible to reintroduce. If a real signature
    /// changes, update the mirror here and the help text together.
    #[test]
    fn every_help_example_evaluates_cleanly() {
        use crate::backend::{ArgSpec, ImportNeed};

        let mut hello = binary(&[("name", "option<string>"), ("excited", "option<bool>")]);
        hello.imports = vec![
            ImportNeed {
                slot: "eo9:text/text".to_string(),
                interface: "eo9:text/text".to_string(),
                version: "0.1.0".to_string(),
                required: true,
            },
            ImportNeed {
                slot: "eo9:time/time".to_string(),
                interface: "eo9:time/time".to_string(),
                version: "0.1.0".to_string(),
                required: true,
            },
        ];
        let mut entropy_seeded = provider(&["eo9:entropy/entropy"]);
        entropy_seeded.args = vec![ArgSpec {
            name: "seed".to_string(),
            ty: "u64".to_string(),
        }];
        let mut time_frozen = provider(&["eo9:time/time"]);
        time_frozen.args = vec![
            ArgSpec {
                name: "now-seconds".to_string(),
                ty: "u64".to_string(),
            },
            ArgSpec {
                name: "monotonic-ns".to_string(),
                ty: "u64".to_string(),
            },
        ];
        let mut session = session_with(&[
            ("hello", hello),
            ("rng", binary(&[("count", "u64")])),
            ("entropy.seeded", entropy_seeded),
            ("time.frozen", time_frozen),
        ]);

        let mut examples = 0;
        for line in help_lines() {
            let Some(example) = line.split("e.g. ").nth(1) else {
                continue;
            };
            examples += 1;
            let result = run(&mut session, example);
            if let LineResult::Error(message) = result {
                // The one acceptable refusal: the example evaluates to a provider or
                // environment (those are meant for `let`/`$`, not to be run bare). Any
                // other error — a parse error, a missing argument, an unknown flag —
                // means the help text ships an example the shell itself refuses.
                assert!(
                    message.contains("providers are composed"),
                    "help example `{example}` fails: {message}"
                );
            }
        }
        assert!(
            examples >= 4,
            "expected the help text to carry at least four examples, found {examples}"
        );
    }
}
