//! The usermode `eo9` binary: the embedder that assembles the runtime, the module store,
//! and the unix root providers into a running usermode Eo9 instance (SPEC.md "Usermode
//! binary"; plan/11-usermode.md).
//!
//! Command map:
//!
//! * `run`      — resolve (store name or path) → compile through the compile cache →
//!   spawn with the unix root providers → drive to completion → print the WAVE outcome.
//! * `describe` — the component algebra's `describe` over a name or path.
//! * `compile`  — warm the compile cache without running anything.
//! * `store`    — `add` / `ls` / `gc` on the content-addressed module store.
//! * `shell`    — run eosh (the Eo9 shell, itself a guest component) against a session:
//!   a `/bin` name view of the store, the terminal, and the exec capability.
//!
//! Exit codes for `run` and `shell` mirror the three-way program outcome: 0 success,
//! 1 failure, 2 abnormal (trap or kill); 3 means eo9 itself failed before an outcome
//! existed.

mod cli;
mod compile;
mod describe;
mod providers;
mod run;
mod shell;
mod source;
mod storecmd;

use std::process::ExitCode;

use cli::{ArgStream, Config, EXIT_ERROR, EXIT_SUCCESS};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(args) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("eo9: error: {message}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn dispatch(args: Vec<String>) -> Result<u8, String> {
    let mut stream = ArgStream::new(args);
    let mut cfg = Config::default();
    cli::consume_global_options(&mut stream, &mut cfg)?;

    let Some(command) = stream.next() else {
        print_help();
        return Ok(EXIT_ERROR);
    };
    match command.as_str() {
        "run" => {
            cli::consume_global_options(&mut stream, &mut cfg)?;
            let target = program_reference(&mut stream, "run")?;
            let flags = cli::parse_program_flags(&mut stream)?;
            run::cmd_run(&cfg, &target, &flags)
        }
        "describe" => {
            cli::consume_global_options(&mut stream, &mut cfg)?;
            let target = program_reference(&mut stream, "describe")?;
            cli::expect_end(&mut stream, "describe")?;
            describe::cmd_describe(&cfg, &target)
        }
        "compile" => {
            cli::consume_global_options(&mut stream, &mut cfg)?;
            let target = program_reference(&mut stream, "compile")?;
            cli::expect_end(&mut stream, "compile")?;
            compile::cmd_compile(&cfg, &target)
        }
        "store" => storecmd::cmd_store(&mut stream, &mut cfg),
        "shell" => {
            let command = cli::parse_shell_args(&mut stream, &mut cfg)?;
            shell::cmd_shell(&cfg, command)
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(EXIT_SUCCESS)
        }
        other => Err(format!("unknown command `{other}`; run `eo9 help`")),
    }
}

/// The positional program reference of `run` / `describe` / `compile`.
fn program_reference(stream: &mut ArgStream, command: &str) -> Result<String, String> {
    match stream.next() {
        Some(target) if !target.starts_with('-') => Ok(target),
        Some(option) => Err(format!("unknown option `{option}` for `{command}`")),
        None => Err(format!("`{command}` needs a program name or path")),
    }
}

fn print_help() {
    println!(
        "eo9 — a usermode Eo9 instance: compile and run Eo9 programs on the host OS

USAGE:
    eo9 [OPTIONS] <COMMAND> [ARGS]

COMMANDS:
    run <name-or-path> [--<flag> <value> ...]
                              Resolve a program (bare dotted store name, or a path to a
                              component), compile it through the compile cache, run it
                              against the unix root providers, and print its outcome as WAVE
    describe <name-or-path>   Show a component's kind, imports, exports, and arguments
    compile <name-or-path>    Compile a program and warm the compile cache
    store add <path> [--name <dotted-name>]
                              Add a component file to the module store, optionally binding a name
    store ls                  List name bindings, objects, and compile-cache entries
    store gc [--max-cache-bytes <n>]
                              Evict compile-cache entries down to a size budget
    shell [-c <command>]      Run eosh, the Eo9 shell: interactive REPL on the terminal, or
                              one command line with -c (programs resolve from the store's
                              bound names; --fs-root governs what children may touch)
    help                      Show this message

OPTIONS (before the program name; `--<flag> <value>` after it belongs to the program):
    -v, --verbose             Diagnostics on stderr
        --store <path>        Module store root (default: $EO9_STORE, else ~/.eo9/store)
        --fs-root <dir>       Grant the program the eo9:fs capability, rooted at <dir>
                              (no filesystem access without it; guest paths cannot escape it)
        --exec-snapshot <clone-or-refuse|clone-or-copy>
                              How open-exec snapshots a path (default: clone-or-refuse)
        --max-memory <bytes>  Linear-memory ceiling for the spawned task
        --debug-info          Compile with debug info

EXIT CODES (run):
    0  program success        1  program failure
    2  abnormal (trap/kill)   3  eo9 error before the program produced an outcome"
    );
}
