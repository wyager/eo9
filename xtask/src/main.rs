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
const GUEST_COMPONENTS: &[&str] = &[
    "eo9-example-hello",
    "eo9-example-outcomes",
    "eo9-example-cruncher",
    "eo9-example-readwrite",
    "eosh",
    // Standard stub providers (guest/stubs/*, plan/09-providers-stubs.md).
    "eo9-stub-disk-mem",
    "eo9-stub-disk-none",
    "eo9-stub-entropy-none",
    "eo9-stub-entropy-seeded",
    "eo9-stub-fs-memfs",
    "eo9-stub-fs-none",
    "eo9-stub-fs-readonly",
    "eo9-stub-net-deny",
    "eo9-stub-net-none",
    "eo9-stub-perf-none",
    "eo9-stub-perf-null",
    "eo9-stub-text-none",
    "eo9-stub-text-null",
    "eo9-stub-time-frozen",
    "eo9-stub-time-fuzzy",
    "eo9-stub-time-monotonic-stub",
    "eo9-stub-time-none",
];

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
        "build-kernel" => {
            build_kernel(&root, &arch_arg("build-kernel", rest)?)?;
            Ok(())
        }
        "qemu" => qemu(&root, &arch_arg("qemu", rest)?),
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
    build-kernel <arch>  Precompile the seed wasm component and build the bootable kernel
                         image (aarch64 only so far; ELF for QEMU's -kernel loader)
    qemu <arch>          Build the kernel image and boot it under QEMU with serial on stdio
                         (aarch64 only so far; exits when the kernel powers off, Ctrl-A X to quit)
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
    )?;
    // eo9-sched is shared with the bare-metal kernel, so keep it honestly no_std by also
    // checking it against the bare-metal target. This runs after the kernel build so
    // rustup has already ensured that target is installed for the pinned toolchain (the
    // root workspace's rust-toolchain.toml does not list it; the kernel's does).
    run(
        root,
        "cargo",
        ["check", "-p", "eo9-sched", "--target", KERNEL_CHECK_TARGET],
    )
}

fn test(root: &Path) -> Result<(), String> {
    run(root, "cargo", ["test", "--workspace"])?;
    // Kernel unit tests run on the host triple; the placeholder crate is `no_std`
    // except under `cfg(test)`. Guest component crates have no wasm test runner wired
    // up and are exercised by host-side integration tests instead, but eosh-core is a
    // plain no_std library whose unit tests run on the host triple — passed explicitly,
    // because the guest workspace defaults to the wasm target (guest/.cargo/config.toml).
    run(&root.join("kernel"), "cargo", ["test", "--workspace"])?;
    let host = host_triple()?;
    run(
        &root.join("guest"),
        "cargo",
        ["test", "-p", "eosh-core", "--target", host.as_str()],
    )
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
        // The eo9 APIs return Component Model futures, so components built from them
        // use the async canonical built-ins; the validator only accepts those with the
        // cm-async feature enabled.
        run(
            &guest,
            "wasm-tools",
            [
                OsStr::new("validate"),
                OsStr::new("--features"),
                OsStr::new("cm-async"),
                component.as_os_str(),
            ],
        )?;
        println!("xtask: built component {}", component.display());
    }
    Ok(())
}

/// Amount of RAM given to the QEMU guest. Must stay in sync with `RAM_SIZE` in
/// `kernel/eo9-kernel/src/heap.rs`, which hands everything above the image to the heap.
const KERNEL_QEMU_MEMORY: &str = "512M";

/// Build the bootable kernel image for `arch` and return its path.
///
/// For aarch64 this precompiles the seed wasm component (kernel/seed/hello.wat) for the
/// bare-metal target with the host wasmtime, then builds `eo9-kernel` in release mode with
/// the `wasm-seed` feature so the artifact is embedded in the image. The result is an ELF
/// that QEMU's `-kernel` loader boots directly.
fn build_kernel(root: &Path, arch: &str) -> Result<PathBuf, String> {
    if arch != "aarch64" {
        return Err(format!(
            "`build-kernel {arch}` is not implemented yet: the bare-metal spike covers aarch64 \
             only so far (plan/12-kernel.md)"
        ));
    }

    let seed = precompile_seed(root)?;
    let kernel_dir = root.join("kernel");
    run_with_env(
        &kernel_dir,
        "cargo",
        [
            "build",
            "-p",
            "eo9-kernel",
            "--release",
            "--target",
            KERNEL_CHECK_TARGET,
            "--features",
            "wasm-seed",
        ],
        &[("EO9_SEED_CWASM", seed.as_os_str())],
    )?;

    let image = kernel_dir
        .join("target")
        .join(KERNEL_CHECK_TARGET)
        .join("release")
        .join("eo9-kernel");
    if !image.is_file() {
        return Err(format!(
            "kernel build succeeded but {} is missing",
            image.display()
        ));
    }
    println!("xtask: built kernel image {}", image.display());
    Ok(image)
}

