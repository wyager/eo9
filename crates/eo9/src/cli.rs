//! The command-line surface: configuration, the tiny argv parser, and exit codes.
//!
//! Parsing is hand-rolled over `std::env` (plan/11-usermode.md: no CLI framework). The
//! grammar is deliberately small: global options may appear before the command and
//! between the command and its positional argument; for `run`, everything after the
//! program name belongs to the program as `--<flag> <value>` pairs.

use std::path::PathBuf;

use eo9_providers_unix::fs::ExecSnapshotPolicy;
use eo9_store::Store;

/// `run`: the program's `main` returned its success value.
pub const EXIT_SUCCESS: u8 = 0;
/// `run`: the program's `main` returned its failure value.
pub const EXIT_FAILURE: u8 = 1;
/// `run`: the program never returned (trap or kill) — the outcome's abnormal arm.
pub const EXIT_ABNORMAL: u8 = 2;
/// eo9 itself failed before the program produced an outcome (bad usage, resolution,
/// compilation, or spawn errors).
pub const EXIT_ERROR: u8 = 3;

/// Configuration assembled from the global options (and their defaults).
#[derive(Debug, Default, Clone)]
pub struct Config {
    /// `-v` / `--verbose`: diagnostics on stderr.
    pub verbose: bool,
    /// `--store`: module store root; defaults to `$EO9_STORE`, then `~/.eo9/store`.
    pub store_root: Option<PathBuf>,
    /// `--fs-root`: grant the program the `eo9:fs` capability, rooted at this host
    /// directory. Without the flag no filesystem is granted at all (no ambient default);
    /// guest paths can never escape the root (the unix fs provider enforces containment).
    pub fs_root: Option<PathBuf>,
    /// `--exec-snapshot`: how `open-exec` snapshots a path (default: clone-or-refuse).
    pub exec_snapshot: ExecSnapshotPolicy,
    /// `--max-memory`: linear-memory ceiling (bytes) for the spawned task.
    pub max_memory: Option<u64>,
    /// `--debug-info`: compile images with debug info.
    pub debug_info: bool,
    /// `--max-fuel`: total fuel budget for the run; when it is exhausted the task is
    /// killed and the run ends as `abnormal(killed)`. `None` means unlimited (the
    /// default), matching the previous behavior.
    pub max_fuel: Option<u64>,
    /// `--outcome`: where the typed outcome line goes. The program's own output always
    /// stays on stdout; the outcome line defaults to stderr so pipes carry only program
    /// output (the exit code already encodes success/failure/abnormal).
    pub outcome: OutcomeChannel,
}

/// Where `eo9 run` writes the rendered `program-outcome` line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeChannel {
    /// Print the outcome line on stderr (the default).
    #[default]
    Stderr,
    /// Print the outcome line on stdout (the pre-2026-05 behavior).
    Stdout,
    /// Do not print the outcome line at all; the exit code still reports it.
    Quiet,
}

impl Config {
    /// Open the module store this invocation should use.
    pub fn open_store(&self) -> Result<Store, String> {
        match &self.store_root {
            Some(root) => Store::open(root),
            None => Store::open_default(),
        }
        .map_err(|err| format!("cannot open the module store: {err}"))
    }
}

/// Verbose diagnostics: `vlog!(cfg, "...")` writes to stderr when `-v` was given.
macro_rules! vlog {
    ($cfg:expr, $($arg:tt)*) => {
        if $cfg.verbose {
            eprintln!("eo9: {}", format!($($arg)*));
        }
    };
}
pub(crate) use vlog;

/// A forward-only cursor over the argv tokens.
pub struct ArgStream {
    args: Vec<String>,
    pos: usize,
}

impl ArgStream {
    pub fn new(args: Vec<String>) -> ArgStream {
        ArgStream { args, pos: 0 }
    }

    /// The next token, without consuming it.
    pub fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(String::as_str)
    }

    /// Consume and return the next token.
    #[allow(clippy::should_implement_trait)] // not an Iterator: `peek` without `Peekable` noise
    pub fn next(&mut self) -> Option<String> {
        let token = self.args.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    /// Consume the value of the option named `flag` (the next token), or explain that it
    /// is missing.
    fn value_of(&mut self, flag: &str) -> Result<String, String> {
        self.next()
            .ok_or_else(|| format!("option `{flag}` needs a value"))
    }
}

