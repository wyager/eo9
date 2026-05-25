//! Build orchestration for the Eo9 repository.
//!
//! The repo contains three Cargo workspaces (host at the repo root, `guest/`, `kernel/`);
//! this tool is the one entry point that drives all of them. Run it as
//! `cargo xtask <command>` (alias in `.cargo/config.toml`) or `cargo run -p xtask -- <command>`.
//!
//! The CI gate used by reviewer agents is `cargo xtask ci`.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Guest crates that `build-guest` turns into wasm components (package names).
const GUEST_COMPONENTS: &[&str] = &["eo9-guest-placeholder"];

/// Target used to build guest crates before componentizing them.
const GUEST_TARGET: &str = "wasm32-unknown-unknown";

/// Bare-metal target used to keep the kernel workspace honest about `no_std`
/// until area 12 introduces the real per-arch targets.
const KERNEL_CHECK_TARGET: &str = "aarch64-unknown-none";

/// Architectures accepted by `build-kernel` and `qemu` (QEMU bring-up order).
const KERNEL_ARCHES: &[&str] = &["aarch64", "riscv64", "x86_64"];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String]) -> Result<(), String> {
    let root = repo_root();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = args.get(1..).unwrap_or(&[]);
    match cmd {
        "build" => {
            expect_no_args("build", rest)?;
            build(&root)
        }
        "test" => {
            expect_no_args("test", rest)?;
            test(&root)
        }
        "build-guest" => {
            expect_no_args("build-guest", rest)?;
            build_guest(&root)
        }
        "build-kernel" => build_kernel(&arch_arg("build-kernel", rest)?),
        "qemu" => qemu(&arch_arg("qemu", rest)?),
        "fmt" => fmt(&root, check_flag("fmt", rest)?),
        "lint" => {
            expect_no_args("lint", rest)?;
            lint(&root)
        }
        "ci" => {
            expect_no_args("ci", rest)?;
            ci(&root)
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command `{other}`; run `cargo xtask help`")),
    }
}

fn print_help() {
    println!(
        "xtask — build orchestration across the eo9 host, guest, and kernel workspaces

USAGE:
    cargo xtask <command>

COMMANDS:
    build                Build the host workspace and the kernel workspace ({KERNEL_CHECK_TARGET})
    test                 Run host workspace tests and kernel workspace tests (host triple)
    build-guest          Build guest crates for {GUEST_TARGET} and componentize them with
                         `wasm-tools component new` into guest/target/components/*.wasm
    build-kernel <arch>  Build a bootable kernel image (not implemented yet — area 12)
    qemu <arch>          Boot a kernel image under QEMU (not implemented yet — area 12)
    fmt [--check]        Run `cargo fmt --all` in all three workspaces
    lint                 Run `cargo clippy -D warnings` in all three workspaces
    ci                   The merge gate: fmt --check, lint, build, test, build-guest
    help                 Show this message

ARCHES: {}",
        KERNEL_ARCHES.join(", ")
    );
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn build(root: &Path) -> Result<(), String> {
    run(root, "cargo", ["build", "--workspace"])?;
    run(
        &root.join("kernel"),
        "cargo",
        ["build", "--workspace", "--target", KERNEL_CHECK_TARGET],
    )
}

fn test(root: &Path) -> Result<(), String> {
    run(root, "cargo", ["test", "--workspace"])?;
    // Kernel unit tests run on the host triple; the placeholder crate is `no_std`
    // except under `cfg(test)`. Guest crates have no wasm test runner wired up and are
    // exercised by host-side integration tests instead.
    run(&root.join("kernel"), "cargo", ["test", "--workspace"])
}

fn build_guest(root: &Path) -> Result<(), String> {
    let guest = root.join("guest");
    run(
        &guest,
        "cargo",
        [
            "build",
            "--workspace",
            "--release",
            "--target",
            GUEST_TARGET,
        ],
    )?;

    let components_dir = guest.join("target").join("components");
    std::fs::create_dir_all(&components_dir)
        .map_err(|err| format!("failed to create {}: {err}", components_dir.display()))?;

    for package in GUEST_COMPONENTS {
        let module = guest
            .join("target")
            .join(GUEST_TARGET)
            .join("release")
            .join(format!("{}.wasm", package.replace('-', "_")));
        let component = components_dir.join(format!("{package}.wasm"));
        run(
            &guest,
            "wasm-tools",
            [
                OsStr::new("component"),
                OsStr::new("new"),
                module.as_os_str(),
                OsStr::new("-o"),
                component.as_os_str(),
            ],
        )?;
        run(
            &guest,
            "wasm-tools",
            [OsStr::new("validate"), component.as_os_str()],
        )?;
        println!("xtask: built component {}", component.display());
    }
    Ok(())
}

fn build_kernel(arch: &str) -> Result<(), String> {
    Err(format!(
        "`build-kernel {arch}` is not implemented yet: bootable kernel images are area 12 \
         (plan/12-kernel.md); the kernel workspace currently holds only a placeholder crate"
    ))
}

fn qemu(arch: &str) -> Result<(), String> {
    Err(format!(
        "`qemu {arch}` is not implemented yet: QEMU images and launch scripts are area 12 \
         (plan/12-kernel.md)"
    ))
}

fn fmt(root: &Path, check: bool) -> Result<(), String> {
    let mut args = vec!["fmt", "--all"];
    if check {
        args.push("--check");
    }
    for dir in workspaces(root) {
        run(&dir, "cargo", args.clone())?;
    }
    Ok(())
}

fn lint(root: &Path) -> Result<(), String> {
    run(
        root,
        "cargo",
        [
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run(
        &root.join("guest"),
        "cargo",
        [
            "clippy",
            "--workspace",
            "--target",
            GUEST_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run(
        &root.join("kernel"),
        "cargo",
        [
            "clippy",
            "--workspace",
            "--target",
            KERNEL_CHECK_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// The merge gate (plan/01-workspace.md): everything a reviewer agent runs before merging.
fn ci(root: &Path) -> Result<(), String> {
    fmt(root, true)?;
    lint(root)?;
    build(root)?;
    test(root)?;
    build_guest(root)?;
    println!("xtask: ci passed (fmt, lint, build, test, build-guest)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The repository root: xtask always lives at `<root>/xtask`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live one level below the repository root")
        .to_path_buf()
}

/// The three workspace roots, in the order they are formatted/linted.
fn workspaces(root: &Path) -> [PathBuf; 3] {
    [root.to_path_buf(), root.join("guest"), root.join("kernel")]
}

fn expect_no_args(cmd: &str, rest: &[String]) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "`{cmd}` takes no arguments (got `{}`)",
            rest.join(" ")
        ))
    }
}

fn arch_arg(cmd: &str, rest: &[String]) -> Result<String, String> {
    match rest {
        [arch] if KERNEL_ARCHES.contains(&arch.as_str()) => Ok(arch.clone()),
        [arch] => Err(format!(
            "unknown arch `{arch}` for `{cmd}`; expected one of: {}",
            KERNEL_ARCHES.join(", ")
        )),
        _ => Err(format!(
            "`{cmd}` takes exactly one argument: an arch ({})",
            KERNEL_ARCHES.join(", ")
        )),
    }
}

fn check_flag(cmd: &str, rest: &[String]) -> Result<bool, String> {
    match rest {
        [] => Ok(false),
        [flag] if flag == "--check" => Ok(true),
        _ => Err(format!("`{cmd}` accepts only an optional `--check` flag")),
    }
}

/// Run a command in `dir`, streaming its output, and fail on a non-zero exit status.
fn run<I, S>(dir: &Path, program: &str, args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let shown: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let shown = shown.join(" ");
    println!("xtask: [{}] {program} {shown}", dir.display());

    let status = Command::new(program)
        .args(&args)
        .current_dir(dir)
        // Each workspace pins its toolchain via rust-toolchain.toml; drop the variable the
        // rustup shim set for the xtask build so child cargo invocations respect the pin of
        // the workspace they run in rather than inheriting xtask's toolchain.
        .env_remove("RUSTUP_TOOLCHAIN")
        .status()
        .map_err(|err| format!("failed to run `{program} {shown}`: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{program} {shown}` failed ({status}) in {}",
            dir.display()
        ))
    }
}
