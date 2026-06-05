//! init — the Eo9 service-boot program (executor v1).
//!
//! An ordinary Eo9 binary with no private powers (anyone can write a different init):
//! it parses a small service config, resolves each named program from `/bin`, detaches
//! it to the service registry under its restart policy, and then keeps a console
//! program running in the foreground. When the console exits and services are still
//! running, the console is restarted (owner ruling D: leaving the shell and halting the
//! machine are different intents); when the console exits with nothing left running,
//! init exits too.
//!
//! Config format (one entry per line; `#` starts a comment):
//!
//! ```text
//! # services: <name> = <program> [--flag value …] restart <policy> [--flag value …]
//! worker  = cruncher --seed 7 --rounds 900000000 restart restart.always
//! greeter = hello-closed restart restart.never
//! # the console (optional; default `console = eosh`):
//! console = eosh
//! ```
//!
//! Programs are referenced by their `/bin` names; a composition can be detached by
//! `save`-ing it under a name first (in a `--svc` shell) and referencing that name.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eo9_guest::buffer;

mod bindings {
    wit_bindgen::generate!({
        world: "init",
        generate_all,
        with: {
            "eo9:io/buffers@0.1.0": eo9_guest::api::io::buffers,
            "eo9:text/types@0.1.0": eo9_guest::api::text::types,
            "eo9:text/text@0.1.0": eo9_guest::api::text::text,
            "eo9:fs/fs@0.1.0": eo9_guest::api::fs::fs,
        },
    });
}

use bindings::eo9::exec::{compile, component_algebra as algebra, task};
use bindings::eo9::svc::{detach as svc_detach, detach_optional, services, services_optional};
use bindings::{Guest, ProgramFailure, ProgramSuccess, export};
use eo9_guest::api::fs::fs;
use eo9_guest::api::text::text;

// -----------------------------------------------------------------------------------------
// Config parsing
// -----------------------------------------------------------------------------------------

/// One `--flag value` pair from a config line.
struct Flag {
    name: String,
    value: String,
}

/// One service entry: `<name> = <program> [flags…] restart <policy> [flags…]`.
struct ServiceEntry {
    name: String,
    program: String,
    args: Vec<Flag>,
    policy: String,
    policy_args: Vec<Flag>,
}

/// What to do when the console exits while services are still running.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConsoleRestart {
    /// Restart it (owner ruling D: leaving the shell and halting are different intents).
    /// This is the default, meant for interactive consoles.
    Always,
    /// Do not restart: init exits when the console does (services then die with the
    /// embedding process). The right mode for scripted/piped sessions, where a console
    /// that has reached end-of-input would otherwise restart-loop forever.
    Never,
}

/// The parsed config: the services to detach plus the console program.
struct Config {
    services: Vec<ServiceEntry>,
    console: String,
    console_restart: ConsoleRestart,
}

/// Split a line into whitespace-separated words (config values with spaces are not
/// supported in v1 — keep service arguments simple or `save` a composition).
fn words(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

/// Collect `--flag value` pairs from `tokens`, stopping at (and reporting) the index of
/// the first token that is not part of a pair.
fn collect_flags(tokens: &[&str]) -> Result<(Vec<Flag>, usize), String> {
    let mut flags = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let Some(name) = token.strip_prefix("--") else {
            break;
        };
        let Some(value) = tokens.get(index + 1) else {
            return Err(format!("flag `--{name}` has no value"));
        };
        flags.push(Flag {
            name: name.to_string(),
            value: (*value).to_string(),
        });
        index += 2;
    }
    Ok((flags, index))
}