/// Assemble and precompile the seed component for the bare-metal target.
///
/// The artifact must be loadable by the kernel's `no_std` wasmtime engine, so the
/// compilation config mirrors what that engine computes for itself on an OS-less target:
/// no signals-based traps, no virtual-memory reservations or guards, no copy-on-write
/// memory initialization, and no wasm proposals beyond what the kernel build enables
/// (feature unification gives this host build GC and threads support via eo9-runtime's
/// wasmtime features; the kernel build has neither).
fn precompile_seed(root: &Path) -> Result<PathBuf, String> {
    let wat_path = root.join("kernel").join("seed").join("hello.wat");
    let component = wat::parse_file(&wat_path)
        .map_err(|err| format!("failed to assemble {}: {err}", wat_path.display()))?;

    let mut config = wasmtime::Config::new();
    config
        .target(KERNEL_CHECK_TARGET)
        .map_err(|err| format!("wasmtime rejected target {KERNEL_CHECK_TARGET}: {err:#}"))?;
    config.wasm_component_model(true);
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(false);
    config.gc_support(false);
    config.wasm_threads(false);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|err| format!("failed to build the seed-precompile engine: {err:#}"))?;
    let artifact = engine
        .precompile_component(&component)
        .map_err(|err| format!("failed to precompile the seed component: {err:#}"))?;

    let out_dir = root.join("kernel").join("target").join("seed");
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join("hello.cwasm");
    std::fs::write(&out_path, &artifact)
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;
    println!(
        "xtask: precompiled seed component {} ({} bytes, target {KERNEL_CHECK_TARGET})",
        out_path.display(),
        artifact.len()
    );
    Ok(out_path)
}

/// Build the kernel image for `arch` and boot it under QEMU with serial on stdio.
///
/// The exact invocation (aarch64): `qemu-system-aarch64 -M virt -cpu max -smp 1 -m 512M
/// -nographic -kernel <image>`. The kernel powers the machine off via PSCI when its run
/// completes (or on panic), so QEMU exits by itself; to quit earlier press Ctrl-A then X.
fn qemu(root: &Path, arch: &str) -> Result<(), String> {
    let image = build_kernel(root, arch)?;
    let qemu = format!("qemu-system-{arch}");
    println!(
        "xtask: booting {} under {qemu} (serial on stdio; the kernel powers off when done, \
         or press Ctrl-A then X to quit)",
        image.display()
    );
    run(
        root,
        &qemu,
        [
            OsStr::new("-M"),
            OsStr::new("virt"),
            OsStr::new("-cpu"),
            OsStr::new("max"),
            OsStr::new("-smp"),
            OsStr::new("1"),
            OsStr::new("-m"),
            OsStr::new(KERNEL_QEMU_MEMORY),
            OsStr::new("-nographic"),
            OsStr::new("-kernel"),
            image.as_os_str(),
        ],
    )
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

/// The host target triple, from `rustc -vV` (needed to run host-side tests inside the
/// guest workspace, which defaults every build to the wasm target).
fn host_triple() -> Result<String, String> {
    let output = Command::new("rustc")
        .arg("-vV")
        // Match `run`: respect each workspace's rust-toolchain.toml pin rather than the
        // toolchain the rustup shim picked for xtask itself.
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .map_err(|err| format!("failed to run `rustc -vV`: {err}"))?;
    if !output.status.success() {
        return Err(format!("`rustc -vV` failed ({})", output.status));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| String::from("`rustc -vV` printed no `host:` line"))
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
    run_with_env(dir, program, args, &[])
}

/// Like [`run`], but with extra environment variables set for the child process.
fn run_with_env<I, S>(
    dir: &Path,
    program: &str,
    args: I,
    envs: &[(&str, &OsStr)],
) -> Result<(), String>
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
        .envs(envs.iter().map(|(key, value)| (key, *value)))
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
