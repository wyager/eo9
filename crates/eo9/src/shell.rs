//! `eo9 shell`: run eosh, the Eo9 shell, as an ordinary Eo9 program.
//!
//! The shell has no private powers (SPEC.md "Shell"): eosh is a guest component that
//! imports `eo9:exec`, `eo9:text`, and `eo9:fs`, and this command is just the embedder
//! that builds its **session**:
//!
//! * a session directory under the store root whose `bin/` holds one `<name>.wasm` per
//!   bound store name (plus the dev-tree example/stub components), because eosh resolves
//!   program names as `/bin/<name>.wasm` on its granted filesystem;
//! * the usual root providers (terminal stdio, host clocks, OS RNG), an fs rooted at that
//!   session directory, and the exec capability whose child policy grants children the
//!   same session roots a direct `eo9 run` would get — never exec itself;
//! * the existing drive loop; interactive when no command was given, one-shot with `-c`.
//!
//! Known limitation (runtime escalation E5): children execute inside the shell's own
//! resume donations, so a long-running child throttles the shell until it finishes.

use std::fs;
use std::path::{Path, PathBuf};

use eo9_runtime::{NamedArg, Outcome, SpawnLimits, Task};
use eo9_store::{Name, Store};

use crate::cli::{Config, vlog};
use crate::compile;
use crate::providers;
use crate::run;
use crate::source::{self, ProgramSource};

/// Where `cargo xtask build-guest` puts components in a development tree, relative to the
/// current directory: the fallback source for eosh itself and the fill-in source for the
/// session bin view.
const DEV_COMPONENTS_DIR: &str = "guest/target/components";

pub fn cmd_shell(cfg: &Config, command: Option<String>) -> Result<u8, String> {
    let store = cfg.open_store()?;

    let eosh = resolve_eosh(cfg, &store)?;
    let session_root = materialize_session(cfg, &store)?;

    let loaded = compile::load_image(cfg, &store, &eosh)?;
    let shell_providers = providers::shell_providers(cfg, &session_root, &loaded.image)?;

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
    let mut task = Task::spawn(&loaded.image, &args, limits, shell_providers)
        .map_err(|err| format!("cannot spawn eosh ({}): {err}", eosh.origin))?;

    let outcome = run::drive_to_completion(cfg, &mut task);
    let (rendered, code) = run::render_outcome(&outcome);
    match &outcome {
        // A clean shell exit stays quiet: everything worth seeing was already printed by
        // eosh (and its children) through the text capability.
        Outcome::Success(_) => vlog!(cfg, "shell outcome: {rendered}"),
        _ => println!("{rendered}"),
    }
    Ok(code)
}

/// Locate the eosh component. Lookup order: the store-bound name `eosh` (the installed
/// form), then the dev-tree artifact `guest/target/components/eosh.wasm` relative to the
/// current directory (the checkout convenience).
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

    Err(format!(
        "cannot find the eosh component: bind it in the store \
         (`eo9 store add <path-to-eosh.wasm> --name eosh`) or build it in a development \
         tree (`cargo xtask build-guest`, which produces {DEV_COMPONENTS_DIR}/eosh.wasm), \
         then run `eo9 shell` again"
    ))
}

/// Build (refreshing on every shell start) the session directory the shell's filesystem
/// is rooted at: `<store-root>/shell/bin/<name>.wasm`, one entry per bound store name —
/// hard-linked to the store object when possible, copied otherwise — plus the dev-tree
/// components under the names they answer to in a shell (`hello`, `entropy.seeded`, …).
/// Store bindings win over dev-tree components of the same name.
fn materialize_session(cfg: &Config, store: &Store) -> Result<PathBuf, String> {
    let session = store.root().join("shell");
    let bin = session.join("bin");
    if bin.exists() {
        fs::remove_dir_all(&bin).map_err(|err| {
            format!(
                "cannot refresh the session bin view {}: {err}",
                bin.display()
            )
        })?;
    }
    fs::create_dir_all(&bin).map_err(|err| {
        format!(
            "cannot create the session bin view {}: {err}",
            bin.display()
        )
    })?;

    let mut entries = 0usize;
    for (name, hash) in store.names().map_err(|err| err.to_string())? {
        place(&store.object_path(&hash), &bin.join(format!("{name}.wasm")))?;
        entries += 1;
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
            let Some(shell_name) = dev_shell_name(stem) else {
                continue;
            };
            let target = bin.join(format!("{shell_name}.wasm"));
            if target.exists() {
                continue;
            }
            place(&path, &target)?;
            entries += 1;
        }
    }

    vlog!(
        cfg,
        "session bin view {} holds {entries} program(s)",
        bin.display()
    );
    Ok(session)
}

/// Put one program into the bin view: hard-link when source and view share a filesystem
/// (the store objects always do), copy otherwise.
fn place(source: &Path, target: &Path) -> Result<(), String> {
    if fs::hard_link(source, target).is_ok() {
        return Ok(());
    }
    fs::copy(source, target).map(|_| ()).map_err(|err| {
        format!(
            "cannot place {} into the session bin view: {err}",
            source.display()
        )
    })
}

/// The shell name a dev-tree component file answers to: `eo9-example-hello` → `hello`,
/// `eo9-stub-time-frozen` → `time.frozen`, anything else (`eosh`) verbatim. `None` when
/// the result would not be a valid dotted name.
fn dev_shell_name(stem: &str) -> Option<String> {
    let name = if let Some(example) = stem.strip_prefix("eo9-example-") {
        example.to_string()
    } else if let Some(stub) = stem.strip_prefix("eo9-stub-") {
        match stub.split_once('-') {
            Some((api, flavor)) => format!("{api}.{flavor}"),
            None => stub.to_string(),
        }
    } else {
        stem.to_string()
    };
    Name::parse(&name).ok().map(|_| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_components_answer_to_their_shell_names() {
        assert_eq!(
            dev_shell_name("eo9-example-hello").as_deref(),
            Some("hello")
        );
        assert_eq!(
            dev_shell_name("eo9-example-readwrite").as_deref(),
            Some("readwrite")
        );
        assert_eq!(
            dev_shell_name("eo9-stub-entropy-seeded").as_deref(),
            Some("entropy.seeded")
        );
        assert_eq!(
            dev_shell_name("eo9-stub-time-monotonic-stub").as_deref(),
            Some("time.monotonic-stub")
        );
        assert_eq!(dev_shell_name("eosh").as_deref(), Some("eosh"));
        // Something that cannot be a dotted name is skipped rather than mis-bound.
        assert_eq!(dev_shell_name("Not A Name"), None);
    }
}
