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
    "eo9-example-sockcheck",
    "eo9-example-lspci",
    "eo9-example-l2check",
    "eo9-example-l4check",
    "eosh",
    // Basic coreutils (guest/coreutils/*, plan/17-coreutils.md).
    "eo9-coreutil-cat",
    "eo9-coreutil-ls",
    "eo9-coreutil-find",
    "eo9-coreutil-wc",
    "eo9-coreutil-head",
    "eo9-coreutil-stat",
    "eo9-coreutil-mkdir",
    "eo9-coreutil-rm",
    "eo9-coreutil-cp",
    "eo9-coreutil-touch",
    "eo9-coreutil-echo",
    "eo9-coreutil-rng",
    // Standard stub providers (guest/stubs/*, plan/09-providers-stubs.md).
    "eo9-stub-disk-mem",
    "eo9-stub-disk-none",
    "eo9-stub-disk-virtio",
    "eo9-stub-entropy-none",
    "eo9-stub-entropy-seeded",
    "eo9-stub-fs-eofs",
    "eo9-stub-fs-memfs",
    "eo9-stub-fs-none",
    "eo9-stub-fs-overlay",
    "eo9-stub-fs-readonly",
    "eo9-stub-net-l2-deny",
    "eo9-stub-net-l2-none",
    "eo9-stub-net-l3-deny",
    "eo9-stub-net-l3-none",
    "eo9-stub-net-l4-deny",
    "eo9-stub-net-l4-loopback",
    "eo9-stub-net-l4-none",
    "eo9-stub-net-l4-over-l2",
    "eo9-stub-net-virtio",
    "eo9-stub-pci-none",
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

/// The riscv64 bare-metal target (QEMU `virt`, S-mode under OpenSBI). Host-AOT artifacts
/// for it are produced by the same precompile pipeline as aarch64, but emitting riscv64
/// machine code needs the non-host Cranelift backends, which only the `kernel-cross-aot`
/// xtask feature links (`build-kernel riscv64` re-runs itself with that feature when
/// needed, so plain xtask builds stay lean).
const KERNEL_RISCV64_TARGET: &str = "riscv64gc-unknown-none-elf";

/// The x86_64 bare-metal target (QEMU `q35`, PVH direct boot). `build-kernel x86_64` runs
/// the same host-AOT precompile pipeline as the other ports (Cranelift's x86_64 backend is
/// the host backend, so no extra feature is needed) and builds the wasm feature set minus
/// on-target codegen, which arrives with the W^X milestone (plan/12).
const KERNEL_X86_64_TARGET: &str = "x86_64-unknown-none";

/// Bare-metal targets the feature-less kernel workspace is built and clippy-checked for in
/// `build`/`lint` (and therefore `ci`), so a change cannot silently break a ported
/// architecture. Only aarch64 additionally gets the full wasm feature set exercised under
/// QEMU (`build-kernel` / `qemu`); riscv64 is the second full port, x86_64 the in-progress
/// third (plan/12).
const KERNEL_CI_TARGETS: &[&str] = &[
    "aarch64-unknown-none",
    "riscv64gc-unknown-none-elf",
    "x86_64-unknown-none",
];

/// Architectures accepted by `build-kernel` and `qemu` (QEMU bring-up order).
const KERNEL_ARCHES: &[&str] = &["aarch64", "riscv64", "x86_64"];

/// The wasm-tools CLI family the repo is pinned to (plan/01 Decisions: the 0.250 crate
/// family ships as CLI 1.250.x). `doctor` warns — but does not fail — on a mismatch.
const PINNED_WASM_TOOLS_CLI: &str = "1.250";

/// Minimum node major version needed by the /vm verify harnesses (they rely on JSPI).
const MIN_NODE_MAJOR: u32 = 25;

