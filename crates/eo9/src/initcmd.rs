//! `eo9 init [config]` — run the service-boot program with the svc capability.
//!
//! init (an ordinary guest program, `guest/init`) is how you run "Eo9 as a service
//! host" on a Unix box: it detaches every service named in the config, then keeps a
//! console (eosh by default) running in the foreground; leaving the console restarts it
//! while services live (owner ruling D), and everything dies with this process (owner
//! ruling E — the registry is process-bound).
//!
//! Without a config argument the built-in default is used: no services, `console = eosh`
//! — i.e. `eo9 init` behaves like `eo9 --svc shell` plus the console-restart rule.

use std::fs;
use std::path::Path;

use eo9_runtime::{NamedArg, ServiceRegistry, SpawnLimits, Task};
use eo9_store::Name;

use crate::cli::{Config, vlog};
use crate::source::ProgramSource;
use crate::{compile, providers, run, seed, shell, source};

/// The built-in config when no file is given.
const DEFAULT_CONFIG: &str =
    "# eo9 init: no config file given — just the console.\nconsole = eosh\n";

pub fn cmd_init(cfg: &Config, config_path: Option<&str>) -> Result<u8, String> {
    let store = cfg.open_store()?;

    if let Err(err) = seed::ensure_seeded(cfg, &store) {
        eprintln!("eo9: warning: could not seed/refresh the module store: {err}");
    }

    // The config: a file, or the built-in default.
    let mut config_text = match config_path {
        Some(path) => fs::read_to_string(path)
            .map_err(|err| format!("cannot read the init config `{path}`: {err}"))?,
        None => DEFAULT_CONFIG.to_string(),
    };

    // Scripted sessions (piped stdin) get `console-restart = never` unless the config
    // says otherwise: a console reading from an exhausted pipe exits immediately, so
    // ruling D's restart-while-services-live would loop forever. Interactive terminals
    // keep the ruling-D default (`always`).
    let user_set_console_restart = config_text
        .lines()
        .any(|line| line.trim_start().starts_with("console-restart"));
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) && !user_set_console_restart {
        vlog!(
            cfg,
            "stdin is not a terminal: defaulting to `console-restart = never` (the config \
             can override)"
        );
        config_text.push_str("\nconsole-restart = never\n");
    }

    // Resolve the init program: store name first (seeding provides it), then the dev
    // tree, then the embedded copy — the same order as eosh resolution.
    let init_program = resolve_init(cfg, &store)?;

    // The session: same materialization as the shell (programs under /bin, manifest).
    let (session_root, _names) = shell::materialize_session(cfg, &store)?;
    if let Err(err) = fs::write(
        session_root.join("session"),
        providers::session_manifest(cfg),
    ) {
        eprintln!("eo9: warning: cannot write the session manifest (`env` will say less): {err}");
    }

    let loaded = compile::load_image(cfg, &store, &init_program)?;

    // The registry: process-bound (ruling E). init gets svc, and so does its console
    // (so `svc list` / `svc stop` work there); the console's children do not (ruling B).
    let registry = ServiceRegistry::new(loaded.image.engine());
    let init_providers = providers::init_providers(cfg, &session_root, &loaded.image, &registry)?;

    vlog!(
        cfg,
        "init starts with the svc capability; its console holds it too; programs run \
         from that console do not"
    );

    // init's single argument: the config text.
    let args = [NamedArg::new("config", run::wave_string(&config_text))];
    let limits = SpawnLimits {
        max_memory: cfg.max_memory,
        max_table_elements: None,
    };
    let mut task = Task::spawn(&loaded.image, &args, limits, init_providers).map_err(|err| {
        run::stale_store_hint(
            &init_program.origin,
            format!("cannot spawn init ({}): {err}", init_program.origin),
        )
    })?;

    let outcome = run::drive_with_services(cfg, &mut task, &registry);

    // Teardown (ruling E): whatever is still alive dies with this process, named.
    {
        let mut registry = registry.lock().unwrap();
        if registry.any_alive() {
            eprintln!(
                "eo9: init exited; stopping the services still running (services live \
                 only as long as this eo9 process)"
            );
            for service in registry.list() {
                if service.state != eo9_runtime::ServiceState::Finished {
                    eprintln!("eo9:   stopped: {}", service.name);
                }
            }
        }
        registry.stop_all();
    }

    let (rendered, code) = run::render_outcome(&outcome);
    match &outcome {
        eo9_runtime::Outcome::Success(_) => vlog!(cfg, "init outcome: {rendered}"),
        _ => run::print_outcome(cfg, &rendered),
    }
    Ok(code)
}

/// Locate the init component: the store-bound name `init`, then the dev-tree artifact,
/// then the copy embedded in this binary.
fn resolve_init(cfg: &Config, store: &eo9_store::Store) -> Result<ProgramSource, String> {
    let name = Name::parse("init").expect("`init` is a valid store name");
    let bound = store
        .lookup_name_in(eo9_store::DEFAULT_PROFILE, &name)
        .map_err(|err| err.to_string())?;
    if bound.is_some() {
        return source::resolve_program(cfg, "init");
    }

    let dev = Path::new(shell::DEV_COMPONENTS_DIR).join("init.wasm");
    if dev.is_file() {
        return source::resolve_program(cfg, &dev.display().to_string());
    }

    if let Some(bytes) = seed::embedded("init") {
        vlog!(cfg, "using the init component embedded in this binary");
        return Ok(ProgramSource {
            bytes: bytes.to_vec(),
            hash: eo9_store::ObjectHash::of(bytes),
            origin: "init (embedded in the eo9 binary)".to_string(),
        });
    }

    Err(format!(
        "cannot find the init program: it is not bound in the module store, not in the dev \
         tree (`cargo xtask build-guest`, which produces {}/init.wasm), and not embedded in \
         this binary — run `eo9 store reseed`, or build the guest components",
        shell::DEV_COMPONENTS_DIR
    ))
}