/// Consume every recognized global option at the stream's current position.
pub fn consume_global_options(stream: &mut ArgStream, cfg: &mut Config) -> Result<(), String> {
    while let Some(token) = stream.peek() {
        match token {
            "-v" | "--verbose" => {
                cfg.verbose = true;
                stream.next();
            }
            "--debug-info" => {
                cfg.debug_info = true;
                stream.next();
            }
            "--store" => {
                stream.next();
                cfg.store_root = Some(PathBuf::from(stream.value_of("--store")?));
            }
            "--fs-root" => {
                stream.next();
                cfg.fs_root = Some(PathBuf::from(stream.value_of("--fs-root")?));
            }
            "--exec-snapshot" => {
                stream.next();
                cfg.exec_snapshot = parse_exec_snapshot(&stream.value_of("--exec-snapshot")?)?;
            }
            "--max-memory" => {
                stream.next();
                let value = stream.value_of("--max-memory")?;
                cfg.max_memory = Some(value.parse().map_err(|err| {
                    format!("invalid --max-memory value {value:?} (bytes expected): {err}")
                })?);
            }
            "--max-fuel" => {
                stream.next();
                let value = stream.value_of("--max-fuel")?;
                cfg.max_fuel = Some(value.parse().map_err(|err| {
                    format!("invalid --max-fuel value {value:?} (fuel units expected): {err}")
                })?);
            }
            "--outcome" => {
                stream.next();
                cfg.outcome = match stream.value_of("--outcome")?.as_str() {
                    "stderr" => OutcomeChannel::Stderr,
                    "stdout" => OutcomeChannel::Stdout,
                    "quiet" => OutcomeChannel::Quiet,
                    other => {
                        return Err(format!(
                            "invalid --outcome value {other:?}: expected stderr, stdout, or quiet"
                        ));
                    }
                };
            }
            _ => break,
        }
    }
    Ok(())
}

fn parse_exec_snapshot(value: &str) -> Result<ExecSnapshotPolicy, String> {
    match value {
        "clone-or-refuse" => Ok(ExecSnapshotPolicy::CloneOrRefuse),
        "clone-or-copy" => Ok(ExecSnapshotPolicy::CloneOrCopy),
        other => Err(format!(
            "invalid --exec-snapshot policy {other:?}: expected clone-or-refuse or clone-or-copy"
        )),
    }
}

/// One argument destined for the program being run: either a `--<flag> <value>` pair or a
/// bare positional value (filled against `main`'s parameters in declaration order; see
/// [`crate::run`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramArg {
    /// `--name value`.
    Flag { name: String, value: String },
    /// A bare value with no flag name.
    Positional(String),
}

/// Parse the rest of the stream as the program's own arguments: `--<flag> <value>` pairs
/// and bare positional values, in the order they were given.
pub fn parse_program_flags(stream: &mut ArgStream) -> Result<Vec<ProgramArg>, String> {
    let mut args = Vec::new();
    while let Some(token) = stream.next() {
        match token.strip_prefix("--").filter(|name| !name.is_empty()) {
            Some(name) => {
                let value = stream
                    .next()
                    .ok_or_else(|| format!("program argument `--{name}` is missing its value"))?;
                args.push(ProgramArg::Flag {
                    name: name.to_string(),
                    value,
                });
            }
            None if token == "--" => {
                // Everything after a literal `--` is positional, even if it looks like a flag.
                while let Some(rest) = stream.next() {
                    args.push(ProgramArg::Positional(rest));
                }
            }
            None => args.push(ProgramArg::Positional(token)),
        }
    }
    Ok(args)
}

