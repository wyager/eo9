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
    "eo9-stub-entropy-none",
    "eo9-stub-entropy-seeded",
    "eo9-stub-fs-memfs",
    "eo9-stub-fs-none",
    "eo9-stub-fs-readonly",
    "eo9-stub-net-deny",
    "eo9-stub-net-none",
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

/// Architectures accepted by `build-kernel` and `qemu` (QEMU bring-up order).
const KERNEL_ARCHES: &[&str] = &["aarch64", "riscv64", "x86_64"];

/// Components baked into the kernel's read-only store image: (guest package, shell name).
/// The shell names follow the same convention the usermode store seeding uses
/// (`eo9-example-hello` → `hello`, `eo9-stub-entropy-seeded` → `entropy.seeded`).
const KERNEL_STORE_COMPONENTS: &[(&str, &str)] = &[
    ("eosh", "eosh"),
    ("eo9-example-hello", "hello"),
    ("eo9-example-outcomes", "outcomes"),
    ("eo9-example-cruncher", "cruncher"),
    ("eo9-example-readwrite", "readwrite"),
    ("eo9-stub-entropy-seeded", "entropy.seeded"),
    ("eo9-stub-time-frozen", "time.frozen"),
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
        "build-web-demo" => {
            expect_no_args("build-web-demo", rest)?;
            build_web_demo(&root)
        }
        "build-web-vm" => {
            expect_no_args("build-web-vm", rest)?;
            build_web_vm(&root)
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
    test                 Refresh the guest components (build-guest), then run host workspace
                         tests and kernel workspace tests (host triple)
    build-guest          Build guest crates for {GUEST_TARGET} and componentize them with
                         `wasm-tools component new` into guest/target/components/*.wasm
    build-web-demo       Build the guest components, then transpile the /try page's set into
                         www/site/try/components/ via www/try-build (commit the result; the
                         deployed site needs no extra tooling). Not part of `ci`.
    build-web-vm         Pre-AOT the web-VM demo components to pulley32, build the wasm32
                         blob (www/web-eo9, the real runtime stack for the /vm page), and
                         install it into www/site/vm/ (commit the result; ci does not need it)
    build-kernel <arch>  Precompile the seed/async canaries, eo9-example-hello, and entropy.seeded for
                         bare metal and build the bootable kernel image (aarch64 only so far;
                         ELF for QEMU's -kernel loader)
    qemu <arch>          Build the kernel image and boot it under QEMU with serial on stdio
                         (aarch64 only so far; exits when the kernel powers off, Ctrl-A X to quit)
    fmt [--check]        Run `cargo fmt --all` in all three workspaces
    lint                 Run `cargo clippy -D warnings` in all three workspaces
    ci                   The merge gate: fmt --check, lint, build, build-guest, test
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

/// Build the guest components and regenerate the eo9.org `/try` page's transpiled bundle.
///
/// The transpiler (`www/try-build`, its own workspace) turns the example components into
/// browser-runnable ES modules + core wasm and writes them, plus the launcher manifest, into
/// `www/site/try/components/`. The output is committed, so this only needs re-running when the
/// shipped components (or the transpiler pin) change; `ci` deliberately does not depend on it.
fn build_web_demo(root: &Path) -> Result<(), String> {
    build_guest(root)?;
    let manifest = root.join("www").join("try-build").join("Cargo.toml");
    let components = root.join("guest").join("target").join("components");
    let out = root.join("www").join("site").join("try").join("components");
    run(
        root,
        "cargo",
        [
            OsStr::new("run"),
            OsStr::new("--release"),
            OsStr::new("--manifest-path"),
            manifest.as_os_str(),
            OsStr::new("--"),
            OsStr::new("--components"),
            components.as_os_str(),
            OsStr::new("--out"),
            out.as_os_str(),
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

    // Build the blob in its own workspace for wasm32-unknown-unknown.
    let manifest = root.join("www").join("web-eo9").join("Cargo.toml");
    run(
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
fn build_store_image(root: &Path) -> Result<PathBuf, String> {
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

    let out_dir = root.join("kernel").join("target").join("precompiled");
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join("store.img");
    std::fs::write(&out_path, &image)
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;
    println!(
        "xtask: assembled store image {} ({} bytes, {} components)",
        out_path.display(),
        image.len(),
        KERNEL_STORE_COMPONENTS.len()
    );
    Ok(out_path)
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
    if arch != "aarch64" {
        return Err(format!(
            "`build-kernel {arch}` is not implemented yet: the bare-metal kernel covers aarch64 \
             only so far (plan/12-kernel.md)"
        ));
    }

    // The seed canary, assembled from WAT.
    let seed_wat = root.join("kernel").join("seed").join("hello.wat");
    let seed_wasm = wat::parse_file(&seed_wat)
        .map_err(|err| format!("failed to assemble {}: {err}", seed_wat.display()))?;
    let seed = precompile_for_kernel(root, &seed_wasm, "seed component", "seed.cwasm")?;

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
    let sleepy = precompile_for_kernel(root, &sleepy_wasm, "sleepy canary", "sleepy.cwasm")?;

    // The real hello program, built from the guest workspace.
    let hello_component = build_guest_component(root, "eo9-example-hello")?;
    let hello_wasm = std::fs::read(&hello_component)
        .map_err(|err| format!("failed to read {}: {err}", hello_component.display()))?;
    let hello = precompile_for_kernel(root, &hello_wasm, "eo9-example-hello", "hello.cwasm")?;

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
    )?;

    // The read-only store image: every listed component plus its host-AOT artifact,
    // keyed by shell name, for the kernel's `program=<name>` selection (and, later,
    // eosh's /bin view).
    let store_image = build_store_image(root)?;

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

/// Precompile a component for the bare-metal target, writing `kernel/target/precompiled/<name>`.
///
/// The artifact must be loadable by the kernel's `no_std` wasmtime engine, so the
/// compilation config mirrors what that engine computes for itself on an OS-less target:
/// no signals-based traps, no virtual-memory reservations or guards, no copy-on-write
/// memory initialization, and no wasm proposals beyond what the kernel build enables
/// (feature unification gives this host build GC, threads, and component-model-async
/// support via eo9-runtime's wasmtime features; the kernel build has none of those).
fn precompile_for_kernel(
    root: &Path,
    component: &[u8],
    what: &str,
    file_name: &str,
) -> Result<PathBuf, String> {
    let mut config = wasmtime::Config::new();
    config
        .target(KERNEL_CHECK_TARGET)
        .map_err(|err| format!("wasmtime rejected target {KERNEL_CHECK_TARGET}: {err:#}"))?;
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
    config.gc_support(false);
    config.wasm_threads(false);
    let engine = wasmtime::Engine::new(&config)
        .map_err(|err| format!("failed to build the kernel-precompile engine: {err:#}"))?;
    let artifact = engine
        .precompile_component(component)
        .map_err(|err| format!("failed to precompile {what}: {err:#}"))?;

    let out_dir = root.join("kernel").join("target").join("precompiled");
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join(file_name);
    std::fs::write(&out_path, &artifact)
        .map_err(|err| format!("failed to write {}: {err}", out_path.display()))?;
    println!(
        "xtask: precompiled {what} -> {} ({} bytes, target {KERNEL_CHECK_TARGET})",
        out_path.display(),
        artifact.len()
    );
    Ok(out_path)
}

/// Build the kernel image for `arch` and boot it under QEMU with serial on stdio.
///
/// The exact invocation (aarch64): `qemu-system-aarch64 -M virt,gic-version=2 -cpu max -smp 1 -m 512M
/// -nographic -kernel <image>`. The kernel powers the machine off via PSCI when its run
/// completes (or on panic), so QEMU exits by itself; to quit earlier press Ctrl-A then X.
fn qemu(root: &Path, arch: &str, append: &[String]) -> Result<(), String> {
    let image = build_kernel(root, arch)?;
    let qemu = format!("qemu-system-{arch}");
    println!(
        "xtask: booting {} under {qemu} (serial on stdio; the kernel powers off when done, \
         or press Ctrl-A then X to quit)",
        image.display()
    );
    let mut args: Vec<std::ffi::OsString> = [
        // Pin GICv2: the kernel brings up the GIC distributor + CPU interface over MMIO
        // (src/gic.rs) to forward the generic-timer interrupt so the executor can wfi-idle.
        // With `-cpu max` QEMU would otherwise default to GICv3 (a system-register CPU
        // interface with per-PE redistributors), which that minimal MMIO bring-up does not
        // drive.
        "-M",
        "virt,gic-version=2",
        "-cpu",
        "max",
        "-smp",
        "1",
        "-m",
        KERNEL_QEMU_MEMORY,
        "-nographic",
        "-kernel",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(image.as_os_str().to_os_string());
    // Anything after the architecture becomes the kernel command line, e.g.
    // `cargo xtask qemu aarch64 program=cruncher seed=9 rounds=200000`.
    if !append.is_empty() {
        args.push("-append".into());
        args.push(append.join(" ").into());
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
