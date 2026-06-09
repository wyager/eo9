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
use crate::backend::{AbnormalExit, Backend, ComponentInfo, ComponentKind, Outcome, ServiceInfo};
use crate::cache::{ArgMemoEntry, SessionCache};
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
    /// The user asked to halt the machine (`poweroff`): leave the shell AND tell the
    /// embedder — under init, plain `exit` restarts the console while services live,
    /// so halting is its own intent, reported as the shell's own typed outcome.
    Poweroff,
}

/// The typed refusal `poweroff` prints in a session whose supervisor withheld the power
/// capability (a network session: telnetd grants it only behind `--allow-poweroff`).
/// Named here so the unit tests and the embedders pin one string.
pub const POWEROFF_REFUSAL: &str = "error: poweroff: missing capability: power — this \
session's supervisor withheld machine halt (a telnet session can only poweroff when \
telnetd was started with --allow-poweroff)";

/// Ceiling on the session history (the `history` builtin's list and the editor's
/// recall source). Eviction drops the oldest entry — the GAPS "eosh session history is
/// unbounded" fix: a console session can run for the machine's whole uptime, so
/// per-line state must be bounded. The editor's recall is a further-capped (64) view
/// over this (eosh-inc's `editor::RECALL_CAP`).
pub const HISTORY_CAP: usize = 256;

/// Cap on a per-argument doc line handed to the editor's candidate list (the list
/// renders `candidate  doc` on one terminal line; the manual's own line cap is 120
/// bytes, this trims the column to fit).
const ARG_DOC_BUDGET: usize = 80;

/// One program's argument-completion hints, merged for the editor (the repl M3
/// consumer): the WIT signature is the mechanical truth — one entry per `describe`
/// ArgSpec — and the manual only ANNOTATES it (doc line, `values:` literals, `kind:`
/// tag; manual-only arguments are dropped). All manual-supplied text is sanitized
/// here (control bytes stripped, doc lines trimmed to the column budget), so a
/// hostile manual cannot inject terminal escapes through the completion menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgHints {
    pub args: Vec<ArgHint>,
}

/// One flag of [`ArgHints`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgHint {
    /// The flag name (without `--`), from the WIT signature.
    pub name: String,
    /// The WIT type text — drives the editor's typed candidates.
    pub ty: String,
    /// The manual's per-arg doc first line, sanitized and trimmed.
    pub doc: Option<String>,
    /// The manual's `values:` literals, split and sanitized — ADDITIVE candidates
    /// only (the editor's grammar keeps the free forms unconditionally).
    pub values: Vec<String>,
    /// The manual's `kind:` tag, sanitized.
    pub kind: Option<String>,
}

