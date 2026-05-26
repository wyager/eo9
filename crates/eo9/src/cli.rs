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
#[derive(Debug, Default)]
pub struct Config {
    /// `-v` / `--verbose`: diagnostics on stderr.
    pub verbose: bool,
    /// `--store`: module store root; defaults to `$EO9_STORE`, then `~/.eo9/store`.
    pub store_root: Option<PathBuf>,
    /// `--fs-root`: host directory the program's `eo9:fs` capability is rooted at;
    /// defaults to the process's current working directory. Guest paths can never escape
    /// this root (the unix fs provider enforces containment).
    pub fs_root: Option<PathBuf>,
    /// `--exec-snapshot`: how `open-exec` snapshots a path (default: clone-or-refuse).
    pub exec_snapshot: ExecSnapshotPolicy,
    /// `--max-memory`: linear-memory ceiling (bytes) for the spawned task.
    pub max_memory: Option<u64>,
    /// `--debug-info`: compile images with debug info.
    pub debug_info: bool,
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

/// Parse the rest of the stream as the program's own `--<flag> <value>` pairs.
pub fn parse_program_flags(stream: &mut ArgStream) -> Result<Vec<(String, String)>, String> {
    let mut flags = Vec::new();
    while let Some(token) = stream.next() {
        let name = token.strip_prefix("--").filter(|name| !name.is_empty());
        let Some(name) = name else {
            return Err(format!(
                "unexpected argument {token:?}: program arguments are passed as `--<flag> <value>` pairs"
            ));
        };
        let value = stream
            .next()
            .ok_or_else(|| format!("program argument `--{name}` is missing its value"))?;
        flags.push((name.to_string(), value));
    }
    Ok(flags)
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
                ("name".to_string(), "eo9".to_string()),
                ("excited".to_string(), "true".to_string()),
            ]
        );

        assert!(parse_program_flags(&mut stream(&["name"])).is_err());
        assert!(parse_program_flags(&mut stream(&["--name"])).is_err());
        assert!(parse_program_flags(&mut stream(&["--", "x"])).is_err());
    }

    #[test]
    fn expect_end_rejects_leftovers() {
        assert!(expect_end(&mut stream(&[]), "describe").is_ok());
        assert!(expect_end(&mut stream(&["extra"]), "describe").is_err());
    }
}