/// Parse the `shell` command's own arguments: global options plus an optional
/// `-c`/`--command <line>` one-shot command.
pub fn parse_shell_args(
    stream: &mut ArgStream,
    cfg: &mut Config,
) -> Result<Option<String>, String> {
    let mut command = None;
    loop {
        consume_global_options(stream, cfg)?;
        match stream.peek() {
            Some("-c") | Some("--command") => {
                let flag = stream.next().expect("peeked token exists");
                let line = stream
                    .next()
                    .ok_or_else(|| format!("option `{flag}` needs a command line"))?;
                if command.replace(line).is_some() {
                    return Err("`-c`/`--command` may be given at most once".to_string());
                }
            }
            Some(other) => {
                return Err(format!("unexpected argument `{other}` for `shell`"));
            }
            None => break,
        }
    }
    Ok(command)
}

/// Fail if any tokens remain.
pub fn expect_end(stream: &mut ArgStream, command: &str) -> Result<(), String> {
    match stream.next() {
        None => Ok(()),
        Some(token) => Err(format!(
            "unexpected extra argument `{token}` for `{command}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(tokens: &[&str]) -> ArgStream {
        ArgStream::new(tokens.iter().map(|t| t.to_string()).collect())
    }

    #[test]
    fn global_options_are_consumed_up_to_the_first_other_token() {
        let mut args = stream(&[
            "-v",
            "--store",
            "/tmp/store",
            "--max-memory",
            "1048576",
            "--exec-snapshot",
            "clone-or-copy",
            "run",
            "--verbose",
        ]);
        let mut cfg = Config::default();
        consume_global_options(&mut args, &mut cfg).unwrap();
        assert!(cfg.verbose);
        assert_eq!(
            cfg.store_root.as_deref(),
            Some(std::path::Path::new("/tmp/store"))
        );
        assert_eq!(cfg.max_memory, Some(1_048_576));
        assert_eq!(cfg.exec_snapshot, ExecSnapshotPolicy::CloneOrCopy);
        // Stops at the command; the later `--verbose` belongs to whatever follows.
        assert_eq!(args.next().as_deref(), Some("run"));
    }

    #[test]
    fn missing_option_values_are_reported() {
        let mut cfg = Config::default();
        let err = consume_global_options(&mut stream(&["--store"]), &mut cfg).unwrap_err();
        assert!(err.contains("--store"), "unexpected message: {err}");
        let err =
            consume_global_options(&mut stream(&["--max-memory", "lots"]), &mut cfg).unwrap_err();
        assert!(err.contains("--max-memory"), "unexpected message: {err}");
        let err = consume_global_options(&mut stream(&["--exec-snapshot", "maybe"]), &mut cfg)
            .unwrap_err();
        assert!(err.contains("clone-or-refuse"), "unexpected message: {err}");
    }

    #[test]
    fn program_flags_are_name_value_pairs() {
        let flags =
            parse_program_flags(&mut stream(&["--name", "eo9", "--excited", "true"])).unwrap();
        assert_eq!(
            flags,
            vec![
                ProgramArg::Flag {
                    name: "name".to_string(),
                    value: "eo9".to_string()
                },
                ProgramArg::Flag {
                    name: "excited".to_string(),
                    value: "true".to_string()
                },
            ]
        );

        assert!(parse_program_flags(&mut stream(&["--name"])).is_err());
    }

    #[test]
    fn bare_values_are_positional_arguments() {
        let args =
            parse_program_flags(&mut stream(&["a.txt", "--excited", "true", "b.txt"])).unwrap();
        assert_eq!(
            args,
            vec![
                ProgramArg::Positional("a.txt".to_string()),
                ProgramArg::Flag {
                    name: "excited".to_string(),
                    value: "true".to_string()
                },
                ProgramArg::Positional("b.txt".to_string()),
            ]
        );

        // A literal `--` makes everything after it positional, even flag-shaped tokens.
        let args = parse_program_flags(&mut stream(&["--", "--weird-name"])).unwrap();
        assert_eq!(
            args,
            vec![ProgramArg::Positional("--weird-name".to_string())]
        );
    }

    #[test]
    fn expect_end_rejects_leftovers() {
        assert!(expect_end(&mut stream(&[]), "describe").is_ok());
        assert!(expect_end(&mut stream(&["extra"]), "describe").is_err());
    }
}
