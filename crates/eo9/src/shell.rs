//! `eo9 shell`: run eosh, the Eo9 shell, as an ordinary Eo9 program.
//!
//! The shell has no private powers (SPEC.md "Shell"): eosh is a guest component that
//! imports `eo9:exec`, `eo9:text`, and `eo9:fs`, and this command is just the embedder
//! that builds its **session**:
//!
//! * a session directory under the store root whose `bin/` holds one `<name>.wasm` per
//!   bound store name (plus the dev-tree example/stub components), because eosh resolves
//!   program names as `/bin/<name>.wasm` on its granted filesystem;
//! * the usual root providers (terminal stdio, host clocks, OS RNG), the layered session
//!   filesystem (the read-only `/bin` program view over the writable `--fs-root`), and the
//!   full `eo9:exec` capability (component algebra, compile, spawn) — whose child policy
//!   hands every child the *same* environment by default, so a child `eosh` is a full peer
//!   (it can resolve `/bin`, compose, compile, and spawn, and recurse further); attenuate
//!   any one command with `only`;
//! * the existing drive loop; interactive when no command was given, one-shot with `-c`.
//!
//! Known limitation (runtime escalation E5): children execute inside the shell's own
//! resume donations, so a long-running child throttles the shell until it finishes.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use eo9_runtime::{NamedArg, Outcome, SpawnLimits, Task};
use eo9_store::{Name, Store};

use crate::cli::{Config, vlog};
use crate::compile;
use crate::complete::ShellCompleter;
use crate::interactive::InteractiveText;
use crate::providers;
use crate::run;
use crate::seed;
use crate::source::{self, ProgramSource};

/// Where `cargo xtask build-guest` puts components in a development tree, relative to the
/// current directory: the fallback source for eosh itself and the fill-in source for the
/// session bin view.
const DEV_COMPONENTS_DIR: &str = "guest/target/components";

pub fn cmd_shell(cfg: &Config, command: Option<String>) -> Result<u8, String> {
    let store = cfg.open_store()?;

    // First run against an empty store: seed it from the embedded components so the
    // shell has programs to offer; after an upgrade, refresh the bundled names so a store
    // seeded by an older eo9 keeps working. A seeding problem never blocks the shell.
    if let Err(err) = seed::ensure_seeded(cfg, &store) {
        eprintln!("eo9: warning: could not seed/refresh the module store: {err}");
    }

    let eosh = resolve_eosh(cfg, &store)?;
    let (session_root, session_names) = materialize_session(cfg, &store)?;

    // The session manifest: what this session holds and what children receive, written
    // where eosh can read it with its own fs capability (the `env` builtin renders it).
    // Informational only — failing to write it never blocks the shell.
    if let Err(err) = fs::write(
        session_root.join("session"),
        providers::session_manifest(cfg),
    ) {
        eprintln!("eo9: warning: cannot write the session manifest (`env` will say less): {err}");
    }

    // Interactive sessions on a real terminal get the line editor (history + tab
    // completion over the session's names); piped input and `-c` keep the plain
    // provider so transcripts behave exactly as before.
    let interactive =
        command.is_none() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let editor = interactive
        .then(|| InteractiveText::new(ShellCompleter::new(session_names, cfg.fs_root.clone())));

    let loaded = compile::load_image(cfg, &store, &eosh)?;
    let shell_providers = providers::shell_providers(cfg, &session_root, &loaded.image, editor)?;

    // Spawn-time visibility of what children inherit (user-study finding #8): the full
    // picture is in the session manifest that `env` renders, but a `-v` line states it up
    // front so it is not silent. Children get exactly what `only` can then restrict.
    vlog!(
        cfg,
        "children spawned from this shell inherit: text, time, entropy, fs ({}), and the \
         full exec surface (component algebra, compile, spawn); restrict any command with `only`",
        match &cfg.fs_root {
            Some(root) => format!("/bin read-only + {} writable", root.display()),
            None => "/bin read-only, no writable data root".to_string(),
        }
    );

    // eosh's single argument: `command: option<string>` — interactive REPL when absent,
    // one-shot command when present.
    let command_value = match &command {
        Some(line) => format!("some({})", run::wave_string(line)),
        None => "none".to_string(),
    };
    let args = [NamedArg::new("command", command_value)];

    let limits = SpawnLimits {
        max_memory: cfg.max_memory,
        max_table_elements: None,
    };
    let mut task = Task::spawn(&loaded.image, &args, limits, shell_providers).map_err(|err| {
        run::stale_store_hint(
            &eosh.origin,
            format!("cannot spawn eosh ({}): {err}", eosh.origin),
        )
    })?;

    let outcome = run::drive_to_completion(cfg, &mut task);
    let (rendered, code) = run::render_outcome(&outcome);
    let one_shot = command.is_some();
    match &outcome {
        // A clean shell exit stays quiet: everything worth seeing was already printed by
        // eosh (and its children) through the text capability.
        Outcome::Success(_) => vlog!(cfg, "shell outcome: {rendered}"),
        // One-shot (`-c`): eosh already surfaced the command's own outcome (on stderr) and
        // the program's output, so re-printing eosh's `failure(…)` wrapper here is the
        // redundant "outcome one layer down" the user studies flagged — the exit code
        // below carries it instead. An unexpected eosh trap/kill (which eosh could not
        // report itself) still falls through to be surfaced.
        Outcome::Failure(_) if one_shot => vlog!(cfg, "shell outcome: {rendered}"),
        _ => run::print_outcome(cfg, &rendered),
    }
    // Exit-code contract for `-c`, matching `eo9 run`: 0 success, 1 the command reported
    // failure, 2 the command ended abnormally (trapped/killed) or eosh itself did, 3 the
    // shell could not run the command at all (or an eo9-level error, returned earlier as
    // `Err`). eosh's `program-failure` carries the inner command's three-way class as the
    // variant case (plan/11 D14/D16), so the case name of the typed failure value is the
    // class — no string parsing of free-form text.
    let code = match &outcome {
        Outcome::Failure(value) if one_shot => {
            let case = value.value.split('(').next().unwrap_or("").trim();
            match case {
                // The command ran and reported failure in its own vocabulary.
                "command-failed" => crate::cli::EXIT_FAILURE,
                // The command ended abnormally — same class as `eo9 run`'s exit 2.
                "command-trapped" | "command-killed" => crate::cli::EXIT_ABNORMAL,
                // The command never ran (`not-runnable`), or eosh's own streams broke
                // (`io`): an eosh/eo9-level error, not the command's outcome.
                "not-runnable" | "io" => crate::cli::EXIT_ERROR,
                // An older eosh (or an unexpected case) keeps the plain failure code.
                _ => code,
            }
        }
        _ => code,
    };
    Ok(code)
}