/// Merge one memo entry into editor-ready hints (see [`ArgHints`]).
fn merge_arg_hints(entry: &ArgMemoEntry) -> ArgHints {
    let manual_args = entry
        .manual
        .as_ref()
        .map(|manual| manual.args.as_slice())
        .unwrap_or(&[]);
    let args = entry
        .info
        .args
        .iter()
        .map(|spec| {
            let documented = manual_args.iter().find(|arg| arg.name == spec.name);
            let doc = documented
                .and_then(|arg| arg.doc.first())
                .map(|line| {
                    let mut line = crate::manual::sanitize(line);
                    if let Some((cut, _)) = line.char_indices().nth(ARG_DOC_BUDGET) {
                        line.truncate(cut);
                        line.push('…');
                    }
                    line
                })
                .filter(|line| !line.is_empty());
            let values = documented
                .and_then(|arg| arg.values.as_deref())
                .map(|values| {
                    values
                        .split(',')
                        .map(|value| crate::manual::sanitize(value.trim()))
                        .filter(|value| !value.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let kind = documented
                .and_then(|arg| arg.kind.as_deref())
                .map(|kind| crate::manual::sanitize(kind.trim()))
                .filter(|kind| !kind.is_empty());
            ArgHint {
                name: spec.name.clone(),
                ty: spec.ty.clone(),
                doc,
                values,
                kind,
            }
        })
        .collect();
    ArgHints { args }
}

/// One shell session: the backend plus everything the user has built up in it.
pub struct Session<B: Backend> {
    backend: B,
    bindings: BTreeMap<String, B::Component>,
    environment: Option<B::Component>,
    cache: SessionCache<B::Image>,
    history: Vec<String>,
    /// Where the per-command outcome line (`ok: …`/`error: …`) goes: standard output in
    /// an interactive REPL (the default), standard error in one-shot (`--command`) mode so
    /// a `-c` invocation's standard output carries only the program's own output — matching
    /// `eo9 run`, whose outcome line is on stderr by default.
    outcome_on_stderr: bool,
    /// Whether this session's supervisor withheld the power capability (the embedder
    /// calls [`Session::refuse_poweroff`]). When set, `poweroff` — and a child program's
    /// typed poweroff intent — prints [`POWEROFF_REFUSAL`] instead of ending the session,
    /// so the refusal is visible to whoever typed the command (the silent no-op cost a
    /// bench recovery round; GAPS 2026-06-08).
    power_refused: bool,
}

impl<B: Backend> Session<B> {
    pub fn new(backend: B) -> Self {
        Session {
            backend,
            bindings: BTreeMap::new(),
            environment: None,
            cache: SessionCache::new(),
            history: Vec::new(),
            outcome_on_stderr: false,
            power_refused: false,
        }
    }

    /// Route the per-command outcome line to standard error instead of standard output
    /// (used by one-shot `--command` mode so pipes carry only program output).
    pub fn route_outcome_to_stderr(&mut self) {
        self.outcome_on_stderr = true;
    }

    /// Mark this session as lacking the power capability: `poweroff` becomes a typed,
    /// printed refusal ([`POWEROFF_REFUSAL`]) and the session continues, instead of the
    /// intent flowing up as the shell's own outcome. Called by the embedding `main` when
    /// its supervisor said so (eosh's `power: option<bool>` argument bound to
    /// `some(false)` — what telnetd passes for its sessions unless `--allow-poweroff`).
    pub fn refuse_poweroff(&mut self) {
        self.power_refused = true;
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

    /// The session's `let`-binding names — one of the editor's per-prompt vocabulary
    /// sources (alongside the backend's `/bin` listing).
    pub fn binding_names(&self) -> Vec<String> {
        self.bindings.keys().cloned().collect()
    }

    /// The newest `cap` history entries, oldest first — the editor's per-prompt recall
    /// snapshot (a capped view of the already-capped session history).
    pub fn recall_view(&self, cap: usize) -> Vec<String> {
        let start = self.history.len().saturating_sub(cap);
        self.history[start..].to_vec()
    }

    /// Execute one line of input: parse, dispatch, print, and report what happened.
    pub async fn execute_line(&mut self, line: &str) -> LineResult {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if self.history.len() >= HISTORY_CAP {
                // Bounded per-line state (see [`HISTORY_CAP`]); the `history` builtin
                // then lists the surviving window, renumbered from 1 — the honest
                // rendering of a window.
                self.history.remove(0);
            }
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
            Command::Poweroff => self.run_poweroff(),
            Command::Let { name, expr } => self.run_let(name, &expr).await,
            Command::Save { name, expr } => self.run_save(name, &expr).await,
            Command::Detach { name, expr, policy } => self.run_detach(name, &expr, &policy).await,
            Command::SvcList => self.run_svc_list(),
            Command::SvcLog(name) => self.run_svc_log(&name),
            Command::SvcStop(name) => self.run_svc_stop(&name),
            Command::SvcClear(name) => self.run_svc_clear(&name),
            Command::Describe(expr) => self.run_describe(&expr, false).await,
            Command::DescribeBuiltin(word) => {
                // The parser only constructs this for words with a card.
                if let Some(doc) = crate::builtins::builtin_doc(&word) {
                    for line in crate::builtins::render_builtin_doc(doc) {
                        self.backend.print(&line);
                    }
                }
                LineResult::Ok
            }
            Command::DescribeApi(word) => self.run_describe_api(&word).await,
            Command::Man(word) => self.run_man(&word).await,
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
        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
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
        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
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
                // The binding's cache identity freezes now: a later `save` over a leaf
                // it was built from must not retroactively change what it means.
                self.cache.record_binding(&name, expr);
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
        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
        let component = match evaluator
            .eval_plain(expr, "a `save` command (arguments are bound at run time)")
            .await
        {
            Ok(component) => component,
            Err(err) => return self.report(err),
        };
        match self.backend.persist(&name, &component).await {
            Ok(()) => {
                // `/bin/<name>.wasm` changed: keys built from that leaf must miss, and
                // its cached bytes are stale. Unrelated entries are untouched.
                self.cache.note_bin_write(&name);
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
        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
        let output = match evaluator.eval(expr).await {
            Ok(output) => output,
            Err(err) => return self.report(err),
        };
        let mut component = output.component;
        let mut args = output.args;
        if !output.components.is_empty() {
            let message = "error: a detached service cannot carry component-typed \
                           arguments yet (services respawn from their image, but a \
                           component argument is consumed by the spawn that binds it)";
            self.backend.print_error(message);
            return LineResult::Error(String::from(message));
        }

        let info = self.backend.describe(&component);
        if info.kind == ComponentKind::Provider {
            return self.report(EvalError::TopLevelProvider);
        }
        if let Err(err) = complete_args(&mut args, &[], &info.args) {
            return self.report(err);
        }
        let child_imports_fs = imports_fs(&info);
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
        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
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
                if child_imports_fs {
                    // The service is a *concurrent* potential `/bin` writer for the rest
                    // of the session: no point-in-time invalidation is sound, so the
                    // resolve cache turns itself off.
                    self.cache.disable();
                }
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
        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
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

    /// `describe eo9:pci` / `describe eo9:pci/pci`: the OS API cards. The static part
    /// comes from the build-time WIT-doc extraction (`apidocs`); the live part scans
    /// `/bin` through the backend so the card also says who, in *this* store, exports
    /// or imports the thing being described. Unknown API names get the package
    /// inventory instead of a resolution error.
    async fn run_describe_api(&mut self, word: &str) -> LineResult {
        use crate::apidocs::{self, ApiDoc};
        let Some(doc) = apidocs::api_doc(word) else {
            let message = format!(
                "error: no OS API named `{word}` — packages: {} (describe one, or an interface like eo9:fs/fs)",
                apidocs::package_names().join(", ")
            );
            self.backend.print_error(&message);
            return LineResult::Error(message);
        };
        let (lines, target) = match &doc {
            ApiDoc::Package(doc) => (apidocs::render_package(doc), format!("{}/", doc.name)),
            ApiDoc::Interface(doc) => (
                apidocs::render_interface(doc),
                format!("{}/{}", doc.package, doc.name),
            ),
        };
        for line in &lines {
            self.backend.print(line);
        }
        // The live section: who in /bin exports or imports the described surface. A
        // package matches any of its interfaces (the trailing-slash prefix); an
        // interface matches exactly. Entries that fail to open are skipped — a broken
        // store file should not break describing an API.
        let names = self.backend.list_bin().await;
        if names.is_empty() {
            return LineResult::Ok;
        }
        let matches_target = |interface: &str| match target.ends_with('/') {
            true => interface.starts_with(target.as_str()),
            false => interface == target,
        };
        let mut exporters: Vec<String> = Vec::new();
        let mut importers: Vec<String> = Vec::new();
        for name in names {
            let Ok(component) = self.backend.resolve(&name).await else {
                continue;
            };
            let info = self.backend.describe(&component);
            if info.exports.iter().any(|e| matches_target(&e.interface)) {
                exporters.push(name.clone());
            } else if info.imports.iter().any(|i| matches_target(&i.interface)) {
                importers.push(name.clone());
            }
        }
        self.backend.print("in this store:");
        if exporters.is_empty() && importers.is_empty() {
            self.backend
                .print("  nothing in /bin imports or exports it yet");
        }
        if !exporters.is_empty() {
            for line in wrap_names("  exported by: ", &exporters) {
                self.backend.print(&line);
            }
        }
        if !importers.is_empty() {
            for line in wrap_names("  imported by: ", &importers) {
                self.backend.print(&line);
            }
        }
        LineResult::Ok
    }

    /// `man <name>`: render the named artifact's self-described manual (the
    /// `eo9-manual` custom section — docs/design/component-manuals.md). The dispatch
    /// ladder: a shell word's builtin/operator card, an OS API card, the manual found
    /// in the raw bytes of `/bin/<name>.wasm`, then the honest fallbacks — "no manual"
    /// or "manual malformed", each followed by `describe`'s mechanical view. Display
    /// only: nothing here is evaluated, composed, compiled, or run, and the manual
    /// text never feeds resolution or caching.
    async fn run_man(&mut self, word: &str) -> LineResult {
        // A shell word: the same card `describe <word>` renders.
        if let Some(doc) = crate::builtins::builtin_doc(word) {
            for line in crate::builtins::render_builtin_doc(doc) {
                self.backend.print(&line);
            }
            return LineResult::Ok;
        }
        // A colon marks an OS API name (program names cannot carry one).
        if word.contains(':') {
            return self.run_describe_api(word).await;
        }
        // The NAMED artifact in /bin only — no expression evaluation, no part-chasing,
        // no `let` bindings (a binding is a value, not a /bin artifact; say so rather
        // than "cannot resolve" when the name is only a binding). Bytes come from the
        // session bytes cache when the name was read before, the backend otherwise —
        // a byte scan of bytes the session mostly already holds, never an instantiation.
        let Session {
            backend,
            cache,
            bindings,
            ..
        } = self;
        let mut resolved: Option<(B::Component, Option<Vec<u8>>)> = None;
        if let Some(bytes) = cache.bytes.get(word) {
            match backend.load(bytes) {
                Ok(component) => {
                    let bytes = bytes.to_vec();
                    resolved = Some((component, Some(bytes)));
                }
                Err(err) => return self.report(EvalError::Backend(err)),
            }
        }
        let (component, bytes) = match resolved {
            Some(resolved) => resolved,
            None => match backend.resolve_with_bytes(word).await {
                Ok((component, bytes)) => {
                    if let Some(bytes) = &bytes {
                        cache.bytes.insert(word, bytes.clone());
                    }
                    (component, bytes)
                }
                Err(err) => {
                    if let Some(binding) = bindings.get(word) {
                        let info = backend.describe(binding);
                        backend.print(&format!(
                            "no manual for `{word}` (a session `let` binding); showing describe"
                        ));
                        for line in render_info(&info) {
                            backend.print(&line);
                        }
                        return LineResult::Ok;
                    }
                    return self.report(EvalError::Backend(err));
                }
            },
        };
        let info = self.backend.describe(&component);
        match bytes.as_deref().and_then(crate::manual::extract_manual) {
            Some(payload) => match crate::manual::parse_manual(payload) {
                Ok(manual) => {
                    for line in crate::manual::render_manual(&manual, Some(&info.args)) {
                        self.backend.print(&line);
                    }
                    LineResult::Ok
                }
                Err(err) => {
                    self.backend
                        .print(&format!("manual malformed ({err}); showing describe"));
                    for line in render_info(&info) {
                        self.backend.print(&line);
                    }
                    LineResult::Ok
                }
            },
            None => {
                self.backend
                    .print(&format!("no manual for `{word}`; showing describe"));
                for line in render_info(&info) {
                    self.backend.print(&line);
                }
                LineResult::Ok
            }
        }
    }

    /// The editor's argument-completion source (repl M3): the named /bin program's
    /// argument hints — `describe`'s signature annotated by the component's manual —
    /// memoized per resolved name in the session cache (the lazy memo,
    /// docs/design/component-manuals.md §4) and invalidated by the bytes cache's
    /// structural rules (`save` drops the name, an fs-importing run clears all, a
    /// concurrent writer disables). `None` when the name does not resolve (the editor
    /// keeps the generic grammar). Display/completion only: nothing here is composed,
    /// compiled, or run, and a manual can only ADD candidates, never restrict
    /// (enforced grammar-side in eosh-inc).
    pub async fn arg_hints(&mut self, name: &str) -> Option<ArgHints> {
        if let Some(entry) = self.cache.args_get(name) {
            return Some(merge_arg_hints(entry));
        }
        // Resolve the bytes like `man` does: the session bytes cache first, the
        // backend otherwise (caching what it hands back).
        let Session { backend, cache, .. } = self;
        let mut resolved: Option<(B::Component, Option<Vec<u8>>)> = None;
        if let Some(bytes) = cache.bytes.get(name) {
            if let Ok(component) = backend.load(bytes) {
                let bytes = bytes.to_vec();
                resolved = Some((component, Some(bytes)));
            }
        }
        let (component, bytes) = match resolved {
            Some(resolved) => resolved,
            None => match backend.resolve_with_bytes(name).await {
                Ok((component, bytes)) => {
                    if let Some(bytes) = &bytes {
                        cache.bytes.insert(name, bytes.clone());
                    }
                    (component, bytes)
                }
                // Unresolvable (or a binding-only name): no hints, no memo — the
                // editor falls back to the generic grammar.
                Err(_) => return None,
            },
        };
        let info = self.backend.describe(&component);
        let manual = bytes
            .as_deref()
            .and_then(crate::manual::extract_manual)
            .and_then(|payload| crate::manual::parse_manual(payload).ok());
        let entry = ArgMemoEntry { info, manual };
        let hints = merge_arg_hints(&entry);
        self.cache.args_insert(name, entry);
        Some(hints)
    }

    /// The top-level rule: compose the granted environment onto the command, compile,
    /// spawn with the bound arguments, await the outcome, print it.
    ///
    /// A structurally identical run this session already compiled (the session resolve
    /// cache — see [`crate::cache`]) skips resolution, the algebra, and `compile`
    /// entirely: the cached image spawns again with the arguments bound last time.
    async fn run_program(&mut self, expr: &Expr) -> LineResult {
        let key = self.cache.run_key(expr, self.environment.is_some());

        if let Some(key) = key.as_deref() {
            let Session { backend, cache, .. } = self;
            let hit = cache
                .image_get(key)
                .map(|(image, args, fs_run)| (backend.spawn(image, args, Vec::new()), fs_run));
            if let Some((spawned, fs_run)) = hit {
                let task = match spawned {
                    Ok(task) => task,
                    Err(err) => return self.report(EvalError::Backend(err)),
                };
                let outcome = self.backend.wait(task).await;
                if fs_run {
                    self.cache.note_fs_run();
                }
                return self.finish_run(outcome);
            }
        }

        let mut evaluator =
            Evaluator::with_cache(&mut self.backend, &self.bindings, &mut self.cache.bytes);
        let output = match evaluator.eval(expr).await {
            Ok(output) => output,
            Err(err) => return self.report(err),
        };
        let mut component = output.component;
        let mut args = output.args;
        let components = output.components;

        let info = self.backend.describe(&component);
        if info.kind == ComponentKind::Provider {
            return self.report(EvalError::TopLevelProvider);
        }
        let filled_components: Vec<String> =
            components.iter().map(|arg| arg.name.clone()).collect();
        if let Err(err) = complete_args(&mut args, &filled_components, &info.args) {
            return self.report(err);
        }
        let fs_run = imports_fs(&info);

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
        // Component arguments are owned values, consumed by the spawn that binds them —
        // a line that carries one can never take the cached-image fast path, so it is
        // never remembered (the honesty rule twice over: the argument's referent may
        // also have changed by the next structurally identical line).
        let cacheable = components.is_empty();
        let task = match self.backend.spawn(&image, &args, components) {
            Ok(task) => task,
            Err(err) => return self.report(EvalError::Backend(err)),
        };
        // The image is good (it spawned): remember it for the next structurally
        // identical line, whatever the program's own outcome turns out to be.
        if let (Some(key), true) = (key, cacheable) {
            self.cache.image_insert(key, image, args, fs_run);
        }
        let outcome = self.backend.wait(task).await;
        if fs_run {
            // The program holds the filesystem capability, so it *could* have rewritten
            // `/bin`; nothing tells us whether it did. Honesty rule: re-resolve.
            self.cache.note_fs_run();
        }
        self.finish_run(outcome)
    }

    /// The `poweroff` builtin: end the session with the typed halt intent — unless this
    /// session's supervisor withheld the power capability, in which case the refusal is
    /// printed (never silent) and the session continues.
    fn run_poweroff(&mut self) -> LineResult {
        if self.power_refused {
            self.backend.print_error(POWEROFF_REFUSAL);
            return LineResult::Error(String::from(POWEROFF_REFUSAL));
        }
        LineResult::Poweroff
    }

    /// Render and classify a finished run's outcome (shared by the cached-image fast
    /// path and the full path).
    fn finish_run(&mut self, outcome: Outcome) -> LineResult {
        let rendered = render_outcome(&outcome);
        if self.outcome_on_stderr {
            self.backend.print_error(&rendered);
        } else {
            self.backend.print(&rendered);
        }
        match outcome {
            // A child program's own typed poweroff intent (the convention init and
            // telnetd already match on): the intent flows up the supervision tree, so a
            // shell that holds power forwards it as its own — `telnetd --allow-poweroff`
            // ending with poweroff-requested at the console must reach init — and a
            // shell whose supervisor withheld power refuses it exactly like the typed
            // builtin (visibly, never a silent swallow).
            Outcome::Success(value) if value.value == "poweroff-requested" => self.run_poweroff(),
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

/// Whether a program's residual imports include the filesystem (required or optional):
/// the conservative signal that running it could rewrite `/bin` under the session's
/// resolve cache. The filesystem API has no read/write split at the interface level, so
/// read-only users bump too — correctness over cleverness.
fn imports_fs(info: &ComponentInfo) -> bool {
    info.imports
        .iter()
        .any(|need| need.interface.starts_with("eo9:fs/"))
}

/// Wrap a `prefix: a, b, c` name list at the page column budget, continuation lines
/// indented to the prefix.
fn wrap_names(prefix: &str, names: &[String]) -> Vec<String> {
    const COLUMN_BUDGET: usize = 109;
    let indent = " ".repeat(prefix.chars().count());
    let mut lines = Vec::new();
    let mut line = String::from(prefix);
    for (index, name) in names.iter().enumerate() {
        let piece = if index == 0 {
            name.clone()
        } else {
            format!(", {name}")
        };
        if line.chars().count() + piece.chars().count() > COLUMN_BUDGET
            && line.chars().count() > indent.chars().count()
        {
            line.push(',');
            lines.push(core::mem::take(&mut line));
            line = format!("{indent}{name}");
        } else {
            line.push_str(&piece);
        }
    }
    lines.push(line);
    lines
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
        "  describe <name or expr>       its kind, arguments, imports, exports, and wiring; also",
        "                                  explains the shell's own words (describe describe) and the",
        "                                  OS APIs themselves — e.g. describe eo9:fs/fs",
        "  man <name>                    a program's own manual page (falls back to describe)",
        "  imports <expr>                just the residual imports of an expression",
        "  env                           what this session holds and what programs run from it receive",
        "  env <expr>                    how this session treats the expression's imports, without running it",
        "",
        "builtins: help, env [<expr>], history, let, save, detach, svc, describe <expr>, man <name>, imports <expr>, exit, poweroff (halt the machine — under init, exit only restarts the console)",
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
    fn component_arguments_spawn_alongside_data_arguments_and_are_never_cached() {
        let mut session = session_with(&[
            (
                "timeit",
                binary(&[("prog", "component"), ("verbose", "option<bool>")]),
            ),
            ("hello", binary(&[])),
        ]);
        // `timeit hello`: the positional word fills the component-typed parameter as a
        // program expression; the spawn carries it as a live value.
        let result = run(&mut session, "timeit hello");
        assert_eq!(result, LineResult::Ok);
        let spawn = session
            .backend
            .log
            .iter()
            .find(|line| line.starts_with("spawn"))
            .expect("a spawn happened")
            .clone();
        assert!(spawn.contains("prog=c"), "component arg missing: {spawn}");
        assert!(
            spawn.contains("verbose=none"),
            "option default missing: {spawn}"
        );

        // A structurally identical second line re-resolves and re-spawns in full: lines
        // carrying component arguments never take the cached-image fast path (the value
        // is consumed by the spawn, and its referent may have changed).
        session.backend.log.clear();
        let result = run(&mut session, "timeit hello");
        assert_eq!(result, LineResult::Ok);
        let compiles = session
            .backend
            .log
            .iter()
            .filter(|line| line.starts_with("compile"))
            .count();
        assert_eq!(
            compiles, 1,
            "expected a full re-run, got {:?}",
            session.backend.log
        );
    }

    #[test]
    fn detach_refuses_component_arguments() {
        let mut session = session_with(&[
            ("timeit", binary(&[("prog", "component")])),
            ("hello", binary(&[])),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        let result = run(
            &mut session,
            "detach t = timeit hello restart restart.never",
        );
        assert!(matches!(result, LineResult::Error(_)));
        let err = session.backend.err.join("\n");
        assert!(err.contains("component-typed"), "got: {err}");
    }

    // -- poweroff and the power capability ------------------------------------------

    #[test]
    fn poweroff_without_the_power_capability_is_a_typed_printed_refusal() {
        let mut session = session_with(&[]);
        session.refuse_poweroff();
        let result = run(&mut session, "poweroff");
        // Typed, printed, names the missing capability — and the session continues
        // (the silent no-op cost a bench recovery round; GAPS 2026-06-08).
        assert_eq!(result, LineResult::Error(POWEROFF_REFUSAL.to_string()));
        let err = session.backend.err.join("\n");
        assert!(err.contains("missing capability: power"), "got: {err}");
        assert!(err.contains("--allow-poweroff"), "got: {err}");
        // The session is still usable afterwards.
        assert_eq!(run(&mut session, "help"), LineResult::Ok);
    }

    #[test]
    fn a_childs_typed_poweroff_intent_is_forwarded_when_power_is_held() {
        // The intent flows up the supervision tree: a program ending with the typed
        // poweroff-requested outcome (telnetd --allow-poweroff honoring a remote
        // poweroff) ends this session with the shell's own halt intent.
        let mut session = session_with(&[("telnetd", binary(&[]))]);
        session.backend.outcome = Outcome::Success(WaveValue {
            ty: "program-success".to_string(),
            value: "poweroff-requested".to_string(),
        });
        let result = run(&mut session, "telnetd");
        assert_eq!(result, LineResult::Poweroff);
        // The outcome line still printed before the forward.
        assert_eq!(session.backend.out, vec!["ok: poweroff-requested"]);
    }

    #[test]
    fn a_childs_typed_poweroff_intent_is_refused_like_the_builtin_without_power() {
        let mut session = session_with(&[("telnetd", binary(&[]))]);
        session.refuse_poweroff();
        session.backend.outcome = Outcome::Success(WaveValue {
            ty: "program-success".to_string(),
            value: "poweroff-requested".to_string(),
        });
        let result = run(&mut session, "telnetd");
        assert_eq!(result, LineResult::Error(POWEROFF_REFUSAL.to_string()));
        let err = session.backend.err.join("\n");
        assert!(err.contains("missing capability: power"), "got: {err}");
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
        // Use it twice. The first use duplicates the stored value rather than
        // consuming it; the second is a resolve-cache hit (same structural key), which
        // re-spawns the cached image without touching the binding at all — the binding
        // demonstrably survives both.
        assert_eq!(run(&mut session, "det-env $ app"), LineResult::Ok);
        assert_eq!(run(&mut session, "det-env $ app"), LineResult::Ok);
        let duplicates = session
            .backend
            .log
            .iter()
            .filter(|line| line.starts_with("duplicate"))
            .count();
        assert_eq!(duplicates, 1);
        let spawns = session
            .backend
            .log
            .iter()
            .filter(|line| line.starts_with("spawn"))
            .count();
        assert_eq!(spawns, 2);
    }

    /// A binary whose residual imports include the filesystem (the conservative
    /// could-rewrite-/bin signal).
    fn fs_binary() -> crate::backend::ComponentInfo {
        let mut info = binary(&[]);
        info.imports.push(crate::backend::ImportNeed {
            slot: "eo9:fs/fs".to_string(),
            interface: "eo9:fs/fs".to_string(),
            version: "0.1.0".to_string(),
            required: true,
        });
        info
    }

    #[test]
    fn a_repeated_line_respawns_the_cached_image_and_touches_nothing_else() {
        let mut session = session_with(&[
            ("gpu.virtio", provider(&["eo9:gfx/gfx"])),
            ("draw", binary(&[])),
        ]);
        assert_eq!(run(&mut session, "gpu.virtio $ draw"), LineResult::Ok);
        let first_len = session.backend.log.len();
        // A different spelling of the same structure: extra whitespace and parens.
        assert_eq!(run(&mut session, "gpu.virtio  $  (draw)"), LineResult::Ok);
        let second: Vec<&String> = session.backend.log[first_len..].iter().collect();
        assert_eq!(second.len(), 2, "expected spawn+wait only, got {second:?}");
        assert!(second[0].starts_with("spawn(i1"), "got {second:?}");
        assert!(second[1].starts_with("wait"), "got {second:?}");
    }

    #[test]
    fn changed_arguments_miss_and_rebuild() {
        let mut session = session_with(&[("hello", binary(&[("name", "string")]))]);
        assert_eq!(run(&mut session, "hello --name a"), LineResult::Ok);
        let first_len = session.backend.log.len();
        assert_eq!(run(&mut session, "hello --name b"), LineResult::Ok);
        // Different argument value = different structural key: the line re-evaluates
        // (v1 keys include arguments; argument-stripped sharing is the recorded
        // follow-up) — and the spawn carries the new value.
        assert!(
            session.backend.log[first_len..]
                .iter()
                .any(|line| line.contains("name=\"b\"") && line.starts_with("spawn")),
            "log: {:?}",
            &session.backend.log[first_len..]
        );
    }

    #[test]
    fn save_invalidates_runs_built_from_that_name_only() {
        let mut session = session_with(&[("x", binary(&[])), ("y", binary(&[]))]);
        assert_eq!(run(&mut session, "x"), LineResult::Ok);
        assert_eq!(run(&mut session, "y"), LineResult::Ok);
        assert_eq!(run(&mut session, "save x = y"), LineResult::Ok);
        let len = session.backend.log.len();
        // `x` was rewritten: its key moved, the line re-resolves.
        assert_eq!(run(&mut session, "x"), LineResult::Ok);
        assert!(
            session.backend.log[len..]
                .iter()
                .any(|line| line.starts_with("resolve(x)") || line.starts_with("load(x)")),
            "log: {:?}",
            &session.backend.log[len..]
        );
        // `y` was not: its cached image respawns untouched.
        let len = session.backend.log.len();
        assert_eq!(run(&mut session, "y"), LineResult::Ok);
        let tail: Vec<&String> = session.backend.log[len..].iter().collect();
        assert_eq!(tail.len(), 2, "expected spawn+wait only, got {tail:?}");
    }

    #[test]
    fn fs_importing_runs_invalidate_bin_keys() {
        let mut session = session_with(&[("hello", binary(&[])), ("rm", fs_binary())]);
        assert_eq!(run(&mut session, "hello"), LineResult::Ok);
        // `rm` holds the filesystem capability: it could have rewritten /bin, and the
        // filesystem cannot tell us whether it did.
        assert_eq!(run(&mut session, "rm"), LineResult::Ok);
        let len = session.backend.log.len();
        assert_eq!(run(&mut session, "hello"), LineResult::Ok);
        assert!(
            session.backend.log[len..]
                .iter()
                .any(|line| line.starts_with("resolve(hello)")),
            "log: {:?}",
            &session.backend.log[len..]
        );
    }

    #[test]
    fn an_unrelated_let_evicts_nothing() {
        let mut session = session_with(&[
            ("hello", binary(&[])),
            ("time.frozen", provider(&["eo9:time/time"])),
        ]);
        assert_eq!(run(&mut session, "hello"), LineResult::Ok);
        assert_eq!(run(&mut session, "let t = time.frozen"), LineResult::Ok);
        let len = session.backend.log.len();
        assert_eq!(run(&mut session, "hello"), LineResult::Ok);
        let tail: Vec<&String> = session.backend.log[len..].iter().collect();
        assert_eq!(tail.len(), 2, "expected spawn+wait only, got {tail:?}");
    }

    #[test]
    fn partial_reuse_loads_cached_bytes_instead_of_re_reading() {
        let mut session = session_with(&[
            ("time.frozen", provider(&["eo9:time/time"])),
            ("a", binary(&[])),
            ("b", binary(&[])),
        ]);
        assert_eq!(run(&mut session, "time.frozen $ a"), LineResult::Ok);
        let len = session.backend.log.len();
        // A different line sharing a leaf: the image cache misses, but the shared
        // leaf's bytes are already in the session — `load`, not a filesystem re-read.
        assert_eq!(run(&mut session, "time.frozen $ b"), LineResult::Ok);
        let tail: Vec<&String> = session.backend.log[len..].iter().collect();
        assert!(
            tail.iter()
                .any(|line| line.starts_with("load(time.frozen)")),
            "log: {tail:?}"
        );
        assert!(
            !tail
                .iter()
                .any(|line| line.starts_with("resolve(time.frozen)")),
            "log: {tail:?}"
        );
    }

    #[test]
    fn detaching_an_fs_importing_child_disables_the_cache() {
        let mut session = session_with(&[
            ("hello", binary(&[])),
            ("worker", fs_binary()),
            ("restart.never", provider(&["eo9:svc/restart-policy"])),
        ]);
        session.backend.svc_grants = (true, true);
        assert_eq!(run(&mut session, "hello"), LineResult::Ok);
        assert_eq!(
            run(&mut session, "detach w = worker restart restart.never"),
            LineResult::Ok
        );
        // The service is a concurrent potential /bin writer: caching is off for the
        // rest of the session, so even a previously cached line re-resolves.
        let len = session.backend.log.len();
        assert_eq!(run(&mut session, "hello"), LineResult::Ok);
        assert!(
            session.backend.log[len..]
                .iter()
                .any(|line| line.starts_with("resolve(hello)")),
            "log: {:?}",
            &session.backend.log[len..]
        );
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
    fn describe_works_on_builtins_including_itself() {
        let mut session = session_with(&[]);
        assert_eq!(run(&mut session, "describe describe"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("kind: builtin") && out.contains("e.g. describe entropy.seeded"),
            "describe describe renders its own card: {out}"
        );
        // Nothing was resolved, evaluated, or compiled.
        assert!(
            session.backend.log.is_empty(),
            "log: {:?}",
            session.backend.log
        );

        for word in [
            "help", "let", "save", "detach", "svc", "env", "imports", "exit", "only",
        ] {
            session.backend.out.clear();
            assert_eq!(
                run(&mut session, &format!("describe {word}")),
                LineResult::Ok,
                "describe {word}"
            );
            assert!(
                session.backend.out.iter().any(|l| l.starts_with("kind: ")),
                "describe {word} renders a card: {:?}",
                session.backend.out
            );
        }
    }

    #[test]
    fn describe_works_on_the_operators() {
        let mut session = session_with(&[]);
        for (word, expect) in [("$", "Composition"), ("&", "Environment extension")] {
            session.backend.out.clear();
            assert_eq!(
                run(&mut session, &format!("describe {word}")),
                LineResult::Ok,
                "describe {word}"
            );
            let out = session.backend.out.join("\n");
            assert!(
                out.contains("kind: operator") && out.contains(expect),
                "describe {word}: {out}"
            );
        }
    }

    #[test]
    fn describe_works_on_api_packages_and_interfaces() {
        use crate::backend::{ComponentKind, ImportNeed};
        let importer = crate::backend::ComponentInfo {
            kind: ComponentKind::Binary,
            imports: alloc::vec![ImportNeed {
                slot: String::from("eo9:fs/fs"),
                interface: String::from("eo9:fs/fs"),
                version: String::from("0.1.0"),
                required: true,
            }],
            exports: Vec::new(),
            args: Vec::new(),
        };
        let mut session = session_with(&[("cat", importer), ("memfs", provider(&["eo9:fs/fs"]))]);

        assert_eq!(run(&mut session, "describe eo9:fs"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("kind: OS API package") && out.contains("package: eo9:fs@"),
            "package card renders: {out}"
        );
        assert!(
            out.contains("eo9:fs/fs —"),
            "the package card lists its interfaces: {out}"
        );
        assert!(
            out.contains("in this store:")
                && out.contains("exported by: memfs")
                && out.contains("imported by: cat"),
            "the live store section cross-references /bin: {out}"
        );

        session.backend.out.clear();
        assert_eq!(run(&mut session, "describe eo9:fs/fs"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("kind: OS API interface") && out.contains("interface: eo9:fs/fs@"),
            "interface card renders: {out}"
        );
        assert!(
            out.contains("functions:"),
            "the interface card lists functions: {out}"
        );
        assert!(
            out.contains("exported by: memfs") && out.contains("imported by: cat"),
            "the interface card's live section matches exactly: {out}"
        );
    }

    #[test]
    fn describe_of_an_unknown_api_lists_the_packages() {
        let mut session = session_with(&[]);
        let result = run(&mut session, "describe eo9:nope");
        assert!(matches!(result, LineResult::Error(_)));
        let err = session.backend.err.join("\n");
        assert!(
            err.contains("no OS API named `eo9:nope`") && err.contains("eo9:fs"),
            "the not-found message lists the package inventory: {err}"
        );
    }

    #[test]
    fn describe_of_a_program_named_like_nothing_builtin_still_uses_the_backend() {
        // `describe <single non-shell word>` keeps the expression path: resolution,
        // backend.describe, the wiring tree.
        let mut session = session_with(&[("memfs", provider(&["eo9:fs/fs"]))]);
        assert_eq!(run(&mut session, "describe memfs"), LineResult::Ok);
        assert!(session.backend.out.iter().any(|l| l == "kind: provider"));
        // And a parenthesized shell word forces the expression path too (resolution
        // failure, not a card) — the escape hatch if a store ever shipped such a name.
        session.backend.out.clear();
        let result = run(&mut session, "describe (help)");
        assert!(matches!(result, LineResult::Error(_)), "got: {result:?}");
    }

    // -- man ------------------------------------------------------------------------

    /// A manual whose args match [`telnetd_like`]'s signature.
    const MAN_TEXT: &str = "eo9-manual 1\n\
        name: telnetd\n\
        synopsis: serve eosh sessions over telnet, one fused task per session\n\
        description:\n\
        \x20 Composes the session stack and serves sessions sequentially.\n\
        arg port u16 optional\n\
        \x20 doc: TCP port to listen on (default 23)\n\
        \x20 kind: port\n\
        arg address string optional\n\
        \x20 doc: IPv4 acquisition mode\n\
        \x20 values: dhcp\n\
        example: telnetd --port 2323\n\
        \x20 doc: serve on a non-privileged port\n\
        see-also: net.l4.over-l2, net.text, eosh\n\
        end\n";

    fn telnetd_like() -> crate::backend::ComponentInfo {
        binary(&[("port", "option<u16>"), ("address", "option<string>")])
    }

    #[test]
    fn man_renders_an_embedded_manual_without_running_anything() {
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "telnetd",
            telnetd_like(),
            crate::manual::fixtures::component_with_manual(MAN_TEXT),
        );
        assert_eq!(run(&mut session, "man telnetd"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.starts_with("telnetd — serve eosh sessions over telnet"),
            "the manual rendered: {out}"
        );
        assert!(out.contains("--port: u16 (optional)"), "{out}");
        assert!(out.contains("values: dhcp"), "{out}");
        assert!(out.contains("telnetd --port 2323"), "{out}");
        assert!(out.contains("see-also: net.l4.over-l2"), "{out}");
        // The signature agrees, so nothing is flagged.
        assert!(!out.contains("(!)"), "{out}");
        // Display only: resolve + describe, never compose/compile/spawn.
        assert!(
            !session
                .backend
                .log
                .iter()
                .any(|line| line.starts_with("compile")
                    || line.starts_with("spawn")
                    || line.starts_with("compose")),
            "log: {:?}",
            session.backend.log
        );
    }

    #[test]
    fn man_flags_a_manual_that_disagrees_with_the_signature() {
        // The program's real signature lacks `address` and types port differently: the
        // manual is self-reported, so the disagreement is flagged, not trusted.
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "telnetd",
            binary(&[("port", "u32")]),
            crate::manual::fixtures::component_with_manual(MAN_TEXT),
        );
        assert_eq!(run(&mut session, "man telnetd"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("(!) the program declares `u32`"),
            "type mismatch flagged: {out}"
        );
        assert!(
            out.contains("(!) the program declares no `--address` argument"),
            "unknown argument flagged: {out}"
        );
    }

    #[test]
    fn man_without_a_manual_falls_back_to_describe() {
        let mut session = session_with(&[("hello", binary(&[("name", "option<string>")]))]);
        assert_eq!(run(&mut session, "man hello"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("no manual for `hello`; showing describe"),
            "{out}"
        );
        assert!(
            out.contains("kind: binary"),
            "the describe view follows: {out}"
        );
        assert!(out.contains("--name: option<string>"), "{out}");
    }

    // -- arg_hints (the repl M3 lazy memo) -------------------------------------------

    #[test]
    fn arg_hints_merge_describe_with_the_manual_and_memoize() {
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "telnetd",
            telnetd_like(),
            crate::manual::fixtures::component_with_manual(MAN_TEXT),
        );
        let hints =
            block_on_ready(session.arg_hints("telnetd")).expect("resolvable program has hints");
        // One hint per WIT ArgSpec, in signature order; the manual annotates.
        assert_eq!(hints.args.len(), 2);
        assert_eq!(hints.args[0].name, "port");
        assert_eq!(hints.args[0].ty, "option<u16>");
        assert_eq!(
            hints.args[0].doc.as_deref(),
            Some("TCP port to listen on (default 23)")
        );
        assert_eq!(hints.args[0].kind.as_deref(), Some("port"));
        assert!(hints.args[0].values.is_empty());
        assert_eq!(hints.args[1].name, "address");
        assert_eq!(hints.args[1].values, vec!["dhcp".to_string()]);
        // The memo: a second ask answers from the cache — no new resolve/describe.
        let resolves = |log: &[String]| {
            log.iter()
                .filter(|line| line.starts_with("resolve") || line.starts_with("describe"))
                .count()
        };
        let before = resolves(&session.backend.log);
        let again = block_on_ready(session.arg_hints("telnetd")).expect("memo hit");
        assert_eq!(again, hints);
        assert_eq!(resolves(&session.backend.log), before, "memo hit re-resolved");
    }

    #[test]
    fn arg_hints_without_a_manual_are_the_bare_signature() {
        let mut session = session_with(&[("hello", binary(&[("name", "option<string>")]))]);
        let hints = block_on_ready(session.arg_hints("hello")).expect("hints");
        assert_eq!(hints.args.len(), 1);
        assert_eq!(hints.args[0].name, "name");
        assert_eq!(hints.args[0].doc, None);
        assert!(hints.args[0].values.is_empty());
        assert_eq!(hints.args[0].kind, None);
    }

    #[test]
    fn arg_hints_for_an_unresolvable_name_are_none() {
        let mut session = session_with(&[]);
        assert_eq!(block_on_ready(session.arg_hints("ghost")), None);
        // A `let` binding is not a /bin artifact either (the memo is keyed by
        // resolved program name; the editor only asks for Program-tagged words).
        let mut session = session_with(&[("rng", binary(&[]))]);
        assert_eq!(run(&mut session, "let mine = rng"), LineResult::Ok);
        assert_eq!(block_on_ready(session.arg_hints("mine")), None);
    }

    #[test]
    fn arg_hints_drop_manual_only_arguments_and_sanitize_text() {
        // The manual documents an argument the program does not declare (dropped: the
        // WIT signature is the truth) and carries control bytes in its doc/values
        // (stripped: the text reaches the editor's candidate list).
        let manual = "eo9-manual 1\n\
            name: evil\n\
            synopsis: s\n\
            arg real string optional\n\
            \x20 doc: ok\x1b[31m doc\n\
            \x20 values: a\x07b, c\n\
            arg phantom string required\n\
            \x20 doc: not in the signature\n\
            end\n";
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "evil",
            binary(&[("real", "option<string>")]),
            crate::manual::fixtures::component_with_manual(manual),
        );
        let hints = block_on_ready(session.arg_hints("evil")).expect("hints");
        assert_eq!(hints.args.len(), 1, "manual-only args dropped: {hints:?}");
        assert_eq!(hints.args[0].name, "real");
        assert_eq!(hints.args[0].doc.as_deref(), Some("ok[31m doc"));
        assert_eq!(
            hints.args[0].values,
            vec!["ab".to_string(), "c".to_string()]
        );
        // A malformed manual degrades to the bare signature, never an error.
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "broken",
            binary(&[("x", "bool")]),
            crate::manual::fixtures::component_with_manual("eo9-manual 1\nname: b\n"),
        );
        let hints = block_on_ready(session.arg_hints("broken")).expect("hints");
        assert_eq!(hints.args.len(), 1);
        assert_eq!(hints.args[0].doc, None);
    }

    #[test]
    fn arg_hints_memo_follows_the_bytes_cache_invalidation_rules() {
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "telnetd",
            telnetd_like(),
            crate::manual::fixtures::component_with_manual(MAN_TEXT),
        );
        session.backend.program(
            "writer",
            crate::backend::ComponentInfo {
                kind: ComponentKind::Binary,
                imports: vec![crate::backend::ImportNeed {
                    slot: "eo9:fs/fs".to_string(),
                    interface: "eo9:fs/fs".to_string(),
                    version: "0.1.0".to_string(),
                    required: true,
                }],
                exports: vec![],
                args: vec![],
            },
        );
        let resolves = |log: &[String]| {
            log.iter()
                .filter(|line| line.starts_with("resolve("))
                .count()
        };
        block_on_ready(session.arg_hints("telnetd")).expect("hints");
        let baseline = resolves(&session.backend.log);
        // A run whose program imports eo9:fs clears the memo (the program could have
        // rewritten /bin): the next ask resolves again.
        assert_eq!(run(&mut session, "writer"), LineResult::Ok);
        block_on_ready(session.arg_hints("telnetd")).expect("hints");
        assert!(
            resolves(&session.backend.log) > baseline,
            "fs run must invalidate the memo: {:?}",
            session.backend.log
        );
    }

    #[test]
    fn man_of_a_malformed_manual_says_so_and_shows_describe() {
        // Truncated text (no `end`): present but unusable.
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "broken",
            binary(&[]),
            crate::manual::fixtures::component_with_manual("eo9-manual 1\nname: broken\n"),
        );
        assert_eq!(run(&mut session, "man broken"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(out.contains("manual malformed ("), "{out}");
        assert!(out.contains("showing describe"), "{out}");
        assert!(out.contains("kind: binary"), "{out}");
    }

    #[test]
    fn man_of_shell_words_and_apis_renders_their_cards() {
        let mut session = session_with(&[]);
        assert_eq!(run(&mut session, "man describe"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(out.contains("kind: builtin"), "{out}");
        // Nothing was resolved for a builtin card.
        assert!(session.backend.log.is_empty(), "{:?}", session.backend.log);

        session.backend.out.clear();
        assert_eq!(run(&mut session, "man $"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(out.contains("kind: operator"), "{out}");

        session.backend.out.clear();
        assert_eq!(run(&mut session, "man eo9:fs"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(out.contains("kind: OS API package"), "{out}");

        // `man man` explains itself, same as `describe describe`.
        session.backend.out.clear();
        assert_eq!(run(&mut session, "man man"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("kind: builtin") && out.contains("manual"),
            "{out}"
        );
    }

    #[test]
    fn man_of_an_unknown_name_is_the_existing_resolve_error() {
        let mut session = session_with(&[]);
        let result = run(&mut session, "man nosuch");
        assert!(matches!(result, LineResult::Error(_)), "{result:?}");
        assert!(
            session
                .backend
                .err
                .iter()
                .any(|line| line.contains("cannot resolve `nosuch`")),
            "err: {:?}",
            session.backend.err
        );
    }

    #[test]
    fn man_of_a_let_binding_says_bindings_have_no_manual() {
        let mut session = session_with(&[("time.frozen", provider(&["eo9:time/time"]))]);
        assert_eq!(run(&mut session, "let t = time.frozen"), LineResult::Ok);
        assert_eq!(run(&mut session, "man t"), LineResult::Ok);
        let out = session.backend.out.join("\n");
        assert!(
            out.contains("no manual for `t` (a session `let` binding); showing describe"),
            "{out}"
        );
        assert!(out.contains("kind: provider"), "{out}");
    }

    #[test]
    fn man_reads_cached_bytes_instead_of_re_resolving() {
        let mut session = session_with(&[]);
        session.backend.program_with_bytes(
            "telnetd",
            telnetd_like(),
            crate::manual::fixtures::component_with_manual(MAN_TEXT),
        );
        assert_eq!(run(&mut session, "man telnetd"), LineResult::Ok);
        let first_len = session.backend.log.len();
        assert_eq!(run(&mut session, "man telnetd"), LineResult::Ok);
        let tail: Vec<&String> = session.backend.log[first_len..].iter().collect();
        assert!(
            tail.iter().any(|line| line.starts_with("load(telnetd)")),
            "the second man load()s the session-cached bytes: {tail:?}"
        );
        assert!(
            !tail.iter().any(|line| line.starts_with("resolve(telnetd)")),
            "no filesystem re-read: {tail:?}"
        );
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
        assert_eq!(run(&mut session, "poweroff"), LineResult::Poweroff);
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

    // -- history bounds and the editor's snapshots ----------------------------------

    #[test]
    fn history_is_capped_and_recall_view_windows_it() {
        let mut session = session_with(&[]);
        for index in 0..HISTORY_CAP + 10 {
            // Comment lines: recorded in history, no parse/eval side effects.
            run(&mut session, &format!("# line {index}"));
        }
        // The cap held: the oldest 10 were evicted.
        assert_eq!(session.history.len(), HISTORY_CAP);
        assert_eq!(session.history[0], "# line 10");
        assert_eq!(
            session.history.last().unwrap(),
            &format!("# line {}", HISTORY_CAP + 9)
        );
        // The recall view is the newest window, oldest first.
        let view = session.recall_view(64);
        assert_eq!(view.len(), 64);
        assert_eq!(view[0], format!("# line {}", HISTORY_CAP + 10 - 64));
        assert_eq!(view[63], format!("# line {}", HISTORY_CAP + 9));
        // A view wider than the history is just the history.
        assert_eq!(session.recall_view(10_000).len(), HISTORY_CAP);
    }

    #[test]
    fn binding_names_lists_the_session_lets() {
        let mut session = session_with(&[("hello", binary(&[]))]);
        assert!(session.binding_names().is_empty());
        run(&mut session, "let h = hello");
        assert_eq!(session.binding_names(), vec!["h".to_string()]);
    }
}