fn parse_config(config: &str) -> Result<Config, String> {
    let mut services = Vec::new();
    let mut console: Option<String> = None;
    let mut console_restart: Option<ConsoleRestart> = None;

    for (line_number, raw_line) in config.lines().enumerate() {
        let line = match raw_line.find('#') {
            Some(at) => &raw_line[..at],
            None => raw_line,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let context = |reason: &str| format!("line {}: {reason}: {raw_line}", line_number + 1);

        let Some((name, rest)) = line.split_once('=') else {
            return Err(context("expected `<name> = <program> …`"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(context("the entry has no name"));
        }
        let tokens = words(rest);
        if tokens.is_empty() {
            return Err(context("the entry names no program"));
        }

        if name == "console" {
            // The console line: just a program reference (no args, no restart clause —
            // init itself is the console's restart policy, per ruling D).
            if tokens.len() != 1 {
                return Err(context(
                    "the console entry takes a single program name (no arguments or \
                     restart clause — init restarts the console itself)",
                ));
            }
            console = Some(tokens[0].to_string());
            continue;
        }

        if name == "console-restart" {
            // What happens when the console exits while services live.
            console_restart = match tokens {
                ref t if t.len() == 1 && t[0] == "always" => Some(ConsoleRestart::Always),
                ref t if t.len() == 1 && t[0] == "never" => Some(ConsoleRestart::Never),
                _ => {
                    return Err(context(
                        "`console-restart` must be `always` (restart while services live — \
                         the interactive default) or `never` (init exits with the console — \
                         the scripted default)",
                    ));
                }
            };
            continue;
        }

        // A service line: program [flags…] restart policy [flags…]. Split at the LAST
        // bare `restart` word (so a program named `restart` still works).
        let split = tokens
            .iter()
            .rposition(|token| *token == "restart")
            .ok_or_else(|| {
                context(
                    "missing the `restart <policy>` clause (every service needs a restart \
                     policy: restart.never, restart.always, or restart.backoff)",
                )
            })?;
        let (program_part, policy_part) = tokens.split_at(split);
        let policy_part = &policy_part[1..]; // drop the `restart` keyword

        if program_part.is_empty() {
            return Err(context("the entry names no program before `restart`"));
        }
        if policy_part.is_empty() {
            return Err(context("`restart` must be followed by a policy name"));
        }

        let program = program_part[0].to_string();
        let (args, consumed) = collect_flags(&program_part[1..]).map_err(|err| context(&err))?;
        if consumed != program_part.len() - 1 {
            return Err(context(
                "unexpected token after the program's `--flag value` arguments",
            ));
        }

        let policy = policy_part[0].to_string();
        let (policy_args, consumed) =
            collect_flags(&policy_part[1..]).map_err(|err| context(&err))?;
        if consumed != policy_part.len() - 1 {
            return Err(context(
                "unexpected token after the policy's `--flag value` arguments",
            ));
        }

        services.push(ServiceEntry {
            name: name.to_string(),
            program,
            args,
            policy,
            policy_args,
        });
    }

    Ok(Config {
        services,
        console: console.unwrap_or_else(|| "eosh".to_string()),
        console_restart: console_restart.unwrap_or(ConsoleRestart::Always),
    })
}

// -----------------------------------------------------------------------------------------
// Program resolution and argument binding
// -----------------------------------------------------------------------------------------

/// Output helpers: every line init prints is prefixed so console output and init's own
/// narration stay distinguishable.
struct Out {
    text: text::TextImpl,
}

impl Out {
    fn new() -> Self {
        Out {
            text: text::default(),
        }
    }

    fn say(&self, line: &str) {
        let _ = text::write(&self.text, text::OutputStream::Out, "init: ");
        let _ = text::write(&self.text, text::OutputStream::Out, line);
        let _ = text::write(&self.text, text::OutputStream::Out, "\n");
    }

    fn warn(&self, line: &str) {
        let _ = text::write(&self.text, text::OutputStream::Err, "init: ");
        let _ = text::write(&self.text, text::OutputStream::Err, line);
        let _ = text::write(&self.text, text::OutputStream::Err, "\n");
    }
}

/// Resolve a `/bin` program name to an open component value (the same interim
/// convention as eosh: open `/bin/<name>.wasm` for execution, read it, `load` it).
async fn resolve(handle: &fs::FsImpl, name: &str) -> Result<algebra::Component, String> {
    let path = format!("/bin/{name}.wasm");
    let exec_handle = fs::open_exec(handle, path.clone())
        .await
        .map_err(|err| format!("cannot resolve `{name}` ({path}): {}", fs_error_text(&err)))?;

    let size = fs::exec_size(&exec_handle);
    let mut bytes: Vec<u8> = Vec::with_capacity(size as usize);
    while (bytes.len() as u64) < size {
        let offset = bytes.len() as u64;
        let chunk = buffer::with_capacity(size - offset);
        let (chunk, result) = fs::exec_read(&exec_handle, offset, chunk).await;
        let read =
            result.map_err(|err| format!("reading `{name}` failed: {}", fs_error_text(&err)))?;
        if read.bytes_read == 0 {
            return Err(format!("reading `{name}` ended early (zero-length read)"));
        }
        bytes.extend_from_slice(&buffer::prefix_to_vec(&chunk, read.bytes_read));
    }

    algebra::load(&bytes).map_err(|err| format!("cannot load `{name}`: {err:?}"))
}

fn fs_error_text(err: &fs::FsError) -> String {
    match err {
        fs::FsError::NotFound => String::from("not found (is it installed under /bin?)"),
        fs::FsError::Denied => String::from("refused by the filesystem's policy"),
        other => format!("{other:?}"),
    }
}

/// Encode text as a WAVE string literal.
fn wave_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Bind config `--flag value` pairs against a program's declared argument signature
/// (type-directed, the same rules as the CLI): string parameters are taken literally
/// and WAVE-quoted; `option<…>` values are wrapped in `some(…)`; everything else is
/// passed through as WAVE text for the runtime to type-check.
fn bind_args(
    info: &algebra::ComponentInfo,
    flags: &[Flag],
) -> Result<Vec<svc_detach::NamedArg>, String> {
    let mut bound = Vec::with_capacity(flags.len());
    for flag in flags {
        let spec = info.args.iter().find(|spec| spec.name == flag.name);
        let value = match spec {
            Some(spec) => encode_for_type(&spec.ty, &flag.value),
            // Unknown flags pass through; the runtime's signature check reports them
            // with the full picture (unknown vs. missing vs. ill-typed).
            None => flag.value.clone(),
        };
        bound.push(svc_detach::NamedArg {
            name: flag.name.clone(),
            value,
        });
    }
    Ok(bound)
}

fn encode_for_type(ty: &str, value: &str) -> String {
    if ty == "string" {
        return wave_string(value);
    }
    if let Some(inner) = ty.strip_prefix("option<").and_then(|t| t.strip_suffix(">")) {
        if value == "none" {
            return String::from("none");
        }
        return format!("some({})", encode_for_type(inner.trim(), value));
    }
    // A single token for a `list<string>` parameter is the one-element list -- the
    // named-flag spelling of the variadic tail (`echo --text hi`), same rule as the
    // shell (eosh-core wave.rs). A bracketed value passes through as written.
    if ty
        .strip_prefix("list<")
        .and_then(|t| t.strip_suffix(">"))
        .map(str::trim)
        == Some("string")
        && !value.trim_start().starts_with('[')
    {
        return format!("[{}]", wave_string(value));
    }
    value.to_string()
}

// -----------------------------------------------------------------------------------------
// The program
// -----------------------------------------------------------------------------------------

/// Render a task outcome in one line.
fn outcome_text(outcome: &task::ProgramOutcome) -> String {
    match outcome {
        task::ProgramOutcome::Success(value) => {
            if value.value.is_empty() {
                String::from("success")
            } else {
                format!("success({})", value.value)
            }
        }
        task::ProgramOutcome::Failure(value) => {
            if value.value.is_empty() {
                String::from("failure")
            } else {
                format!("failure({})", value.value)
            }
        }
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Trapped(reason)) => {
            format!("abnormal(trapped({reason}))")
        }
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Killed) => {
            String::from("abnormal(killed)")
        }
    }
}

/// Render a detach refusal in plain words.
fn detach_error_text(err: &svc_detach::DetachError) -> String {
    use svc_detach::DetachError as E;
    match err {
        E::NotClosed(needs) => format!(
            "it still requires {} (compose those in and `save` the result, then reference \
             the saved name)",
            needs.join(", ")
        ),
        E::NotABinary => String::from("it is a provider, not a runnable program"),
        E::NameTaken(name) => format!("a service named `{name}` already exists"),
        E::InvalidName(name) => format!("`{name}` is not a usable service name"),
        E::InvalidPolicy(reason) => format!("invalid restart policy: {reason}"),
        E::Exhausted => String::from("the service registry is full"),
        E::Internal(reason) => reason.clone(),
    }
}

struct Init;

impl Guest for Init {
    async fn main(config: String) -> Result<ProgramSuccess, ProgramFailure> {
        let out = Out::new();

        // The whole point of init is the svc capability; without it, say so clearly.
        let Some(detach_handle) = detach_optional::default() else {
            return Err(ProgramFailure::NoSvcCapability(String::from(
                "this session does not hold the svc capability (run init via `eo9 init`, \
                 which grants it; `eo9 run init` does not)",
            )));
        };
        let Some(services_handle) = services_optional::default() else {
            return Err(ProgramFailure::NoSvcCapability(String::from(
                "this session holds detach but not services; init needs both halves",
            )));
        };

        let parsed = parse_config(&config).map_err(ProgramFailure::BadConfig)?;
        let fs_handle = fs::default();

        // ----- detach every service entry --------------------------------------------
        let mut started: u32 = 0;
        for entry in &parsed.services {
            let result = start_service(&fs_handle, &detach_handle, entry).await;
            match result {
                Ok(()) => {
                    out.say(&format!(
                        "started `{}` ({} under {})",
                        entry.name, entry.program, entry.policy
                    ));
                    started += 1;
                }
                Err(reason) => {
                    // One bad entry never blocks the rest of the boot (or the console).
                    out.warn(&format!("could not start `{}`: {reason}", entry.name));
                }
            }
        }
        if !parsed.services.is_empty() {
            out.say(&format!(
                "{started} of {} service(s) running; `svc list` at the console inspects them",
                parsed.services.len()
            ));
        }

        // ----- the console loop (ruling D) --------------------------------------------
        // Resolve and compile the console once; each restart is a fresh spawn of the
        // same image.
        let console = resolve(&fs_handle, &parsed.console)
            .await
            .map_err(ProgramFailure::ConsoleFailed)?;
        let opts = compile::CompileOpts {
            debug_info: false,
            safepoint_maps: false,
        };
        let image = compile::compile(console, opts).map_err(|err| {
            ProgramFailure::ConsoleFailed(format!("compiling the console failed: {err:?}"))
        })?;

        // Hard ceiling on console restarts: even under `console-restart = always`, a
        // console that cannot actually run (wedged terminal, missing input) must not
        // restart-loop forever. Generous for interactive use, finite for everything else.
        const MAX_CONSOLE_RESTARTS: u32 = 1000;
        let mut console_runs: u32 = 0;

        loop {
            // eosh's argument signature (`command: option<string>`) is the de-facto
            // console contract; a console with no arguments works too (none are passed
            // beyond what its signature declares as optional).
            let limits = task::SpawnLimits { max_memory: None };
            let console_task = task::spawn(&image, &[], Vec::new(), limits).map_err(|err| {
                ProgramFailure::ConsoleFailed(format!("spawning the console failed: {err:?}"))
            })?;
            console_runs += 1;
            let outcome = task::wait(&console_task).await;
            out.say(&format!("the console exited ({})", outcome_text(&outcome)));

            // The poweroff intent (the console's `poweroff` builtin) flows up as a
            // typed outcome: halt means halt — init exits regardless of running
            // services (the embedder's teardown stops them), instead of restarting
            // the console per ruling D.
            if let task::ProgramOutcome::Success(value) = &outcome
                && value.value == "poweroff-requested"
            {
                out.say("the console requested poweroff; init exiting");
                return Ok(ProgramSuccess::Exited);
            }

            // Scripted mode (`console-restart = never`): the console's exit is init's
            // exit. Whatever services still run die with the embedding process — the
            // process-bound lifetime of owner ruling E.
            if parsed.console_restart == ConsoleRestart::Never {
                out.say("console-restart is `never`; init exiting");
                return Ok(ProgramSuccess::Exited);
            }

            // Ruling D: while services are still alive, leaving the console restarts it
            // (leaving the shell and halting the machine are different intents). When
            // nothing is running anymore, init's job is done.
            let alive = services::list(&services_handle)
                .into_iter()
                .any(|service| !matches!(service.state, services::ServiceState::Finished));
            if !alive {
                out.say("no services running; init exiting");
                return Ok(ProgramSuccess::Exited);
            }
            if console_runs >= MAX_CONSOLE_RESTARTS {
                out.warn(&format!(
                    "the console has been restarted {MAX_CONSOLE_RESTARTS} times; giving up \
                     (services die with this process)"
                ));
                return Ok(ProgramSuccess::Exited);
            }
            out.say(
                "services are still running; restarting the console (stop them with \
                 `svc stop <name>` to let init exit, or end the whole process to stop \
                 everything)",
            );
        }
    }
}

/// Resolve, bind, and detach one service entry.
async fn start_service(
    fs_handle: &fs::FsImpl,
    detach_handle: &svc_detach::DetachImpl,
    entry: &ServiceEntry,
) -> Result<(), String> {
    // The program and its arguments.
    let program = resolve(fs_handle, &entry.program).await?;
    let info = algebra::describe(&program);
    let args = bind_args(&info, &entry.args)?;

    // The restart policy (configured when the entry gives policy flags).
    let mut policy = resolve(fs_handle, &entry.policy).await?;
    if !entry.policy_args.is_empty() {
        let policy_info = algebra::describe(&policy);
        let policy_args: Vec<algebra::NamedArg> = bind_args(&policy_info, &entry.policy_args)?
            .into_iter()
            .map(|arg| algebra::NamedArg {
                name: arg.name,
                value: arg.value,
            })
            .collect();
        policy = algebra::configure(policy, &policy_args)
            .map_err(|err| format!("configuring `{}` failed: {err:?}", entry.policy))?;
    }

    svc_detach::detach(
        detach_handle,
        program,
        policy,
        &entry.name,
        &args,
        svc_detach::LogPolicy::Capture,
    )
    .map(|_| ())
    .map_err(|err| detach_error_text(&err))
}

export!(Init with_types_in bindings);