/// Locate the eosh component. Lookup order: the store-bound name `eosh` (the installed
/// form — first-run seeding normally provides it), then the dev-tree artifact
/// `guest/target/components/eosh.wasm` relative to the current directory (the checkout
/// convenience), then the copy embedded in this binary.
fn resolve_eosh(cfg: &Config, store: &Store) -> Result<ProgramSource, String> {
    let name = Name::parse("eosh").expect("`eosh` is a valid store name");
    let bound = store
        .lookup_name_in(eo9_store::DEFAULT_PROFILE, &name)
        .map_err(|err| err.to_string())?;
    if bound.is_some() {
        return source::resolve_program(cfg, "eosh");
    }

    let dev = Path::new(DEV_COMPONENTS_DIR).join("eosh.wasm");
    if dev.is_file() {
        return source::resolve_program(cfg, &dev.display().to_string());
    }

    if let Some(bytes) = seed::embedded("eosh") {
        vlog!(cfg, "using the eosh component embedded in this binary");
        return Ok(ProgramSource {
            bytes: bytes.to_vec(),
            hash: eo9_store::ObjectHash::of(bytes),
            origin: "eosh (embedded in the eo9 binary)".to_string(),
        });
    }

    Err(format!(
        "cannot find the eosh component: bind it in the store \
         (`eo9 store add <path-to-eosh.wasm> --name eosh`) or build it in a development \
         tree (`cargo xtask build-guest`, which produces {DEV_COMPONENTS_DIR}/eosh.wasm), \
         then run `eo9 shell` again"
    ))
}