/// Components baked into the kernel's read-only store image: (guest package, shell name).
/// The shell names follow the same convention the usermode store seeding uses
/// (`eo9-example-hello` → `hello`, `eo9-stub-entropy-seeded` → `entropy.seeded`).
const KERNEL_STORE_COMPONENTS: &[(&str, &str)] = &[
    ("eosh", "eosh"),
    ("eo9-example-hello", "hello"),
    ("eo9-example-outcomes", "outcomes"),
    ("eo9-example-cruncher", "cruncher"),
    ("eo9-example-readwrite", "readwrite"),
    ("eo9-example-lspci", "lspci"),
    ("eo9-stub-entropy-seeded", "entropy.seeded"),
    ("eo9-stub-time-frozen", "time.frozen"),
    // The storage stack for real hardware: the virtio-blk driver and the eofs filesystem,
    // so the metal shell can compose `disk.virtio $ fs.eofs $ <program>` against a QEMU
    // virtio disk (boot with the `pci` grant and the xtask `disk` flag).
    ("eo9-stub-disk-virtio", "disk.virtio"),
    ("eo9-stub-fs-eofs", "fs.eofs"),
    // The network stack for real hardware: the virtio-net driver, its link-layer
    // check, the TCP/IP middleware, and its transport-layer check, so the metal shell
    // can compose `net.virtio $ l2check` and `net.virtio $ net.l4.over-l2 $ l4check`
    // against a QEMU user-mode NIC (boot with the `pci` grant and the xtask `net` flag).
    ("eo9-stub-net-virtio", "net.virtio"),
    ("eo9-example-l2check", "l2check"),
    ("eo9-stub-net-l4-over-l2", "net.l4.over-l2"),
    ("eo9-example-l4check", "l4check"),
    // Basic coreutils, so the metal shell can inspect its own (read-only) filesystem:
    // `ls /bin`, `cat /session`, `wc`, `head`, `stat`.
    ("eo9-coreutil-ls", "ls"),
    ("eo9-coreutil-cat", "cat"),
    ("eo9-coreutil-echo", "echo"),
    ("eo9-coreutil-wc", "wc"),
    ("eo9-coreutil-head", "head"),
    ("eo9-coreutil-stat", "stat"),
];

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
            // Refresh the guest components first: the host integration tests consume the
            // prebuilt components under guest/target/components, and running them against
            // stale artifacts has bitten before (see plan/01 Decisions).
            build_guest(&root)?;
            test(&root)
        }
        "build-guest" => {
            expect_no_args("build-guest", rest)?;
            build_guest(&root)
        }
        "build-web-vm" => {
            expect_no_args("build-web-vm", rest)?;
            build_web_vm(&root)
        }
        "check-web-vm" => {
            expect_no_args("check-web-vm", rest)?;
            check_web_vm(&root)
        }
        "precompress-site" => {
            expect_no_args("precompress-site", rest)?;
            precompress_site(&root)
        }
        "fingerprint-web-vm" => {
            expect_no_args("fingerprint-web-vm", rest)?;
            fingerprint_web_vm(&root)
        }
        "build-kernel" => {
            build_kernel(&root, &arch_arg("build-kernel", rest)?)?;
            Ok(())
        }
        "qemu" => {
            let Some((arch, append)) = rest.split_first() else {
                return Err(
                    "qemu: expected an architecture argument (e.g. `cargo xtask qemu aarch64`)"
                        .to_string(),
                );
            };
            if !KERNEL_ARCHES.contains(&arch.as_str()) {
                return Err(format!(
                    "qemu: unknown architecture `{arch}` (expected one of {KERNEL_ARCHES:?})"
                ));
            }
            qemu(&root, arch, append)
        }
        "fmt" => fmt(&root, check_flag("fmt", rest)?),
        "lint" => {
            expect_no_args("lint", rest)?;
            lint(&root)
        }
        "ci" => {
            expect_no_args("ci", rest)?;
            ci(&root)
        }
        "doctor" => {
            expect_no_args("doctor", rest)?;
            doctor(&root)
        }
        "refresh-components" => {
            expect_no_args("refresh-components", rest)?;
            refresh_components(&root)
        }
        "package" => {
            expect_no_args("package", rest)?;
            package(&root)
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
    build                Build the host workspace and the feature-less kernel workspace for every
                         ported bare-metal target
    test                 Refresh the guest components (build-guest), then run host workspace
                         tests and kernel workspace tests (host triple)
    build-guest          Build guest crates for {GUEST_TARGET} and componentize them with
                         `wasm-tools component new` into guest/target/components/*.wasm
    build-web-vm         Pre-AOT the web-VM demo components to pulley32, build the wasm32
                         blob (www/web-eo9, the real runtime stack for the /vm page), and
                         install it into www/site/vm/ (commit the result; ci does not need it)
    check-web-vm         Rebuild the /vm blob and store artifacts to a temp staging dir and
                         byte-compare against the committed www/site/vm/ files; nonzero exit
                         if they drifted (run after changing guest sources; ci does not need it)
    precompress-site     Write brotli/gzip siblings next to the compressible files under
                         www/site via www/precompress, so the server can serve pre-compressed
                         bytes (runs automatically at the end of the build-web-* commands;
                         commit the result; ci does not need it)
    fingerprint-web-vm   Rename the /vm immutable assets (the wasm blob and .cwasm store
                         images) to carry a content hash, write vm/assets.json, and drop the
                         old siblings — so they can be cached forever and a rebuild changes the
                         URL (runs automatically inside build-web-vm; commit the result)
    check-web-vm         Verify vm/assets.json matches the committed fingerprinted /vm assets
                         (the names encode the current content hash) — a drift guard; ci does
                         not run it (needs a built blob)
    build-kernel <arch>  Build the bootable kernel image (an ELF for QEMU's -kernel loader).
                         aarch64: precompiles the seed/async canaries, eo9-example-hello,
                         entropy.seeded, and the store image and embeds them; riscv64: the
                         feature-less image (boot/serial/heap/timer/interrupts so far)
    qemu <arch>          Build the kernel image and boot it under QEMU with serial on stdio
                         (aarch64 or riscv64; exits when the kernel powers off, Ctrl-A X to quit)
    fmt [--check]        Run `cargo fmt --all` in all three workspaces
    lint                 Run `cargo clippy -D warnings` in all three workspaces
    ci                   The merge gate: fmt --check, lint, build, build-guest, test
    doctor               Check the host prerequisites (rustup, the pinned nightly, the wasm32
                         target, the wasm-tools CLI; QEMU and node are optional) and print
                         install hints for anything missing
    refresh-components   Copy the built guest components into crates/eo9-components/data/ and
                         regenerate its index — the prebuilt set a `cargo install eo9` build
                         seeds from (run after build-guest; commit the result)
    package              Publishing pre-flight: build-guest, verify crates/eo9-components/data/
                         matches the freshly built components, assemble every publishable crate
                         with `cargo package`, dry-run-publish the leaf crates, and print the
                         exact `cargo publish` sequence (nothing is uploaded)
    help                 Show this message

ARCHES: {}",
        KERNEL_ARCHES.join(", ")
    );
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `cargo xtask doctor`: check the host tools and toolchains the repo needs and print an
/// install hint for anything missing. Required: rustup, the wasm32 guest target on the
/// pinned toolchain, the `wasm-tools` CLI. Informational: the pinned nightly and the
/// bare-metal target (rustup installs both automatically when first needed), QEMU (only for
/// `make qemu`), and node ≥ {MIN_NODE_MAJOR} (only for the /vm verify harnesses).
fn doctor(root: &Path) -> Result<(), String> {
    println!("xtask doctor — checking the host tools and toolchains this repository needs\n");
    let mut missing: Vec<&str> = Vec::new();

    // rustup itself.
    let have_rustup = probe(root, "rustup", &["--version"]).is_some();
    if have_rustup {
        println!("  ok       rustup");
    } else {
        println!("  MISSING  rustup — install it from https://rustup.rs");
        missing.push("rustup");
    }

    // The pinned nightly (informational: rustup auto-installs it on the first build).
    let channel = pinned_channel(root);
    let mut toolchain_installed = false;
    if have_rustup {
        match &channel {
            Some(channel) => {
                toolchain_installed = probe(root, "rustup", &["toolchain", "list"])
                    .map(|out| out.lines().any(|line| line.starts_with(channel.as_str())))
                    .unwrap_or(false);
                if toolchain_installed {
                    println!("  ok       pinned toolchain {channel}");
                } else {
                    println!(
                        "  note     pinned toolchain {channel} is not installed yet — rustup installs \
                         it automatically on the first build (or run `rustup toolchain install {channel}`)"
                    );
                }
            }
            None => println!(
                "  warn     could not read the pinned channel from rust-toolchain.toml — \
                 toolchain checks skipped"
            ),
        }
    }

    // Targets on the pinned (root-resolved) toolchain. build-guest and the web demos need
    // the wasm32 target on the root pin (guest/ and kernel/ declare their own targets in
    // their rust-toolchain.toml, so rustup adds those automatically when they are used).
    if have_rustup && toolchain_installed {
        let installed_targets = probe(root, "rustup", &["target", "list", "--installed"]);
        let has_target = |target: &str| {
            installed_targets
                .as_deref()
                .map(|out| out.lines().any(|line| line.trim() == target))
                .unwrap_or(false)
        };
        if has_target(GUEST_TARGET) {
            println!("  ok       {GUEST_TARGET} target");
        } else {
            println!(
                "  MISSING  {GUEST_TARGET} target — run `rustup target add {GUEST_TARGET}` \
                 (or `make setup`)"
            );
            missing.push(GUEST_TARGET);
        }
        if has_target(KERNEL_CHECK_TARGET) {
            println!("  ok       {KERNEL_CHECK_TARGET} target");
        } else {
            println!(
                "  note     {KERNEL_CHECK_TARGET} target not installed yet — \
                 kernel/rust-toolchain.toml declares it, so rustup adds it on the first \
                 `make qemu` / `cargo xtask build-kernel`"
            );
        }
    } else if have_rustup {
        println!("  note     target checks skipped until the pinned toolchain is installed");
    }

    // The wasm-tools CLI componentizes and validates every guest crate (plan/01 D3).
    match probe(root, "wasm-tools", &["--version"]) {
        Some(version) => {
            let version = version.trim().to_string();
            let pinned_family = version
                .strip_prefix("wasm-tools ")
                .map(|v| v.starts_with(PINNED_WASM_TOOLS_CLI))
                .unwrap_or(false);
            if pinned_family {
                println!("  ok       {version}");
            } else {
                println!(
                    "  warn     {version} — the repo is pinned to the {PINNED_WASM_TOOLS_CLI}.x \
                     family (plan/01 Decisions); a newer CLI usually works, but match the pin if \
                     component validation flags complain"
                );
            }
        }
        None => {
            println!(
                "  MISSING  wasm-tools — run `cargo install --locked wasm-tools` (or `make setup`)"
            );
            missing.push("wasm-tools");
        }
    }

    // Optional: QEMU, only needed to boot the bare-metal kernel.
    match probe(root, "qemu-system-aarch64", &["--version"]) {
        Some(version) => println!(
            "  ok       {}",
            version
                .lines()
                .next()
                .unwrap_or("qemu-system-aarch64")
                .trim()
        ),
        None => println!(
            "  optional qemu-system-aarch64 not found — only needed for `make qemu`; install QEMU \
             with your package manager (e.g. `brew install qemu` / `apt install qemu-system-arm`)"
        ),
    }

    // Optional: node, only needed to run the /vm verify harnesses (they rely on JSPI).
    match probe(root, "node", &["--version"]) {
        Some(version) => {
            let version = version.trim().to_string();
            let major = version
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok())
                .unwrap_or(0);
            if major >= MIN_NODE_MAJOR {
                println!("  ok       node {version}");
            } else {
                println!(
                    "  optional node {version} found, but the /vm verify harnesses need \
                     node >= {MIN_NODE_MAJOR} (JSPI)"
                );
            }
        }
        None => println!(
            "  optional node not found — only needed to run the /vm verify harnesses \
             (node >= {MIN_NODE_MAJOR})"
        ),
    }

    println!();
    if missing.is_empty() {
        println!("xtask: doctor: everything required is installed");
        Ok(())
    } else {
        Err(format!(
            "doctor: missing required tools: {} — run `make setup` and re-check",
            missing.join(", ")
        ))
    }
}

/// Run a doctor probe, returning its stdout on success and `None` if the tool could not be
/// spawned or exited non-zero. Probes never fail `doctor` directly — absence is reported.
fn probe(dir: &Path, program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The `channel = "…"` line of the repo-root rust-toolchain.toml, if readable.
fn pinned_channel(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("rust-toolchain.toml")).ok()?;
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("channel")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim();
        Some(rest.trim_matches('"').to_string())
    })
}

fn build(root: &Path) -> Result<(), String> {
    run(root, "cargo", ["build", "--workspace"])?;
    for target in KERNEL_CI_TARGETS {
        run(
            &root.join("kernel"),
            "cargo",
            ["build", "--workspace", "--target", target],
        )?;
    }
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
    )?;
    // The website server workspace (www/): its unit + integration tests are quick and
    // native; the wasm32 blob workspace stays out of the gate (built by build-web-vm).
    run(&root.join("www"), "cargo", ["test", "--workspace"])
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

    for package in GUEST_COMPONENTS {
        componentize_guest_package(root, package)?;
    }
    Ok(())
}

/// Turn one already-built guest crate into a validated component under
/// guest/target/components, returning the component's path.
fn componentize_guest_package(root: &Path, package: &str) -> Result<PathBuf, String> {
    let guest = root.join("guest");
    let components_dir = guest.join("target").join("components");
    std::fs::create_dir_all(&components_dir)
        .map_err(|err| format!("failed to create {}: {err}", components_dir.display()))?;

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
    // cm-async feature enabled. Named same-interface import slots (e.g. fs.overlay's
    // `upper`/`lower`) carry the `implements` annotation — the same encoding the
    // algebra's `rename` produces — which the validator gates behind cm-implements.
    run(
        &guest,
        "wasm-tools",
        [
            OsStr::new("validate"),
            OsStr::new("--features"),
            OsStr::new("cm-async,cm-implements"),
            component.as_os_str(),
        ],
    )?;
    println!("xtask: built component {}", component.display());
    Ok(component)
}

/// Build one guest crate and componentize it (the targeted version of [`build_guest`],
/// used by `build-kernel` to refresh just the program it embeds).
fn build_guest_component(root: &Path, package: &str) -> Result<PathBuf, String> {
    let guest = root.join("guest");
    run(
        &guest,
        "cargo",
        [
            "build",
            "-p",
            package,
            "--release",
            "--target",
            GUEST_TARGET,
        ],
    )?;
    componentize_guest_package(root, package)
}

/// Write brotli/gzip siblings next to the compressible static assets under `www/site`
/// (see `www/precompress`); the server serves them by `Accept-Encoding` negotiation.
fn precompress_site(root: &Path) -> Result<(), String> {
    let manifest = root.join("www").join("precompress").join("Cargo.toml");
    let site = root.join("www").join("site");
    run(
        root,
        "cargo",
        [
            OsStr::new("run"),
            OsStr::new("--release"),
            OsStr::new("--manifest-path"),
            manifest.as_os_str(),
            OsStr::new("--"),
            OsStr::new("--site"),
            site.as_os_str(),
        ],
    )
}

/// Build the in-browser Eo9 VM page's wasm blob (`www/web-eo9`, served at `/vm/`).
///
/// Steps: build the guest components (for `entropy.seeded`), pre-AOT the demo set to
/// `pulley32` artifacts the blob embeds, build the blob for `wasm32-unknown-unknown` in its
/// own workspace (which patches in the vendored wasmtime with the fiberless
/// component-model-async path — wasm32 has no fiber backend), and copy the result to
/// `www/site/vm/web-eo9.wasm`. The output is committed, so this only needs re-running when
/// the demo components, the vendored wasmtime, or the blob source change; `ci` deliberately
/// does not depend on it.
fn build_web_vm(root: &Path) -> Result<(), String> {
    build_guest(root)?;

    // Pre-AOT the demo components to pulley32 with the same compile-relevant settings the
    // blob's engine uses at load time (www/web-eo9/blob/src/lib.rs::base_config).
    let artifacts = root
        .join("www")
        .join("web-eo9")
        .join("blob")
        .join("artifacts");
    std::fs::create_dir_all(&artifacts)
        .map_err(|err| format!("failed to create {}: {err}", artifacts.display()))?;

    let seed_wat = root.join("kernel").join("seed").join("hello.wat");
    let seed_wasm = wat::parse_file(&seed_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", seed_wat.display()))?;
    let entropy_path = root
        .join("guest")
        .join("target")
        .join("components")
        .join("eo9-stub-entropy-seeded.wasm");
    let entropy_wasm = std::fs::read(&entropy_path)
        .map_err(|err| format!("failed to read {}: {err}", entropy_path.display()))?;

    preaot_for_web(
        &artifacts,
        &seed_wasm,
        "seed component",
        "seed.cwasm",
        false,
    )?;
    preaot_for_web(
        &artifacts,
        &seed_wasm,
        "seed component (fuel)",
        "seed-fuel.cwasm",
        true,
    )?;
    preaot_for_web(
        &artifacts,
        &entropy_wasm,
        "entropy.seeded",
        "entropy-seeded.cwasm",
        false,
    )?;

    // The component-algebra demo (plan/18 D15): the blob runs the real `eo9-component`
    // algebra — load/describe/restrict — on a raw component IN THE BROWSER, then executes
    // it via Pulley. Embed the hello example both as raw component bytes (for the algebra)
    // and pre-AOT'd to pulley32 (for execution), so the demo is self-contained.
    let hello_component = std::fs::read(
        root.join("guest")
            .join("target")
            .join("components")
            .join("eo9-example-hello.wasm"),
    )
    .map_err(|err| format!("failed to read the hello example component: {err}"))?;
    std::fs::write(artifacts.join("example-hello.wasm"), &hello_component)
        .map_err(|err| format!("failed to write the raw hello component to artifacts: {err}"))?;
    preaot_for_web(
        &artifacts,
        &hello_component,
        "example hello (algebra demo)",
        "example-hello.cwasm",
        false,
    )?;

    // eosh — the shell itself, booted in the blob against the in-browser eo9:exec surface
    // (plan/18: eosh in the browser). Pre-AOT'd to pulley32 and embedded.
    let eosh_component = std::fs::read(
        root.join("guest")
            .join("target")
            .join("components")
            .join("eosh.wasm"),
    )
    .map_err(|err| format!("failed to read the eosh component: {err}"))?;
    preaot_for_web(
        &artifacts,
        &eosh_component,
        "eosh shell",
        "eosh.cwasm",
        false,
    )?;

    // Programs eosh can resolve from `/bin` in the browser: each as raw component bytes (for
    // the algebra's `load`, seeded into the blob's MemFs) and pre-AOT'd to pulley32 (for
    // execution via the exec surface). hello + a useful spread of coreutils.
    for (name, package) in [
        ("hello", "eo9-example-hello"),
        ("echo", "eo9-coreutil-echo"),
        ("cat", "eo9-coreutil-cat"),
        ("ls", "eo9-coreutil-ls"),
        ("rng", "eo9-coreutil-rng"),
        // Providers in /bin so `provider $ consumer` compositions are formable through eosh
        // (e.g. `entropy.seeded $ rng`, `time.frozen ... $ hello`), compiled in-blob (plan/18 D22).
        ("entropy.seeded", "eo9-stub-entropy-seeded"),
        ("time.frozen", "eo9-stub-time-frozen"),
    ] {
        let raw = std::fs::read(
            root.join("guest")
                .join("target")
                .join("components")
                .join(format!("{package}.wasm")),
        )
        .map_err(|err| format!("failed to read the {name} component for /bin: {err}"))?;
        std::fs::write(artifacts.join(format!("bin-{name}.wasm")), &raw).map_err(|err| {
            format!("failed to write the raw {name} component to artifacts: {err}")
        })?;
        preaot_for_web(
            &artifacts,
            &raw,
            &format!("/bin {name}"),
            &format!("bin-{name}.cwasm"),
            false,
        )?;
    }

    // The page's HTTP-backed program store: real example programs (and the kernel's async
    // sleep canary) pre-AOT'd to pulley32 and served as static files the blob fetches on
    // demand (www/web-eo9/blob/src/store.rs).
    let store_dir = root.join("www").join("site").join("vm").join("store");
    std::fs::create_dir_all(&store_dir)
        .map_err(|err| format!("failed to create {}: {err}", store_dir.display()))?;
    for example in ["hello", "cruncher", "outcomes", "readwrite"] {
        let component_path = root
            .join("guest")
            .join("target")
            .join("components")
            .join(format!("eo9-example-{example}.wasm"));
        let component = std::fs::read(&component_path)
            .map_err(|err| format!("failed to read {}: {err}", component_path.display()))?;
        preaot_for_web(
            &store_dir,
            &component,
            &format!("example {example}"),
            &format!("{example}.cwasm"),
            false,
        )?;
    }
    // The coreutils (guest/coreutils/*): real Eo9 guest programs the /vm page runs against
    // the blob's in-memory eo9:fs. AOT'd to pulley32 and served by name like the examples.
    for tool in [
        "cat", "ls", "echo", "rng", "wc", "head", "cp", "mkdir", "rm", "touch", "stat", "find",
    ] {
        let component_path = root
            .join("guest")
            .join("target")
            .join("components")
            .join(format!("eo9-coreutil-{tool}.wasm"));
        let component = std::fs::read(&component_path)
            .map_err(|err| format!("failed to read {}: {err}", component_path.display()))?;
        preaot_for_web(
            &store_dir,
            &component,
            &format!("coreutil {tool}"),
            &format!("{tool}.cwasm"),
            false,
        )?;
    }
    let sleepy_wat = root.join("kernel").join("seed").join("sleepy.wat");
    let sleepy_wasm = wat::parse_file(&sleepy_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", sleepy_wat.display()))?;
    preaot_for_web(
        &store_dir,
        &sleepy_wasm,
        "sleepy (async sleep canary)",
        "sleepy.cwasm",
        false,
    )?;

    // Build the blob in its own workspace for wasm32-unknown-unknown.
    //
    // The build is made path-independent (the same sources produce the same blob bytes from
    // any checkout directory) by remapping the absolute prefixes that otherwise leak into
    // panic-location strings: the repository root, the cargo home (registry sources), and
    // the rustup home (the toolchain's libcore/libstd paths). Without this the blob's
    // content hash — and therefore its fingerprinted URL — changed per checkout path.
    let manifest = root.join("www").join("web-eo9").join("Cargo.toml");
    let remap_flags = blob_remap_rustflags(root);
    run_with_env(
        root,
        "cargo",
        [
            OsStr::new("build"),
            OsStr::new("--release"),
            OsStr::new("--target"),
            OsStr::new("wasm32-unknown-unknown"),
            OsStr::new("--manifest-path"),
            manifest.as_os_str(),
            OsStr::new("-p"),
            OsStr::new("web-eo9-blob"),
        ],
        &[("RUSTFLAGS", remap_flags.as_os_str())],
    )?;
    // Keep the blob workspace lint-clean: it is deliberately outside the `ci` gate (wasm32,
    // heavy vendored closure), so its clippy/fmt run here, where the blob is built anyway.
    run(
        &root.join("www").join("web-eo9"),
        "cargo",
        ["fmt", "--all", "--check"],
    )?;
    run_with_env(
        &root.join("www").join("web-eo9"),
        "cargo",
        [
            "clippy",
            "--workspace",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--",
            "-D",
            "warnings",
        ],
        &[("RUSTFLAGS", remap_flags.as_os_str())],
    )?;

    let built = root
        .join("www")
        .join("web-eo9")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("web_eo9_blob.wasm");
    let site_dir = root.join("www").join("site").join("vm");
    std::fs::create_dir_all(&site_dir)
        .map_err(|err| format!("failed to create {}: {err}", site_dir.display()))?;
    let installed = site_dir.join("web-eo9.wasm");
    std::fs::copy(&built, &installed).map_err(|err| {
        format!(
            "failed to copy {} -> {}: {err}",
            built.display(),
            installed.display()
        )
    })?;
    let size = std::fs::metadata(&installed).map(|m| m.len()).unwrap_or(0);
    println!(
        "xtask: installed the web VM blob at {} ({size} bytes)",
        installed.display()
    );
    // Content-fingerprint the immutable assets (rename to carry a content hash, write the
    // manifest the page loads) before compressing, so the committed .br/.gz siblings are of
    // the fingerprinted files and a rebuild that changes the OS yields new, cache-busting URLs.
    fingerprint_web_vm(root)?;
    // Regenerated blob/store artifacts need fresh pre-compressed siblings or the server
    // falls back to serving them uncompressed.
    precompress_site(root)
}

/// `RUSTFLAGS` for the wasm32 blob build: remap the absolute path prefixes that would
/// otherwise end up in panic-location strings, so the blob's bytes (and its fingerprinted
/// URL) do not depend on where the repository happens to be checked out, the cargo home, or
/// the rustup home. Any RUSTFLAGS already present in the environment are preserved.
fn blob_remap_rustflags(root: &Path) -> OsString {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".cargo"));
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".rustup"));
    let mut flags = std::env::var_os("RUSTFLAGS").unwrap_or_default();
    for (prefix, replacement) in [
        (root.to_path_buf(), "/eo9"),
        (cargo_home, "/cargo-home"),
        (rustup_home, "/rustup-home"),
    ] {
        if !flags.is_empty() {
            flags.push(" ");
        }
        flags.push("--remap-path-prefix=");
        flags.push(prefix.as_os_str());
        flags.push("=");
        flags.push(replacement);
    }
    flags
}

/// The `/vm` immutable assets that get content-fingerprinted: the wasm blob and every Pulley
/// `.cwasm` store image. Their URLs become the version, so they can be cached forever.
fn web_vm_fingerprint_plan(site_dir: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    // (canonical file, logical key for the manifest)
    let mut plan = vec![(site_dir.join("web-eo9.wasm"), "blob".to_owned())];
    let store_dir = site_dir.join("store");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&store_dir)
        .map_err(|err| format!("failed to read {}: {err}", store_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("cwasm"))
        // Skip already-fingerprinted leftovers; we rebuild the plan from canonical names.
        .filter(|p| !is_fingerprinted_name(p))
        .collect();
    entries.sort();
    for entry in entries {
        let name = entry
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("bad store artifact name: {}", entry.display()))?
            .to_owned();
        plan.push((entry, format!("store/{name}")));
    }
    Ok(plan)
}

/// Whether a path is a content-fingerprinted immutable asset (`name.<16-hex>.wasm`/`.cwasm`).
/// Mirrors `eo9_www::is_fingerprinted`; duplicated here so xtask stays dependency-light (it
/// must not pull in the web-server crate). Keep the two in sync.
fn is_fingerprinted_name(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !matches!(ext.as_str(), "wasm" | "cwasm") {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    match stem.rsplit_once('.') {
        Some((base, hash)) => {
            !base.is_empty()
                && hash.len() == 16
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

/// A 16-hex-char content fingerprint (64-bit FNV-1a, the same convention the server's ETag
/// uses), short enough for a tidy URL and ample for cache-busting a handful of assets.
fn content_fingerprint(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash ^= bytes.len() as u64;
    hash = hash.wrapping_mul(PRIME);
    format!("{hash:016x}")
}

/// Insert the fingerprint into a canonical filename: `web-eo9.wasm` -> `web-eo9.<hash>.wasm`.
fn fingerprinted_name(canonical: &Path, hash: &str) -> String {
    let stem = canonical
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    format!("{stem}.{hash}.{ext}")
}

/// Delete a file and its `.br`/`.gz` precompressed siblings, ignoring absence.
fn remove_with_siblings(path: &Path) {
    for suffix in ["", ".br", ".gz"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

/// Content-fingerprint the `/vm` immutable assets and write `vm/assets.json`.
///
/// Each canonical asset (`web-eo9.wasm`, `store/*.cwasm`) is hashed once, renamed to embed the
/// hash, and recorded in the manifest the page fetches to resolve URLs. Old fingerprinted
/// variants (and stale `.br`/`.gz` siblings) are removed so a rebuild leaves exactly the
/// current set. Runs inside `build-web-vm`; precompression happens afterward.
fn fingerprint_web_vm(root: &Path) -> Result<(), String> {
    let site_dir = root.join("www").join("site").join("vm");
    // Clear any previously-fingerprinted assets so an OS change doesn't leave old-hash files.
    for dir in [site_dir.clone(), site_dir.join("store")] {
        if let Ok(read) = std::fs::read_dir(&dir) {
            for path in read.filter_map(Result::ok).map(|e| e.path()) {
                let stale_sibling = strip_precompressed_suffix(&path)
                    .is_some_and(|base| is_fingerprinted_name(&base));
                if is_fingerprinted_name(&path) || stale_sibling {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    let plan = web_vm_fingerprint_plan(&site_dir)?;
    let mut manifest_entries: Vec<(String, String)> = Vec::new();
    for (canonical, key) in plan {
        let bytes = std::fs::read(&canonical)
            .map_err(|err| format!("failed to read {}: {err}", canonical.display()))?;
        let hash = content_fingerprint(&bytes);
        let new_name = fingerprinted_name(&canonical, &hash);
        let new_path = canonical.with_file_name(&new_name);
        std::fs::rename(&canonical, &new_path)
            .map_err(|err| format!("failed to rename {}: {err}", canonical.display()))?;
        // Drop the canonical file's stale precompressed siblings; precompress regenerates
        // them for the fingerprinted name.
        remove_with_siblings(&canonical);
        // URL the page fetches: relative to the site root, always `/vm/...`.
        let rel = new_path
            .strip_prefix(root.join("www").join("site"))
            .map_err(|_| "fingerprinted asset escaped the site root".to_owned())?;
        let url = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
        manifest_entries.push((key, url));
    }

    write_assets_manifest(&site_dir, &manifest_entries)?;
    println!(
        "xtask: fingerprinted {} /vm asset(s) and wrote vm/assets.json",
        manifest_entries.len()
    );
    Ok(())
}

/// If `path` ends in `.br`/`.gz`, the path with that suffix removed; else `None`.
fn strip_precompressed_suffix(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    for suffix in [".br", ".gz"] {
        if let Some(base) = name.strip_suffix(suffix) {
            return Some(path.with_file_name(base));
        }
    }
    None
}

/// Write `vm/assets.json`: a nested map `{ "blob": "/vm/...", "store": { "hello": "/vm/store/..." } }`.
/// Hand-rolled JSON (xtask stays dependency-light); the values are build-controlled URLs.
fn write_assets_manifest(site_dir: &Path, entries: &[(String, String)]) -> Result<(), String> {
    let mut blob = String::new();
    let mut store: Vec<(String, String)> = Vec::new();
    for (key, url) in entries {
        match key.strip_prefix("store/") {
            Some(name) => store.push((name.to_owned(), url.clone())),
            None if key == "blob" => blob = url.clone(),
            None => store.push((key.clone(), url.clone())),
        }
    }
    let mut json = String::from("{\n");
    json.push_str(&format!("  \"blob\": {},\n", json_string(&blob)));
    json.push_str("  \"store\": {\n");
    for (i, (name, url)) in store.iter().enumerate() {
        let comma = if i + 1 < store.len() { "," } else { "" };
        json.push_str(&format!(
            "    {}: {}{comma}\n",
            json_string(name),
            json_string(url)
        ));
    }
    json.push_str("  }\n}\n");
    let path = site_dir.join("assets.json");
    std::fs::write(&path, json).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Minimal JSON string escaping for the manifest values (build-controlled names/URLs).
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Drift guard: verify `vm/assets.json` points at committed files whose names still encode
/// their current content hash (so a stale manifest or a hand-edited asset is caught). Does
/// not rebuild the blob, so it is cheap enough to run anywhere.
fn check_web_vm(root: &Path) -> Result<(), String> {
    let site_dir = root.join("www").join("site").join("vm");
    let manifest_path = site_dir.join("assets.json");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|err| format!("failed to read {}: {err}", manifest_path.display()))?;
    // Pull every "/vm/..." URL out of the manifest (values are the only such strings).
    let urls: Vec<String> = manifest
        .split('"')
        .filter(|s| s.starts_with("/vm/"))
        .map(str::to_owned)
        .collect();
    if urls.is_empty() {
        return Err(format!("{} lists no /vm assets", manifest_path.display()));
    }
    let site_root = root.join("www").join("site");
    let mut checked = 0usize;
    for url in urls {
        let rel = url.trim_start_matches('/');
        let path = site_root.join(rel);
        if !path.exists() {
            return Err(format!("assets.json points at {url}, which does not exist"));
        }
        if !is_fingerprinted_name(&path) {
            return Err(format!("assets.json points at non-fingerprinted {url}"));
        }
        let bytes = std::fs::read(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let expected = content_fingerprint(&bytes);
        let actual = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit_once('.').map(|(_, h)| h.to_owned()))
            .unwrap_or_default();
        if expected != actual {
            return Err(format!(
                "{url} is stale: name encodes {actual} but its content hashes to {expected} \
                 (re-run `cargo xtask fingerprint-web-vm` / `build-web-vm`)"
            ));
        }
        checked += 1;
    }
    println!("xtask: check-web-vm ok — {checked} fingerprinted /vm asset(s) match assets.json");
    Ok(())
}

/// Pre-AOT one component to a `pulley32` artifact for the web VM blob. The configuration
/// mirrors `precompile_for_kernel` apart from the target (and must stay in sync with the
/// blob's `base_config`).
fn preaot_for_web(
    out_dir: &Path,
    component: &[u8],
    what: &str,
    file_name: &str,
    consume_fuel: bool,
) -> Result<(), String> {
    let mut config = wasmtime::Config::new();
    config
        .target("pulley32")
        .map_err(|err| format!("wasmtime rejected target pulley32: {err:#}"))?;
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(true);
    config.gc_support(false);
    config.wasm_threads(false);
    config.consume_fuel(consume_fuel);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|err| format!("failed to build the pulley32 pre-AOT engine: {err:#}"))?;
    let artifact = engine
        .precompile_component(component)
        .map_err(|err| format!("failed to precompile {what} for pulley32: {err:#}"))?;
    let out_path = out_dir.join(file_name);
    std::fs::write(&out_path, &artifact)
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;
    println!(
        "xtask: precompiled {what} -> {} ({} bytes, target pulley32, consume_fuel = {consume_fuel})",
        out_path.display(),
        artifact.len()
    );
    Ok(())
}

/// Amount of RAM given to the QEMU guest. Must stay in sync with `RAM_SIZE` in
/// `kernel/eo9-kernel/src/heap.rs`, which hands everything above the image to the heap.
const KERNEL_QEMU_MEMORY: &str = "512M";

/// Assemble the kernel's read-only store image (kernel/eo9-kernel/src/wasm/store.rs
/// documents the format): each listed guest component is built, componentized, and
/// host-AOT precompiled for the bare-metal target, then packed as
/// `name + component bytes + artifact bytes + metadata text`.
fn build_store_image(root: &Path, target: &str) -> Result<PathBuf, String> {
    let mut image: Vec<u8> = Vec::new();
    image.extend_from_slice(b"EO9STOR2");
    image.extend_from_slice(
        &u32::try_from(KERNEL_STORE_COMPONENTS.len())
            .unwrap()
            .to_le_bytes(),
    );
    for (package, shell_name) in KERNEL_STORE_COMPONENTS {
        let component_path = build_guest_component(root, package)?;
        let component = std::fs::read(&component_path)
            .map_err(|err| format!("failed to read {}: {err}", component_path.display()))?;
        let artifact_path = precompile_for_kernel(
            root,
            &component,
            package,
            &format!("store-{shell_name}.cwasm"),
            target,
        )?;
        let artifact = std::fs::read(&artifact_path)
            .map_err(|err| format!("failed to read {}: {err}", artifact_path.display()))?;
        let metadata = component_metadata(shell_name, &component)?;

        let name = shell_name.as_bytes();
        image.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        image.extend_from_slice(name);
        image.extend_from_slice(&u32::try_from(component.len()).unwrap().to_le_bytes());
        image.extend_from_slice(&component);
        image.extend_from_slice(&u32::try_from(artifact.len()).unwrap().to_le_bytes());
        image.extend_from_slice(&artifact);
        let metadata = metadata.as_bytes();
        image.extend_from_slice(&u32::try_from(metadata.len()).unwrap().to_le_bytes());
        image.extend_from_slice(metadata);
    }

    let out_dir = kernel_precompiled_dir(root, target);
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join("store.img");
    std::fs::write(&out_path, &image)
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;
    println!(
        "xtask: assembled store image {} ({} bytes, {} components, target {target})",
        out_path.display(),
        image.len(),
        KERNEL_STORE_COMPONENTS.len()
    );
    Ok(out_path)
}

/// Where host-AOT artifacts for a bare-metal target are written. aarch64 keeps the original
/// flat `kernel/target/precompiled/` layout (so its artifacts and the env-var paths the
/// kernel build embeds stay byte-for-byte identical to before the riscv64 port); every
/// other target gets a per-target subdirectory.
fn kernel_precompiled_dir(root: &Path, target: &str) -> PathBuf {
    let base = root.join("kernel").join("target").join("precompiled");
    if target == KERNEL_CHECK_TARGET {
        base
    } else {
        base.join(target)
    }
}

/// Describe one store component as the plain-text metadata block the kernel embeds next to
/// it (kernel/eo9-kernel/src/wasm/store.rs documents the line format). The kernel cannot
/// parse component binaries itself yet (no on-target codegen or wasm-tools), so `describe`
/// runs here, at image-assembly time, through the same `eo9-component` crate the usermode
/// runtime uses — the kernel's `describe` then simply replays this.
fn component_metadata(shell_name: &str, component: &[u8]) -> Result<String, String> {
    let component = eo9_component::Component::load(component.to_vec()).map_err(|err| {
        format!("store component `{shell_name}` does not load as an eo9 module: {err:?}")
    })?;
    let info = component.describe();
    // Space-separated records; an empty field is spelled `-` so the kernel-side parser
    // never has to disambiguate consecutive separators.
    let field = |text: &str| {
        if text.is_empty() {
            "-".to_string()
        } else {
            text.to_string()
        }
    };
    let mut meta = String::new();
    meta.push_str(match info.kind {
        eo9_component::ComponentKind::Binary => "kind binary\n",
        eo9_component::ComponentKind::Provider => "kind provider\n",
    });
    for need in &info.imports {
        meta.push_str(&format!(
            "import {} {} {} {}\n",
            if need.required {
                "required"
            } else {
                "optional"
            },
            field(&need.slot),
            field(&need.interface),
            field(&need.version),
        ));
    }
    for slot in &info.exports {
        meta.push_str(&format!(
            "export {} {} {}\n",
            field(&slot.name),
            field(&slot.interface),
            field(&slot.version)
        ));
    }
    for arg in &info.args {
        meta.push_str(&format!("arg {} {}\n", field(&arg.name), arg.ty));
    }
    Ok(meta)
}

/// Build the bootable kernel image for `arch` and return its path.
///
/// For aarch64 this precompiles the wasm artifacts the kernel embeds — the hand-written
/// seed component (kernel/seed/hello.wat) and the real `eo9-example-hello` program from
/// the guest workspace — for the bare-metal target with the host wasmtime, then builds
/// `eo9-kernel` in release mode with the `wasm-seed` and `wasm-hello` features so both are
/// embedded in the image. The result is an ELF that QEMU's `-kernel` loader boots directly.
fn build_kernel(root: &Path, arch: &str) -> Result<PathBuf, String> {
    match arch {
        "aarch64" => build_kernel_aarch64(root),
        "riscv64" => build_kernel_riscv64(root),
        "x86_64" => build_kernel_x86_64(root),
        _ => Err(format!(
            "`build-kernel {arch}` is not implemented yet: the bare-metal kernel covers aarch64, \
             riscv64 and x86_64 so far (plan/12-kernel.md)"
        )),
    }
}

/// x86_64 (QEMU `q35`, PVH direct boot): the same host-AOT precompile pipeline as the other
/// ports — the seed canary, the real hello program, the async pair, and the read-only store
/// image — targeted at `x86_64-unknown-none`, then a kernel build with the wasm feature set
/// with the full feature set (`wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen`):
/// since milestone 5 (4 KiB W^X tables + the on-target compiler) the x86_64 shell composes
/// and compiles `$`/`&` on the machine itself, exactly like the other two ports.
///
/// Emitting x86_64 machine code needs that Cranelift backend in the host build; on an
/// x86_64 host it is the host backend, but on this project's aarch64 development machines it
/// is a non-host backend, so — exactly like riscv64 — the off-by-default `kernel-cross-aot`
/// xtask feature (`wasmtime/all-arch`) provides it and this function re-runs itself with the
/// feature when it is absent.
fn build_kernel_x86_64(root: &Path) -> Result<PathBuf, String> {
    let kernel_dir = root.join("kernel");
    let image = kernel_dir
        .join("target")
        .join(KERNEL_X86_64_TARGET)
        .join("release")
        .join("eo9-kernel");

    if !cfg!(feature = "kernel-cross-aot") && !cfg!(target_arch = "x86_64") {
        println!(
            "xtask: re-running with --features kernel-cross-aot (this xtask build does not \
             link the x86_64 Cranelift backend)"
        );
        run(
            root,
            "cargo",
            [
                "run",
                "-p",
                "xtask",
                "--features",
                "kernel-cross-aot",
                "--",
                "build-kernel",
                "x86_64",
            ],
        )?;
        if !image.is_file() {
            return Err(format!(
                "the kernel-cross-aot build succeeded but {} is missing",
                image.display()
            ));
        }
        return Ok(image);
    }

    // The seed canary, assembled from WAT.
    let seed_wat = root.join("kernel").join("seed").join("hello.wat");
    let seed_wasm = wat::parse_file(&seed_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", seed_wat.display()))?;
    let seed = precompile_for_kernel(
        root,
        &seed_wasm,
        "seed component",
        "seed.cwasm",
        KERNEL_X86_64_TARGET,
    )?;

    // The async canary (awaits time.sleep against the kernel timer), assembled from WAT.
    let sleepy_wat = root.join("kernel").join("seed").join("sleepy.wat");
    let sleepy_wasm = wat::parse_file(&sleepy_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", sleepy_wat.display()))?;
    let sleepy = precompile_for_kernel(
        root,
        &sleepy_wasm,
        "sleepy canary",
        "sleepy.cwasm",
        KERNEL_X86_64_TARGET,
    )?;

    // The real hello program, built from the guest workspace.
    let hello_component = build_guest_component(root, "eo9-example-hello")?;
    let hello_wasm = std::fs::read(&hello_component)
        .map_err(|err| format!("failed to read {}: {err}", hello_component.display()))?;
    let hello = precompile_for_kernel(
        root,
        &hello_wasm,
        "eo9-example-hello",
        "hello.cwasm",
        KERNEL_X86_64_TARGET,
    )?;

    // The unmodified entropy.seeded stub (async-ABI configure), exactly as on aarch64.
    let entropy_component = build_guest_component(root, "eo9-stub-entropy-seeded")?;
    let entropy_wasm = std::fs::read(&entropy_component)
        .map_err(|err| format!("failed to read {}: {err}", entropy_component.display()))?;
    let entropy = precompile_for_kernel(
        root,
        &entropy_wasm,
        "eo9-stub-entropy-seeded",
        "entropy-seeded.cwasm",
        KERNEL_X86_64_TARGET,
    )?;

    // The read-only store image (the same component list as aarch64), AOT'd for x86_64.
    let store_image = build_store_image(root, KERNEL_X86_64_TARGET)?;

    // The same seed component as *raw* (un-precompiled) wasm bytes, for the on-target
    // codegen demo: the kernel compiles this with its own Cranelift (wasm-codegen) rather
    // than deserializing a host-produced artifact.
    let seed_wasm_path = kernel_precompiled_dir(root, KERNEL_X86_64_TARGET).join("seed.wasm");
    std::fs::create_dir_all(seed_wasm_path.parent().unwrap())
        .map_err(|err| format!("failed to create precompiled dir: {err}"))?;
    std::fs::write(&seed_wasm_path, &seed_wasm)
        .map_err(|err| format!("failed to write {}: {err}", seed_wasm_path.display()))?;

    run_with_env(
        &kernel_dir,
        "cargo",
        [
            "build",
            "-p",
            "eo9-kernel",
            "--release",
            "--target",
            KERNEL_X86_64_TARGET,
            "--features",
            "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen",
        ],
        &[
            ("EO9_SEED_CWASM", seed.as_os_str()),
            ("EO9_SEED_WASM", seed_wasm_path.as_os_str()),
            ("EO9_HELLO_CWASM", hello.as_os_str()),
            ("EO9_SLEEPY_CWASM", sleepy.as_os_str()),
            ("EO9_ENTROPY_SEEDED_CWASM", entropy.as_os_str()),
            ("EO9_STORE_IMAGE", store_image.as_os_str()),
        ],
    )?;

    if !image.is_file() {
        return Err(format!(
            "kernel build succeeded but {} is missing",
            image.display()
        ));
    }
    println!("xtask: built kernel image {}", image.display());
    Ok(image)
}

/// riscv64 (QEMU `virt`, S-mode under OpenSBI): the same host-AOT precompile pipeline as
/// aarch64 — the seed canary, the real hello program, the async pair, and the read-only
/// store image — targeted at riscv64, then a kernel build with the full feature set
/// (`wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen`). With milestone 5 (Sv39 +
/// W^X + on-target codegen) the riscv64 shell composes and compiles `$`/`&` on the machine
/// itself, exactly like aarch64; cranelift's riscv64 backend is selected automatically by
/// the `host-arch` feature when the kernel is compiled for this target.
///
/// Emitting riscv64 machine code from the host needs the non-host Cranelift backends,
/// which only the off-by-default `kernel-cross-aot` xtask feature links (so every other
/// xtask invocation stays lean). When the feature is absent this function re-runs
/// `cargo run -p xtask --features kernel-cross-aot -- build-kernel riscv64` and returns
/// the image that build produces.
fn build_kernel_riscv64(root: &Path) -> Result<PathBuf, String> {
    let kernel_dir = root.join("kernel");
    let image = kernel_dir
        .join("target")
        .join(KERNEL_RISCV64_TARGET)
        .join("release")
        .join("eo9-kernel");

    if !cfg!(feature = "kernel-cross-aot") {
        println!(
            "xtask: re-running with --features kernel-cross-aot (this xtask build does not \
             link the riscv64 Cranelift backend)"
        );
        run(
            root,
            "cargo",
            [
                "run",
                "-p",
                "xtask",
                "--features",
                "kernel-cross-aot",
                "--",
                "build-kernel",
                "riscv64",
            ],
        )?;
        if !image.is_file() {
            return Err(format!(
                "the kernel-cross-aot build succeeded but {} is missing",
                image.display()
            ));
        }
        return Ok(image);
    }

    // The seed canary, assembled from WAT.
    let seed_wat = root.join("kernel").join("seed").join("hello.wat");
    let seed_wasm = wat::parse_file(&seed_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", seed_wat.display()))?;
    let seed = precompile_for_kernel(
        root,
        &seed_wasm,
        "seed component",
        "seed.cwasm",
        KERNEL_RISCV64_TARGET,
    )?;

    // The async canary (awaits time.sleep against the kernel timer), assembled from WAT.
    let sleepy_wat = root.join("kernel").join("seed").join("sleepy.wat");
    let sleepy_wasm = wat::parse_file(&sleepy_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", sleepy_wat.display()))?;
    let sleepy = precompile_for_kernel(
        root,
        &sleepy_wasm,
        "sleepy canary",
        "sleepy.cwasm",
        KERNEL_RISCV64_TARGET,
    )?;

    // The real hello program, built from the guest workspace.
    let hello_component = build_guest_component(root, "eo9-example-hello")?;
    let hello_wasm = std::fs::read(&hello_component)
        .map_err(|err| format!("failed to read {}: {err}", hello_component.display()))?;
    let hello = precompile_for_kernel(
        root,
        &hello_wasm,
        "eo9-example-hello",
        "hello.cwasm",
        KERNEL_RISCV64_TARGET,
    )?;

    // The unmodified entropy.seeded stub (async-ABI configure), exactly as on aarch64.
    let entropy_component = build_guest_component(root, "eo9-stub-entropy-seeded")?;
    let entropy_wasm = std::fs::read(&entropy_component)
        .map_err(|err| format!("failed to read {}: {err}", entropy_component.display()))?;
    let entropy = precompile_for_kernel(
        root,
        &entropy_wasm,
        "eo9-stub-entropy-seeded",
        "entropy-seeded.cwasm",
        KERNEL_RISCV64_TARGET,
    )?;

    // The read-only store image (the same component list as aarch64), AOT'd for riscv64.
    let store_image = build_store_image(root, KERNEL_RISCV64_TARGET)?;

    // The same seed component as *raw* (un-precompiled) wasm bytes, for the on-target
    // codegen demo: the kernel compiles this with its own Cranelift (wasm-codegen) rather
    // than deserializing a host-produced artifact.
    let seed_wasm_path = kernel_precompiled_dir(root, KERNEL_RISCV64_TARGET).join("seed.wasm");
    std::fs::create_dir_all(seed_wasm_path.parent().unwrap())
        .map_err(|err| format!("failed to create precompiled dir: {err}"))?;
    std::fs::write(&seed_wasm_path, &seed_wasm)
        .map_err(|err| format!("failed to write {}: {err}", seed_wasm_path.display()))?;

    run_with_env(
        &kernel_dir,
        "cargo",
        [
            "build",
            "-p",
            "eo9-kernel",
            "--release",
            "--target",
            KERNEL_RISCV64_TARGET,
            "--features",
            "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen",
        ],
        &[
            ("EO9_SEED_CWASM", seed.as_os_str()),
            ("EO9_SEED_WASM", seed_wasm_path.as_os_str()),
            ("EO9_HELLO_CWASM", hello.as_os_str()),
            ("EO9_SLEEPY_CWASM", sleepy.as_os_str()),
            ("EO9_ENTROPY_SEEDED_CWASM", entropy.as_os_str()),
            ("EO9_STORE_IMAGE", store_image.as_os_str()),
        ],
    )?;

    if !image.is_file() {
        return Err(format!(
            "kernel build succeeded but {} is missing",
            image.display()
        ));
    }
    println!("xtask: built kernel image {}", image.display());
    Ok(image)
}

fn build_kernel_aarch64(root: &Path) -> Result<PathBuf, String> {
    // The seed canary, assembled from WAT.
    let seed_wat = root.join("kernel").join("seed").join("hello.wat");
    let seed_wasm = wat::parse_file(&seed_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", seed_wat.display()))?;
    let seed = precompile_for_kernel(
        root,
        &seed_wasm,
        "seed component",
        "seed.cwasm",
        KERNEL_CHECK_TARGET,
    )?;

    // The same seed component as *raw* (un-precompiled) wasm bytes, for the on-target
    // codegen demo: the kernel compiles this with its own Cranelift (wasm-codegen) rather
    // than deserializing a host-produced artifact.
    let seed_wasm_path = root
        .join("kernel")
        .join("target")
        .join("precompiled")
        .join("seed.wasm");
    std::fs::create_dir_all(seed_wasm_path.parent().unwrap())
        .map_err(|err| format!("failed to create precompiled dir: {err}"))?;
    std::fs::write(&seed_wasm_path, &seed_wasm)
        .map_err(|err| format!("failed to write {}: {err}", seed_wasm_path.display()))?;

    // The async canary (awaits time.sleep against the kernel timer), assembled from WAT.
    let sleepy_wat = root.join("kernel").join("seed").join("sleepy.wat");
    let sleepy_wasm = wat::parse_file(&sleepy_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", sleepy_wat.display()))?;
    let sleepy = precompile_for_kernel(
        root,
        &sleepy_wasm,
        "sleepy canary",
        "sleepy.cwasm",
        KERNEL_CHECK_TARGET,
    )?;

    // The real hello program, built from the guest workspace.
    let hello_component = build_guest_component(root, "eo9-example-hello")?;
    let hello_wasm = std::fs::read(&hello_component)
        .map_err(|err| format!("failed to read {}: {err}", hello_component.display()))?;
    let hello = precompile_for_kernel(
        root,
        &hello_wasm,
        "eo9-example-hello",
        "hello.cwasm",
        KERNEL_CHECK_TARGET,
    )?;

    // The unmodified entropy.seeded stub from the guest workspace: a real SDK-built
    // component whose `configure` export uses the async canonical ABI.
    let entropy_component = build_guest_component(root, "eo9-stub-entropy-seeded")?;
    let entropy_wasm = std::fs::read(&entropy_component)
        .map_err(|err| format!("failed to read {}: {err}", entropy_component.display()))?;
    let entropy = precompile_for_kernel(
        root,
        &entropy_wasm,
        "eo9-stub-entropy-seeded",
        "entropy-seeded.cwasm",
        KERNEL_CHECK_TARGET,
    )?;

    // The read-only store image: every listed component plus its host-AOT artifact,
    // keyed by shell name, for the kernel's `program=<name>` selection (and, later,
    // eosh's /bin view).
    let store_image = build_store_image(root, KERNEL_CHECK_TARGET)?;

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
            "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen",
        ],
        &[
            ("EO9_SEED_CWASM", seed.as_os_str()),
            ("EO9_SEED_WASM", seed_wasm_path.as_os_str()),
            ("EO9_HELLO_CWASM", hello.as_os_str()),
            ("EO9_SLEEPY_CWASM", sleepy.as_os_str()),
            ("EO9_ENTROPY_SEEDED_CWASM", entropy.as_os_str()),
            ("EO9_STORE_IMAGE", store_image.as_os_str()),
        ],
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

/// Precompile a component for a bare-metal target, writing it under
/// [`kernel_precompiled_dir`].
///
/// The artifact must be loadable by the kernel's `no_std` wasmtime engine, so the
/// compilation config mirrors what that engine computes for itself on an OS-less target:
/// no signals-based traps, no virtual-memory reservations or guards, no copy-on-write
/// memory initialization, and no wasm proposals beyond what the kernel build enables
/// (feature unification gives this host build GC, threads, and component-model-async
/// support via eo9-runtime's wasmtime features; the kernel build has none of those).
/// The target string must match the kernel-side `NATIVE_TARGET` for that architecture
/// (kernel/eo9-kernel/src/wasm/mod.rs) so deserialization accepts the artifact. Non-host
/// targets (riscv64) additionally need the `kernel-cross-aot` xtask feature, which links
/// every Cranelift backend.
fn precompile_for_kernel(
    root: &Path,
    component: &[u8],
    what: &str,
    file_name: &str,
    target: &str,
) -> Result<PathBuf, String> {
    let mut config = wasmtime::Config::new();
    config
        .target(target)
        .map_err(|err| format!("wasmtime rejected target {target}: {err:#}"))?;
    if target == KERNEL_X86_64_TARGET {
        // The x86_64 kernel is compiled soft-float (`x86_64-unknown-none`), so no float value
        // may ever cross the generated-code/host boundary in a register. The only such
        // crossing wasmtime has is float "libcalls" (f32/f64 ceil/floor/trunc/nearest when
        // the compilation target lacks SSE4.1), so enable SSE3..SSE4.2 here — then those
        // instructions are emitted inline and no float libcall exists in any artifact. This
        // is `Config::x86_float_abi_ok`'s documented safe condition (b); the kernel-side
        // engine asserts the same thing (kernel/eo9-kernel/src/wasm/mod.rs) and probes the
        // CPU for these features at load time, and xtask's QEMU invocation uses `-cpu max`
        // so they are present under TCG.
        //
        // SAFETY: enabling ISA flags only changes which instructions may be emitted; the
        // kernel engine refuses to load the artifact unless the CPU actually has them.
        unsafe {
            config.cranelift_flag_enable("has_sse3");
            config.cranelift_flag_enable("has_ssse3");
            config.cranelift_flag_enable("has_sse41");
            config.cranelift_flag_enable("has_sse42");
        }
    }
    config.wasm_component_model(true);
    // The component-model async ABI (plus stackful lifts and the extra async built-ins
    // the eo9 guest SDK uses). Compile-relevant: the kernel engine enables exactly the
    // same wasm features (kernel/eo9-kernel/src/wasm/mod.rs).
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(true);
    // Fuel metering is compile-relevant (the generated code carries the fuel decrements).
    // The kernel engine meters fuel so spawned children are preemptible at quantum
    // granularity (plan/12: child fuel / preemption); the precompiled artifacts must match.
    config.consume_fuel(true);
    config.gc_support(false);
    config.wasm_threads(false);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|err| format!("failed to build the kernel-precompile engine: {err:#}"))?;
    let artifact = engine
        .precompile_component(component)
        .map_err(|err| format!("failed to precompile {what}: {err:#}"))?;

    let out_dir = kernel_precompiled_dir(root, target);
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join(file_name);
    std::fs::write(&out_path, &artifact)
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;
    println!(
        "xtask: precompiled {what} -> {} ({} bytes, target {target})",
        out_path.display(),
        artifact.len()
    );
    Ok(out_path)
}

/// Path of the scratch raw disk image the `disk` QEMU flag attaches as a virtio-blk
/// function, creating it (blank, 64 MiB) on first use. Blank is all the demo needs:
/// `fs.eofs` formats a blank device in place on first mount, and writes persist in this
/// file across QEMU runs.
fn ensure_scratch_disk(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("kernel").join("target");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let path = dir.join("eo9-scratch-disk.raw");
    if !path.exists() {
        let file = std::fs::File::create(&path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        file.set_len(64 * 1024 * 1024)
            .map_err(|err| format!("failed to size {}: {err}", path.display()))?;
        println!(
            "xtask: created blank 64 MiB scratch disk at {}",
            path.display()
        );
    }
    Ok(path)
}

/// Build the kernel image for `arch` and boot it under QEMU with serial on stdio.
///
/// The exact invocation (aarch64): `qemu-system-aarch64 -M virt,gic-version=2,highmem=off
/// -cpu max -smp 1 -m 512M -nographic -device virtio-rng-pci -kernel <image>`. The kernel
/// powers the machine off via PSCI when its run completes (or on panic), so QEMU exits by
/// itself; to quit earlier press Ctrl-A then X.
///
/// A bare `disk` argument is consumed by xtask itself (it never reaches the kernel command
/// line): it attaches the scratch raw image as a modern virtio-blk PCI function
/// (`-device virtio-blk-pci,disable-legacy=on`) so the `disk.virtio` driver has real
/// hardware to claim — `cargo xtask qemu aarch64 pci disk`. A bare `net` argument is the
/// same idea for networking: it attaches a modern virtio-net PCI function backed by QEMU
/// user-mode networking (`-netdev user`) so the `net.virtio` driver has a NIC to claim —
/// `cargo xtask qemu aarch64 pci net`.
fn qemu(root: &Path, arch: &str, append: &[String]) -> Result<(), String> {
    let image = build_kernel(root, arch)?;
    let qemu = format!("qemu-system-{arch}");
    println!(
        "xtask: booting {} under {qemu} (serial on stdio; the kernel powers off when done, \
         or press Ctrl-A then X to quit)",
        image.display()
    );
    let machine: &[&str] = match arch {
        // Pin GICv2: the kernel brings up the GIC distributor + CPU interface over MMIO
        // (src/arch/aarch64/gic.rs) to forward the generic-timer interrupt so the executor
        // can wfi-idle. With `-cpu max` QEMU would otherwise default to GICv3 (a
        // system-register CPU interface with per-PE redistributors), which that minimal
        // MMIO bring-up does not drive.
        //
        // `highmem=off` keeps the PCIe ECAM at its low address (0x3f00_0000, inside the
        // kernel's identity-mapped device gigabyte — see kernel src/pci.rs); with the
        // default highmem layout QEMU moves the ECAM above 4 GiB where the kernel has no
        // mapping. RAM (512 MiB) is unaffected.
        //
        // The `virtio-rng-pci` device is a PCIe function with no host-side configuration,
        // so the eo9:pci capability has something real to enumerate next to the host
        // bridge (the `lspci` demo; the kernel never touches it otherwise).
        "aarch64" => &[
            "-M",
            "virt,gic-version=2,highmem=off",
            "-cpu",
            "max",
            "-device",
            "virtio-rng-pci",
        ],
        // Pin the SiFive-style PLIC (`aia=none`) for the same reason: the kernel's
        // interrupt bring-up (src/arch/riscv64/plic.rs) drives the PLIC, not the newer
        // AIA APLIC/IMSIC. The default CPU and QEMU's bundled OpenSBI `-bios` are used.
        "riscv64" => &["-M", "virt,aia=none"],
        // The image boots through QEMU's PVH direct-boot path (the ELF note in
        // src/arch/x86_64/boot.rs); SeaBIOS still POSTs first, which is why the firmware
        // banner appears before the kernel's. `-no-reboot` turns a triple fault into a QEMU
        // exit instead of a silent reboot loop, keeping scripted runs honest. `-cpu max`
        // (as on aarch64) gives the guest SSE3..SSE4.2 under TCG, which the precompiled
        // artifacts are built to assume so wasmtime never emits a float libcall against the
        // soft-float kernel (see `precompile_for_kernel`).
        "x86_64" => &["-M", "q35", "-no-reboot", "-cpu", "max"],
        other => {
            return Err(format!(
                "`qemu {other}` is not implemented yet (plan/12-kernel.md)"
            ));
        }
    };
    let mut args: Vec<std::ffi::OsString> = machine
        .iter()
        .copied()
        .chain([
            "-smp",
            "1",
            "-m",
            KERNEL_QEMU_MEMORY,
            "-nographic",
            "-kernel",
        ])
        .map(Into::into)
        .collect();
    args.push(image.as_os_str().to_os_string());
    // The bare `disk` and `net` arguments are xtask's: attach the scratch virtio-blk disk
    // / a user-mode virtio-net NIC and keep the tokens off the kernel command line.
    let mut cmdline: Vec<String> = Vec::new();
    let mut attach_disk = false;
    let mut attach_net = false;
    for argument in append {
        if argument == "disk" {
            attach_disk = true;
        } else if argument == "net" {
            attach_net = true;
        } else {
            cmdline.push(argument.clone());
        }
    }
    if attach_disk {
        let scratch = ensure_scratch_disk(root)?;
        args.push("-drive".into());
        args.push(format!("if=none,format=raw,id=eo9disk,file={}", scratch.display()).into());
        args.push("-device".into());
        args.push("virtio-blk-pci,drive=eo9disk,disable-legacy=on".into());
    }
    if attach_net {
        args.push("-netdev".into());
        args.push("user,id=eo9net".into());
        args.push("-device".into());
        args.push("virtio-net-pci,netdev=eo9net,disable-legacy=on".into());
    }
    // Anything else after the architecture becomes the kernel command line, e.g.
    // `cargo xtask qemu aarch64 program=cruncher seed=9 rounds=200000`.
    if !cmdline.is_empty() {
        args.push("-append".into());
        args.push(cmdline.join(" ").into());
    }
    run(root, &qemu, args)
}

fn fmt(root: &Path, check: bool) -> Result<(), String> {
    let mut args = vec!["fmt", "--all"];
    if check {
        args.push("--check");
    }
    for dir in workspaces(root) {
        run(&dir, "cargo", args.clone())?;
    }
    // The website server workspace is part of the gate too (plan/15): www-only branches used
    // to be able to land with fmt drift because nothing in `ci` touched that workspace.
    run(&root.join("www"), "cargo", args.clone())?;
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
    for target in KERNEL_CI_TARGETS {
        run(
            &root.join("kernel"),
            "cargo",
            [
                "clippy",
                "--workspace",
                "--target",
                target,
                "--",
                "-D",
                "warnings",
            ],
        )?;
    }
    // The website server workspace (www/): native build, quick tests, no wasm32 blob —
    // the in-browser blob workspace (www/web-eo9) is deliberately NOT in the gate; its
    // clippy/fmt run as part of `build-web-vm` instead.
    run(
        &root.join("www"),
        "cargo",
        [
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// The merge gate (plan/01-workspace.md): everything a reviewer agent runs before merging.
/// build-guest runs before test so the host integration tests never see stale prebuilt
/// components under guest/target/components.
fn ci(root: &Path) -> Result<(), String> {
    fmt(root, true)?;
    lint(root)?;
    build(root)?;
    build_guest(root)?;
    test(root)?;
    println!("xtask: ci passed (fmt, lint, build, build-guest, test)");
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
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                // A missing host tool (wasm-tools, qemu, …) used to surface as a bare
                // "No such file or directory (os error 2)", which reads like a missing
                // input file. Point at the setup path instead (plan/01 D10/D11).
                format!(
                    "`{program}` not found — run `make setup` (or `cargo xtask doctor`) to \
                     install the host tools this command needs"
                )
            } else {
                format!("failed to run `{program} {shown}`: {err}")
            }
        })?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{program} {shown}` failed ({status}) in {}",
            dir.display()
        ))
    }
}

// ---------------------------------------------------------------------------
// Packaging: the prebuilt component bundle and the crates.io pre-flight
// ---------------------------------------------------------------------------

/// The version every published crate carries (kept in lockstep via `workspace.package`).
const PUBLISH_VERSION: &str = "0.1.0";

/// The crates published to crates.io, in dependency order (leaves first; `eo9` last).
const PUBLISH_CRATES: &[&str] = &[
    "eo9-component",
    "eo9-store",
    "eo9-providers-unix",
    "eo9-components",
    "eofs-core",
    "eo9-runtime",
    "eo9-embed",
    "eo9",
];

/// Crates whose dependencies are all already on crates.io, so `cargo publish --dry-run`
/// can fully verify them before anything else has been published.
const PUBLISH_LEAF_CRATES: &[&str] = &[
    "eo9-component",
    "eo9-store",
    "eo9-providers-unix",
    "eo9-components",
    "eofs-core",
];

fn components_build_dir(root: &Path) -> PathBuf {
    root.join("guest").join("target").join("components")
}

fn components_data_dir(root: &Path) -> PathBuf {
    root.join("crates").join("eo9-components").join("data")
}

/// The built guest components as sorted `(stem, bytes)` pairs.
///
/// The set is derived from `GUEST_COMPONENTS` — the same list `build-guest` builds — not
/// from whatever `.wasm` files happen to sit in the build directory, so a removed crate's
/// stale artifact can never sneak into the published bundle (and a missing entry is a
/// clear "run build-guest first" error rather than a silently smaller bundle).
fn built_components(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let dir = components_build_dir(root);
    let mut components = Vec::new();
    for package in GUEST_COMPONENTS {
        let path = dir.join(format!("{package}.wasm"));
        let bytes = std::fs::read(&path).map_err(|err| {
            format!(
                "cannot read {} ({err}); run `cargo xtask build-guest` first",
                path.display()
            )
        })?;
        components.push(((*package).to_string(), bytes));
    }
    components.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(components)
}

/// `cargo xtask refresh-components`: copy the built guest components into
/// crates/eo9-components/data/ and regenerate its index, so the bundle a published `eo9`
/// seeds from matches the source tree. Run after `cargo xtask build-guest`; commit the
/// result.
fn refresh_components(root: &Path) -> Result<(), String> {
    let components = built_components(root)?;
    let data = components_data_dir(root);
    if data.exists() {
        std::fs::remove_dir_all(&data)
            .map_err(|err| format!("cannot clear {}: {err}", data.display()))?;
    }
    std::fs::create_dir_all(&data)
        .map_err(|err| format!("cannot create {}: {err}", data.display()))?;

    let mut index = String::from(
        "// Generated by `cargo xtask refresh-components` — do not edit by hand.\n\
         // (file stem, component bytes), sorted by stem.\n\
         static BUNDLED_COMPONENTS: &[(&str, &[u8])] = &[\n",
    );
    let mut total = 0usize;
    for (stem, bytes) in &components {
        std::fs::write(data.join(format!("{stem}.wasm")), bytes)
            .map_err(|err| format!("cannot write {stem}.wasm into the bundle: {err}"))?;
        index.push_str(&format!(
            "    ({stem:?}, include_bytes!({:?}) as &[u8]),\n",
            format!("{stem}.wasm")
        ));
        total += bytes.len();
    }
    index.push_str("];\n");
    std::fs::write(data.join("index.rs"), index)
        .map_err(|err| format!("cannot write the bundle index: {err}"))?;
    println!(
        "xtask: refreshed crates/eo9-components/data: {} components, {} KiB",
        components.len(),
        total / 1024
    );
    Ok(())
}

/// Verify crates/eo9-components/data/ matches the freshly built guest components.
fn check_components_bundle(root: &Path) -> Result<(), String> {
    let built = built_components(root)?;
    let data = components_data_dir(root);
    let mut bundled_names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&data) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wasm")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                bundled_names.push(stem.to_string());
            }
        }
    }
    bundled_names.sort();

    let mut drifted = Vec::new();
    for (stem, bytes) in &built {
        match std::fs::read(data.join(format!("{stem}.wasm"))) {
            Ok(existing) if existing == *bytes => {}
            Ok(_) => drifted.push(format!("{stem} (contents differ)")),
            Err(_) => drifted.push(format!("{stem} (missing from the bundle)")),
        }
    }
    for name in &bundled_names {
        if !built.iter().any(|(stem, _)| stem == name) {
            drifted.push(format!("{name} (no longer built)"));
        }
    }
    if drifted.is_empty() {
        println!(
            "xtask: eo9-components bundle matches the built components ({} components)",
            built.len()
        );
        Ok(())
    } else {
        Err(format!(
            "the eo9-components bundle is stale: {}; run `cargo xtask refresh-components` and commit the result",
            drifted.join(", ")
        ))
    }
}

/// `cargo xtask package`: the publishing pre-flight. Builds the guest components, verifies
/// the bundled set matches them, assembles every publishable crate with `cargo package`,
/// dry-run-publishes the crates whose dependencies are already on crates.io, and prints the
/// publish sequence. Nothing is uploaded.
fn package(root: &Path) -> Result<(), String> {
    build_guest(root)?;
    check_components_bundle(root)?;

    // Leaf crates (all dependencies already on crates.io): a full dry-run publish, which
    // packages and build-verifies each one. The resulting .crate files land in
    // target/package, so their upload sizes can be reported.
    for krate in PUBLISH_LEAF_CRATES {
        // `--registry crates-io` targets crates.io even when a local cargo config replaces
        // the default registry with a mirror (cargo refuses to publish "to" a replaced
        // source); the dry run uploads nothing and needs no token.
        run(
            root,
            "cargo",
            [
                "publish",
                "--dry-run",
                "--registry",
                "crates-io",
                "-p",
                krate,
            ],
        )?;
    }
    println!("xtask: dry-run-verified leaf crates (target/package):");
    for krate in PUBLISH_LEAF_CRATES {
        let crate_file = root
            .join("target")
            .join("package")
            .join(format!("{krate}-{PUBLISH_VERSION}.crate"));
        let size = std::fs::metadata(&crate_file).map(|m| m.len()).unwrap_or(0);
        println!(
            "xtask:   {krate}-{PUBLISH_VERSION}.crate  {} KiB",
            size / 1024
        );
    }

    // The remaining crates depend on the ones above, so cargo cannot package or verify
    // them until those are live on crates.io; validate their manifests and file lists.
    for krate in PUBLISH_CRATES {
        if PUBLISH_LEAF_CRATES.contains(krate) {
            continue;
        }
        run(root, "cargo", ["package", "--list", "-p", krate])?;
    }

    println!(
        "xtask: pre-flight complete. To publish, run (in this order, waiting for each crate\n\
         xtask: to be live on crates.io before the next):"
    );
    for krate in PUBLISH_CRATES {
        println!("xtask:   cargo publish --registry crates-io -p {krate}");
    }
    println!(
        "xtask: note: only the leaf crates are dry-run-verified here — cargo cannot verify\n\
         xtask: the dependent crates until their dependencies are live on crates.io, so\n\
         xtask: `cargo publish` performs that verification at publish time."
    );
    Ok(())
}