/// Build the session directory the shell's filesystem is rooted at, one per process:
/// `<store-root>/shell/session-<pid>/bin/<name>.wasm`, one entry per bound store name —
/// hard-linked to the store object when possible, copied otherwise — plus the dev-tree
/// components under the names they answer to in a shell (`hello`, `entropy.seeded`, …).
/// Store bindings win over dev-tree components of the same name.
///
/// Sessions used to share a single `shell/bin` directory, rebuilt on every start — so two
/// concurrent `eo9 -c` invocations corrupted each other's view about a third of the time
/// (study 07, S7-6). Each process now owns its own session directory, pinned by a file
/// lock held for the process's lifetime; dead sessions (lock no longer held) are swept on
/// the next start.
///
/// Returns the session directory and the program names placed into the bin view (the
/// names eosh can resolve — also the shell's tab-completion candidates).
fn materialize_session(cfg: &Config, store: &Store) -> Result<(PathBuf, Vec<String>), String> {
    let shell_root = store.root().join("shell");
    fs::create_dir_all(&shell_root)
        .map_err(|err| format!("cannot create {}: {err}", shell_root.display()))?;

    // Pin this session BEFORE its directory exists: the lock file is a sibling of the
    // session directory (`session-<pid>.lock` next to `session-<pid>/`), created and
    // locked first, so no other process's sweep can ever observe a live session
    // directory whose lock is not yet held.
    let session = shell_root.join(format!("session-{}", std::process::id()));
    let lock_path = shell_root.join(format!("session-{}.lock", std::process::id()));
    let lock = fs::File::create(&lock_path)
        .map_err(|err| format!("cannot create {}: {err}", lock_path.display()))?;
    lock.lock()
        .map_err(|err| format!("cannot lock {}: {err}", lock_path.display()))?;
    // Held until the process exits; the sweep in other processes keys off it.
    Box::leak(Box::new(lock));

    // Sweep what previous processes left behind (best-effort; never blocks a session).
    sweep_dead_sessions(&shell_root);

    let bin = session.join("bin");
    if session.exists() {
        // Leftovers from a dead run that had our pid: ours to replace (our own lock is
        // already held, so the sweep above did not touch this directory).
        fs::remove_dir_all(&session).map_err(|err| {
            format!(
                "cannot refresh the session directory {}: {err}",
                session.display()
            )
        })?;
    }
    fs::create_dir_all(&bin).map_err(|err| {
        format!(
            "cannot create the session bin view {}: {err}",
            bin.display()
        )
    })?;

    let mut names: Vec<String> = Vec::new();
    for (name, hash) in store.names().map_err(|err| err.to_string())? {
        place(&store.object_path(&hash), &bin.join(format!("{name}.wasm")))?;
        names.push(name.to_string());
    }

    let dev = Path::new(DEV_COMPONENTS_DIR);
    if dev.is_dir() {
        let listing =
            fs::read_dir(dev).map_err(|err| format!("cannot read {}: {err}", dev.display()))?;
        for entry in listing {
            let path = entry
                .map_err(|err| format!("cannot read {}: {err}", dev.display()))?
                .path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("wasm") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Some(shell_name) = seed::shell_name_for(stem) else {
                continue;
            };
            let target = bin.join(format!("{shell_name}.wasm"));
            if target.exists() {
                continue;
            }
            place(&path, &target)?;
            names.push(shell_name);
        }
    }

    vlog!(
        cfg,
        "session bin view {} holds {} program(s)",
        bin.display(),
        names.len()
    );
    Ok((session, names))
}

/// Put one program into the bin view.
///
/// This must be a *copy*, never a hard link: the session filesystem provider re-verifies
/// every opened file's real path against the session root (containment), and a
/// hard-linked file's kernel-reported path can be any of its links — including another
/// session's bin view or the store object itself, both outside this session's root, which
/// surfaces as a spurious `Denied`. `fs::copy` clones on APFS (copy-on-write), so the
/// per-session cost is an inode, not the bytes.
fn place(source: &Path, target: &Path) -> Result<(), String> {
    fs::copy(source, target).map(|_| ()).map_err(|err| {
        format!(
            "cannot place {} into the session bin view: {err}",
            source.display()
        )
    })
}

/// Remove session directories (and their sibling lock files) whose owning process is
/// gone, plus the legacy shared layout (`shell/bin` + `shell/session` from before
/// sessions were per-process). A session is alive exactly while its `session-<pid>.lock`
/// sibling is held; the lock is always created and acquired *before* the directory (see
/// `materialize_session`), so a directory with an absent or unheld lock is always dead.
/// Best-effort: a sweep failure never stops a session from starting.
fn sweep_dead_sessions(shell_root: &Path) {
    let legacy_bin = shell_root.join("bin");
    if legacy_bin.is_dir() {
        let _ = fs::remove_dir_all(&legacy_bin);
        let _ = fs::remove_file(shell_root.join("session"));
    }
    let Ok(entries) = fs::read_dir(shell_root) else {
        return;
    };
    let our_lock = format!("session-{}.lock", std::process::id());
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_prefix("session-") else {
            continue;
        };
        // Look at lock files only (the dir is handled together with its lock).
        let Some(pid) = stem.strip_suffix(".lock") else {
            continue;
        };
        if name == our_lock {
            continue;
        }
        let lock_path = entry.path();
        let held = fs::File::open(&lock_path).is_ok_and(|file| file.try_lock().is_err());
        if !held {
            let _ = fs::remove_dir_all(shell_root.join(format!("session-{pid}")));
            let _ = fs::remove_file(&lock_path);
        }
    }
    // Directories with no lock file at all (interrupted before the lock existed, or
    // created by something else) are dead by definition — but only sweep ones following
    // our naming scheme, and never our own.
    let Ok(entries) = fs::read_dir(shell_root) else {
        return;
    };
    let ours = format!("session-{}", std::process::id());
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !path.is_dir() || !name.starts_with("session-") || name == ours {
            continue;
        }
        if !shell_root.join(format!("{name}.lock")).exists() {
            let _ = fs::remove_dir_all(&path);
        }
    }
}
