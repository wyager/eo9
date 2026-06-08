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
    "eo9-example-draw",
    "eo9-example-sockcheck",
    "eo9-example-lspci",
    "eo9-example-l2check",
    "eo9-example-l4check",
    "eo9-example-vnicheck",
    "eo9-example-vnic4check",
    "eo9-example-bridgecheck",
    "eo9-example-cancelcheck",
    // The smallest executor: runs another program (a component-typed main argument)
    // and reports how long it took (plan/03 component arguments).
    "eo9-example-time",
    // The shell-over-network supervisor (plan/09 D44, plan/10): serves
    // `net.virtio $ net.l4.over-l2 $ net.text $ eosh` sessions sequentially.
    "eo9-example-telnetd",
    "eosh",
    // The service-boot program (executor v1, docs/design/executor-model.md).
    "init",
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
    "eo9-stub-fs-filtered",
    "eo9-stub-fs-memfs",
    "eo9-stub-fs-none",
    "eo9-stub-fs-overlay",
    "eo9-stub-fs-policy-subtree",
    "eo9-stub-fs-readonly",
    "eo9-stub-gfx-deny",
    "eo9-stub-gfx-mem",
    "eo9-stub-gfx-none",
    "eo9-stub-gpu-virtio",
    "eo9-stub-net-l2-bridge",
    "eo9-stub-net-l2-deny",
    "eo9-stub-net-l2-echo",
    "eo9-stub-net-l2-none",
    "eo9-stub-net-l2-switch",
    "eo9-stub-net-l3-deny",
    "eo9-stub-net-l3-none",
    "eo9-stub-net-l4-deny",
    "eo9-stub-net-l4-filtered",
    "eo9-stub-net-l4-loopback",
    "eo9-stub-net-l4-none",
    "eo9-stub-net-l4-over-l2",
    "eo9-stub-net-policy-ports",
    "eo9-stub-net-rtl8125",
    "eo9-stub-net-text",
    "eo9-stub-net-virtio",
    "eo9-stub-pci-admit-address",
    "eo9-stub-pci-admit-vendor",
    "eo9-stub-pci-deny",
    "eo9-stub-pci-filtered",
    "eo9-stub-pci-none",
    "eo9-stub-perf-none",
    "eo9-stub-perf-null",
    "eo9-stub-restart-always",
    "eo9-stub-restart-backoff",
    "eo9-stub-restart-never",
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
    // The boot supervisor and the standard restart policies (executor v2): the default
    // boot runs init (config: console = eosh), and `detach … restart <policy>` at the
    // metal prompt needs the policy components resolvable under /bin.
    ("init", "init"),
    ("eo9-stub-restart-never", "restart.never"),
    ("eo9-stub-restart-always", "restart.always"),
    ("eo9-stub-restart-backoff", "restart.backoff"),
    ("eo9-example-hello", "hello"),
    // `time <prog>`: the component-argument demo and the smallest executor.
    ("eo9-example-time", "time"),
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
    // The path-policy fs attenuator and its standard subtree policy, so per-path grants
    // compose at the metal prompt: `fs.policy-subtree --prefix /x --access read-only $
    // fs.filtered $ <program>` ("policies are programs", SPEC).
    ("eo9-stub-fs-filtered", "fs.filtered"),
    ("eo9-stub-fs-policy-subtree", "fs.policy-subtree"),
    // The PCI attenuator and its standard admit policies, so a metal composition can
    // grant a driver exactly one device ("policies are programs", SPEC):
    // `pci.admit-address --allow … $ pci.filtered $ lspci` (fixed bus address) or
    // `pci.admit-vendor --allow … $ pci.filtered $ disk.virtio $ …` (device identity).
    ("eo9-stub-pci-filtered", "pci.filtered"),
    ("eo9-stub-pci-admit-address", "pci.admit-address"),
    ("eo9-stub-pci-admit-vendor", "pci.admit-vendor"),
    // The PCI absence stub, so optional-pci programs can be composed to observe "no
    // devices" on metal — and the refusal stub, so required-pci programs can be composed
    // to observe a typed `denied` (`pci.deny $ lspci`) instead of an unsatisfied import.
    ("eo9-stub-pci-none", "pci.none"),
    ("eo9-stub-pci-deny", "pci.deny"),
    // The network stack for real hardware: the virtio-net driver, its link-layer
    // check, the TCP/IP middleware, and its transport-layer check, so the metal shell
    // can compose `net.virtio $ l2check` and `net.virtio $ net.l4.over-l2 $ l4check`
    // against a QEMU user-mode NIC (boot with the `pci` grant and the xtask `net` flag).
    ("eo9-stub-net-virtio", "net.virtio"),
    // The RTL8125 2.5GbE driver — net.virtio's real-silicon sibling for the Orange
    // Pi 5 Plus's two onboard NICs (10ec:8125 behind the RK3588 DW root ports;
    // plan/09 D46, plan/12 board lane). Under QEMU (no RTL8125 model) it refuses
    // typed, naming what it probed; the same compositions swap in on the board:
    //   net.rtl8125 $ l2check --gateway 192.168.1.1
    //   net.rtl8125 $ (net.l4.over-l2 --address … --gateway …) $ l4check --resolver …
    ("eo9-stub-net-rtl8125", "net.rtl8125"),
    ("eo9-example-l2check", "l2check"),
    // The virtual-NIC switch and its two-port check, so the single-owner-NIC sharing
    // demo runs at the metal prompt (one physical NIC, two isolated virtual MACs):
    //   let sw = rename port-a link-a $ rename port-b link-b $ net.l2.switch
    //   net.virtio $ sw $ vnicheck --mode arp
    ("eo9-stub-net-l2-switch", "net.l2.switch"),
    ("eo9-example-vnicheck", "vnicheck"),
    // The 802.1D learning bridge — the switch's trusting sibling (no source rewrite,
    // learned forwarding, flood-on-unknown; plan/09 D42): stacks for fan-out, so the
    // metal payoff is BOTH vnicheck ports completing through a stacked pair:
    //   net.virtio $ (rename port-a eo9:net/l2 $ net.l2.bridge)
    //     $ (rename port-a link-a $ rename port-b link-b $ net.l2.bridge)
    //     $ vnicheck --mode arp
    ("eo9-stub-net-l2-bridge", "net.l2.bridge"),
    ("eo9-stub-net-l4-over-l2", "net.l4.over-l2"),
    ("eo9-example-l4check", "l4check"),
    // The shell over the network (plan/09 D44): the socket-backed text provider and its
    // supervisor, so the metal prompt can serve telnet sessions against the QEMU
    // user-mode NIC (boot with the `pci` grant and the xtask `net telnet` flags, then
    // `telnetd`, then from the host `nc localhost 5555`). Cleartext + unauthenticated —
    // a trusted-LAN/dev tool only; `check-telnet` is the scripted end-to-end gate.
    //   net.virtio $ net.l4.over-l2 $ net.text $ eosh   (what telnetd composes)
    ("eo9-stub-net-text", "net.text"),
    ("eo9-example-telnetd", "telnetd"),
    // The two-stack transport check, so the full shared-link payoff runs at the metal
    // prompt — two l4 stacks, each riding its own switch port, each resolving real DNS
    // (one physical NIC, two virtual MACs, two IP stacks; plan/09 D31):
    //   net.virtio $ (rename port-a link-a $ rename port-b link-b $ net.l2.switch)
    //     $ (rename eo9:net/l4 left $ rename eo9:net/l2 link-a $ net.l4.over-l2)
    //     $ (rename eo9:net/l4 right $ rename eo9:net/l2 link-b
    //        $ net.l4.over-l2 --address 10.0.2.16 --prefix-length 24 --gateway 10.0.2.2)
    //     $ vnic4check --peer 10.0.2.3 --peer-port 53 --mode dns
    ("eo9-example-vnic4check", "vnic4check"),
    // The cancel-mid-flight disk probe (plan/09 D34's executable follow-up): cancels an
    // in-flight disk.virtio read detected via the driver's typed busy error, then
    // verifies later reads byte-for-byte (the drain-before-reuse invariant, live):
    //   pci.admit-address --allow [{segment: 0, bus: 0, device: 1, function: 0}]
    //     $ pci.filtered $ disk.virtio $ cancelcheck --attempts 25
    ("eo9-example-cancelcheck", "cancelcheck"),
    // The per-layer net stubs, the in-memory transport, and the transport conformance
    // check, so the typed-denial and mock-vs-real comparisons run at the metal prompt
    // exactly as they do in usermode (user study 08, finding F4):
    //   net.l2.deny $ net.l4.over-l2 $ l4check   -> the program's own typed denial
    //   net.l4.loopback $ sockcheck --payload p  -> ok: echoed(...)
    ("eo9-stub-net-l2-deny", "net.l2.deny"),
    ("eo9-stub-net-l2-none", "net.l2.none"),
    ("eo9-stub-net-l3-deny", "net.l3.deny"),
    ("eo9-stub-net-l3-none", "net.l3.none"),
    ("eo9-stub-net-l4-deny", "net.l4.deny"),
    ("eo9-stub-net-l4-none", "net.l4.none"),
    ("eo9-stub-net-l4-loopback", "net.l4.loopback"),
    // The transport firewall and its standard port policy ("policies are programs"):
    //   net.policy-ports --allow "[7]" $ net.l4.filtered $ net.l4.loopback-backed program
    ("eo9-stub-net-l4-filtered", "net.l4.filtered"),
    ("eo9-stub-net-policy-ports", "net.policy-ports"),
    ("eo9-example-sockcheck", "sockcheck"),
    // Basic coreutils, so the metal shell can inspect its own (read-only) filesystem:
    // `ls /bin`, `cat /session`, `wc`, `head`, `stat`.
    ("eo9-coreutil-ls", "ls"),
    ("eo9-coreutil-cat", "cat"),
    ("eo9-coreutil-echo", "echo"),
    ("eo9-coreutil-wc", "wc"),
    ("eo9-coreutil-head", "head"),
    ("eo9-coreutil-stat", "stat"),
    // rm completes the writable-store lifecycle on a `storedisk` boot: programs saved at
    // the shell (`save <name> = <expr>`) are removed with `rm /bin/<name>.wasm`. Baked
    // entries refuse removal (read-only), so rm on the standard names is inert.
    ("eo9-coreutil-rm", "rm"),
    // The display stack: the virtio-gpu driver, the RAM framebuffer (the no-hardware
    // deterministic target), the absence/refusal stubs, and the drawing demo, so the
    // metal shell can compose `gpu.virtio $ draw` against a QEMU virtio-gpu (boot with
    // the `pci` grant and the xtask `gpu` flag) or `gfx.mem $ draw` against nothing.
    ("eo9-stub-gpu-virtio", "gpu.virtio"),
    ("eo9-stub-gfx-mem", "gfx.mem"),
    ("eo9-stub-gfx-none", "gfx.none"),
    ("eo9-stub-gfx-deny", "gfx.deny"),
    ("eo9-example-draw", "draw"),
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
            // `build-kernel aarch64 opi5plus [minimal]` builds the Orange Pi 5 Plus board
            // profile and flattens it into the `booti`-bootable Image; everything else is
            // the standard QEMU kernel build.
            match rest {
                [arch, board] if arch == "aarch64" && board == "opi5plus" => {
                    build_kernel_opi5plus(&root, false)?;
                }
                [arch, board, minimal]
                    if arch == "aarch64" && board == "opi5plus" && minimal == "minimal" =>
                {
                    build_kernel_opi5plus(&root, true)?;
                }
                _ => {
                    build_kernel(&root, &arch_arg("build-kernel", rest)?)?;
                }
            }
            Ok(())
        }
        "check-gpu" => {
            expect_no_args("check-gpu", rest)?;
            check_gpu(&root)
        }
        "check-telnet" => {
            expect_no_args("check-telnet", rest)?;
            check_telnet(&root)
        }
        "firstpoll-ab" => {
            let mut rounds: u32 = 5;
            let mut gate_only = false;
            let mut arguments = rest.iter();
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--gate-only" => gate_only = true,
                    "--rounds" => {
                        let value = arguments.next().ok_or_else(|| {
                            "firstpoll-ab: `--rounds` needs a number (e.g. `--rounds 5`)"
                                .to_string()
                        })?;
                        rounds = value.parse().map_err(|_| {
                            format!("firstpoll-ab: `--rounds {value}` is not a number")
                        })?;
                        if rounds == 0 {
                            return Err("firstpoll-ab: `--rounds` must be at least 1".into());
                        }
                    }
                    other => {
                        return Err(format!(
                            "firstpoll-ab: unknown argument `{other}` (accepted: --rounds N, \
                             --gate-only)"
                        ));
                    }
                }
            }
            firstpoll_ab(&root, rounds, gate_only)
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
        "check-components-bundle" => {
            expect_no_args("check-components-bundle", rest)?;
            check_components_bundle(&root)
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
    precompress-site     Write brotli/gzip siblings next to the compressible files under
                         www/site via www/precompress, so the server can serve pre-compressed
                         bytes (runs automatically at the end of the build-web-* commands;
                         commit the result; ci does not need it)
    fingerprint-web-vm   Rename the /vm immutable assets (the wasm blob and .cwasm store
                         images) and copy the page script/style (vm.js/vm.css) to carry a
                         content hash, write vm/assets.json, and drop the
                         old siblings — so they can be cached forever and a rebuild changes the
                         URL (runs automatically inside build-web-vm; commit the result)
    check-web-vm         Verify vm/assets.json matches the committed fingerprinted /vm assets
                         (the names encode the current content hash) — a cheap drift guard over
                         the committed files; no rebuild needed
    build-kernel <arch>  Build the bootable kernel image (an ELF for QEMU's -kernel loader).
                         aarch64: precompiles the seed/async canaries, eo9-example-hello,
                         entropy.seeded, and the store image and embeds them; riscv64: the
                         feature-less image (boot/serial/heap/timer/interrupts so far)
    qemu <arch>          Build the kernel image and boot it under QEMU with serial on stdio
                         (aarch64 or riscv64; exits when the kernel powers off, Ctrl-A X to quit)
    check-gpu            Boot the aarch64 kernel under QEMU with a virtio-gpu (pci gpu), drive
                         `gpu.virtio $ draw` (one frame, then the two-frame partial-damage run)
                         at the serial eosh prompt, screendump the scanout over QMP after each,
                         and compare both images pixel-for-pixel against the independently
                         computed expected pattern
    check-telnet         Boot the aarch64 kernel under QEMU with a user-mode NIC and a slirp
                         host-forward (pci net telnet), drive `telnetd --sessions 2` at the
                         serial eosh prompt, and validate the shell-over-network end to end
                         from the host: connect to localhost:5555, see the greeting and the
                         eosh prompt, run `hello`, verify a concurrent second connection is
                         refused, `exit` closes the connection, and a second sequential
                         session works independently
    firstpoll-ab [--rounds N] [--gate-only]
                         A/B gate for the vendored `first-poll-inline` feature
                         (docs/spikes/first-poll-inline.md): run the async-hardening matrix,
                         the eager-guest pins, and the real-chain suites in BOTH arms of
                         tests/firstpoll-ab (feature off, then on) for a semantic-identity
                         verdict, then N interleaved A/B timing rounds (default 5) reported
                         as medians with spread; --gate-only skips the timing rounds. The
                         standing regression gate for any vendored-async change
    fmt [--check]        Run `cargo fmt --all` in all three workspaces
    lint                 Run `cargo clippy -D warnings` in all three workspaces
    ci                   The merge gate: fmt --check, lint, build, build-guest, test
    doctor               Check the host prerequisites (rustup, the pinned nightly, the wasm32
                         target, the wasm-tools CLI; QEMU and node are optional) and print
                         install hints for anything missing
    refresh-components   Copy the built guest components into crates/eo9-bundled-programs/data/ and
                         regenerate its index — the prebuilt set a `cargo install eo9` build
                         seeds from (run after build-guest; commit the result)
    check-components-bundle
                         Verify crates/eo9-bundled-programs/data/ matches the built guest components
                         byte-for-byte (run by `package`; needs build-guest first — note that
                         fs-eofs only matches when built from the checkout that last refreshed
                         the bundle, see plan/01 D15)
    package              Publishing pre-flight: build-guest, verify crates/eo9-bundled-programs/data/
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
                "  MISSING  wasm-tools — run `cargo install --locked wasm-tools --version '~{PINNED_WASM_TOOLS_CLI}'` (or `make setup`)"
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

/// Validate feature set for componentized guests. Part of every componentize stamp
/// AND the `wasm-tools validate` invocation: one constant, so the stamp cannot drift
/// from the flags actually passed.
const COMPONENT_VALIDATE_FEATURES: &str = "cm-async,cm-implements";

/// One guest-workspace build per xtask invocation (see [`ensure_guest_workspace_built`]).
static GUEST_WORKSPACE_BUILT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `EO9_FORCE_REBUILD=1` (any non-empty value other than `0`) disables every freshness
/// skip below (componentize, kernel precompile); `make gfx FORCE=1` exports it. Outputs
/// are still written through [`write_if_different`], so a forced run that reproduces
/// identical bytes does not trigger a kernel relink.
fn force_rebuild() -> bool {
    std::env::var("EO9_FORCE_REBUILD")
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

/// Content fingerprint for freshness stamps (blake3, hex).
fn fingerprint_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A step is fresh when its output exists and its stamp records exactly the expected
/// input fingerprint. Stamps record *inputs only* (the cargo contract): a hand-corrupted
/// output is not detected, exactly as with any cargo target dir.
fn stamp_fresh(out: &Path, stamp: &Path, fingerprint: &str) -> bool {
    !force_rebuild()
        && out.is_file()
        && std::fs::read_to_string(stamp)
            .map(|recorded| recorded == fingerprint)
            .unwrap_or(false)
}

fn write_stamp(stamp: &Path, fingerprint: &str) -> Result<(), String> {
    std::fs::write(stamp, fingerprint)
        .map_err(|err| format!("failed to write {}: {err}", stamp.display()))
}

/// Write only when the content differs, returning whether a write happened. Keeping
/// mtimes stable on unchanged outputs is what lets the kernel's build.rs
/// (`cargo:rerun-if-changed` on every embedded artifact path) skip the rebuild+relink
/// on warm runs.
fn write_if_different(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    if std::fs::read(path).map(|old| old == bytes).unwrap_or(false) {
        return Ok(false);
    }
    std::fs::write(path, bytes)
        .map(|()| true)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// `wasm-tools --version`, cached for the process (it is part of every componentize
/// stamp: a wasm-tools upgrade must invalidate componentized outputs).
fn wasm_tools_version() -> Result<String, String> {
    static VERSION: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            let output = Command::new("wasm-tools")
                .arg("--version")
                .output()
                .map_err(|err| format!("failed to run wasm-tools --version: {err}"))?;
            if !output.status.success() {
                return Err("wasm-tools --version failed".to_string());
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .clone()
}

fn build_guest(root: &Path) -> Result<(), String> {
    ensure_guest_workspace_built(root)?;
    for package in GUEST_COMPONENTS {
        componentize_guest_package(root, package)?;
    }
    Ok(())
}

/// Build the whole guest workspace for wasm32, once per xtask invocation. Every
/// componentize/precompile consumer funnels through this single `cargo build
/// --workspace` (cargo's own freshness makes the warm case one fast no-op check),
/// replacing the old one-cargo-spawn-per-package pattern that dominated warm
/// `build-kernel` runs.
fn ensure_guest_workspace_built(root: &Path) -> Result<(), String> {
    if GUEST_WORKSPACE_BUILT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    let guest = root.join("guest");
    // Remapped paths make the component bytes identical from any checkout, so the
    // eo9-bundled-programs bundle (and the ci drift check over it) is checkout-independent.
    let remap = remap_rustflags(root);
    run_with_env(
        &guest,
        "cargo",
        [
            "build",
            "--workspace",
            "--release",
            "--target",
            GUEST_TARGET,
        ],
        &[("RUSTFLAGS", remap.as_os_str())],
    )
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
    // Freshness: the componentized output is a pure function of the core module bytes,
    // the wasm-tools binary, and the validate feature set. Skip the two wasm-tools
    // subprocesses when none of those changed since the recorded stamp.
    let module_bytes = std::fs::read(&module)
        .map_err(|err| format!("failed to read {}: {err}", module.display()))?;
    let stamp = components_dir.join(format!("{package}.wasm.stamp"));
    let fingerprint = format!(
        "module={} {} features={COMPONENT_VALIDATE_FEATURES}",
        fingerprint_bytes(&module_bytes),
        wasm_tools_version()?,
    );
    if stamp_fresh(&component, &stamp, &fingerprint) {
        return Ok(component);
    }
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
            OsStr::new(COMPONENT_VALIDATE_FEATURES),
            component.as_os_str(),
        ],
    )?;
    write_stamp(&stamp, &fingerprint)?;
    println!("xtask: built component {}", component.display());
    Ok(component)
}

/// Build one guest crate and componentize it (the targeted version of [`build_guest`],
/// used by `build-kernel` to refresh just the program it embeds).
fn build_guest_component(root: &Path, package: &str) -> Result<PathBuf, String> {
    ensure_guest_workspace_built(root)?;
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
        ("time.fuzzy", "eo9-stub-time-fuzzy"),
        // The browser-runnable spread of the kernel store (plan/18 D39): pixels, sockets,
        // the policy attenuators, and the virtual-NIC switch — every chain below is
        // all-guest (or roots in the page's own providers), so the full composition runs
        // client-side, fused and compiled in-blob exactly like `entropy.seeded $ rng`.
        // Pixels: `gfx.mem $ draw` round-trips the deterministic pattern and reports its
        // checksum (and the page's own canvas provider serves a bare `draw` — see
        // providers.rs).
        ("gfx.mem", "eo9-stub-gfx-mem"),
        ("draw", "eo9-example-draw"),
        // Sockets: `net.l4.loopback $ sockcheck` exercises real TCP/UDP semantics on a
        // loopback transport, entirely in the page.
        ("net.l4.loopback", "eo9-stub-net-l4-loopback"),
        ("sockcheck", "eo9-example-sockcheck"),
        // The policy attenuators ("policies are programs", SPEC): per-path fs grants and
        // the transport firewall, composed at the browser prompt like the metal one.
        ("fs.filtered", "eo9-stub-fs-filtered"),
        ("fs.policy-subtree", "eo9-stub-fs-policy-subtree"),
        ("net.l4.filtered", "eo9-stub-net-l4-filtered"),
        ("net.policy-ports", "eo9-stub-net-policy-ports"),
        // The virtual-NIC switch over the echo fixture: one upstream link, two isolated
        // virtual MACs, the whole switching policy verified by vnicheck — pure-guest
        // networking through the switch, in the browser.
        ("net.l2.switch", "eo9-stub-net-l2-switch"),
        ("net.l2.echo", "eo9-stub-net-l2-echo"),
        ("vnicheck", "eo9-example-vnicheck"),
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
        // Precompile the *executable* form: components whose worlds carry named interface
        // exports (the virtual-NIC switch's ports) encode an `implements` annotation the
        // pinned wasmtime parser predates; stripping it is behavior-neutral and is exactly
        // what the kernel-store and in-blob compile paths do (eo9-component,
        // `executable_bytes`). The raw bytes above keep the full encoding, so the algebra
        // side stays lossless.
        let executable = eo9_component::Component::load(raw.clone())
            .map_err(|err| {
                format!("/bin component `{name}` does not load as an eo9 module: {err:?}")
            })?
            .executable_bytes();
        preaot_for_web(
            &artifacts,
            &executable,
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
    let remap_flags = remap_rustflags(root);
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

/// `RUSTFLAGS` for reproducible wasm builds (the guest components and the wasm32 blob):
/// remap the absolute path prefixes that would otherwise end up in panic-location strings,
/// so the built bytes do not depend on where the repository happens to be checked out, the
/// cargo home, or the rustup home. This is what lets the eo9-bundled-programs bundle be compared
/// byte-for-byte from any checkout (study 11 D9b: the ci drift check), and what keeps the
/// blob's fingerprinted URL stable. Any RUSTFLAGS already present are preserved.
fn remap_rustflags(root: &Path) -> OsString {
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

/// One asset the web-VM fingerprint step processes.
struct FingerprintEntry {
    /// The canonical (un-hashed) file.
    canonical: PathBuf,
    /// The logical key for the manifest ("blob", "store/<name>", "page/<file>").
    key: String,
    /// Whether the canonical file stays after fingerprinting. Build artifacts (the blob,
    /// the store images) are *renamed* — the hashed name is the only copy. Hand-edited
    /// page sources (vm.js, vm.css) are *copied* — the canonical file remains the
    /// editable source (and a no-cache fallback URL), and the hashed copy is what the
    /// page references so the CDN can cache it forever.
    keep_canonical: bool,
}

/// The `/vm` immutable assets that get content-fingerprinted: the wasm blob, every Pulley
/// `.cwasm` store image, and the page's own script and style. Their URLs become the
/// version, so they can be cached forever.
fn web_vm_fingerprint_plan(site_dir: &Path) -> Result<Vec<FingerprintEntry>, String> {
    let mut plan = vec![FingerprintEntry {
        canonical: site_dir.join("web-eo9.wasm"),
        key: "blob".to_owned(),
        keep_canonical: false,
    }];
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
        plan.push(FingerprintEntry {
            canonical: entry,
            key: format!("store/{name}"),
            keep_canonical: false,
        });
    }
    // The page's own script and style: hashed copies referenced by index.html, so they get
    // the same immutable/edge-cacheable treatment as the blob instead of per-request
    // revalidation.
    for page_asset in ["vm.js", "vm.css"] {
        let canonical = site_dir.join(page_asset);
        if canonical.exists() {
            plan.push(FingerprintEntry {
                canonical,
                key: format!("page/{page_asset}"),
                keep_canonical: true,
            });
        }
    }
    Ok(plan)
}

/// Does a quoted HTML attribute value reference this `/vm` page asset, either by its
/// canonical name (`/vm/vm.js`) or by any previously-fingerprinted name
/// (`/vm/vm.<16-hex>.js`)?
fn html_ref_matches_page_asset(token: &str, canonical: &str) -> bool {
    let Some(name) = token.strip_prefix("/vm/") else {
        return false;
    };
    if name == canonical {
        return true;
    }
    let Some((stem, ext)) = canonical.rsplit_once('.') else {
        return false;
    };
    let Some(hash) = name
        .strip_prefix(&format!("{stem}."))
        .and_then(|rest| rest.strip_suffix(&format!(".{ext}")))
    else {
        return false;
    };
    hash.len() == 16
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Rewrite `vm/index.html` so its script/style references point at the current
/// fingerprinted copies. `replacements` maps canonical names ("vm.js") to fingerprinted
/// ones ("vm.<hash>.js"). The rewrite is token-based on quote-delimited attribute values,
/// so it is idempotent across rebuilds (a previously-rewritten reference is recognized
/// and re-pointed).
fn rewrite_page_asset_refs(
    site_dir: &Path,
    replacements: &[(String, String)],
) -> Result<(), String> {
    if replacements.is_empty() {
        return Ok(());
    }
    let index_path = site_dir.join("index.html");
    let html = std::fs::read_to_string(&index_path)
        .map_err(|err| format!("failed to read {}: {err}", index_path.display()))?;
    let mut out = String::with_capacity(html.len());
    for (i, token) in html.split('"').enumerate() {
        if i > 0 {
            out.push('"');
        }
        let replaced = replacements.iter().find_map(|(canonical, hashed)| {
            html_ref_matches_page_asset(token, canonical).then(|| format!("/vm/{hashed}"))
        });
        match replaced {
            Some(reference) => out.push_str(&reference),
            None => out.push_str(token),
        }
    }
    if out != html {
        std::fs::write(&index_path, out)
            .map_err(|err| format!("failed to write {}: {err}", index_path.display()))?;
        println!("xtask: pointed vm/index.html at the fingerprinted page assets");
    }
    Ok(())
}

/// Whether a path is a content-fingerprinted immutable asset (`name.<16-hex>.wasm` /
/// `.cwasm` / `.js` / `.css`).
/// Mirrors `eo9_www::is_fingerprinted`; duplicated here so xtask stays dependency-light (it
/// must not pull in the web-server crate). Keep the two in sync.
fn is_fingerprinted_name(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !matches!(ext.as_str(), "wasm" | "cwasm" | "js" | "css") {
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
    let mut page_replacements: Vec<(String, String)> = Vec::new();
    for entry in plan {
        let canonical = &entry.canonical;
        let bytes = std::fs::read(canonical)
            .map_err(|err| format!("failed to read {}: {err}", canonical.display()))?;
        let hash = content_fingerprint(&bytes);
        let new_name = fingerprinted_name(canonical, &hash);
        let new_path = canonical.with_file_name(&new_name);
        if entry.keep_canonical {
            // Page sources: the hashed file is a copy; the canonical stays as the
            // editable source (served no-cache if requested directly).
            std::fs::copy(canonical, &new_path)
                .map_err(|err| format!("failed to copy {}: {err}", canonical.display()))?;
            let canonical_name = canonical
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            page_replacements.push((canonical_name, new_name.clone()));
        } else {
            // Build artifacts: renamed — the hashed name is the only copy.
            std::fs::rename(canonical, &new_path)
                .map_err(|err| format!("failed to rename {}: {err}", canonical.display()))?;
            // Drop the canonical file's stale precompressed siblings; precompress
            // regenerates them for the fingerprinted name.
            remove_with_siblings(canonical);
        }
        // URL the page fetches: relative to the site root, always `/vm/...`.
        let rel = new_path
            .strip_prefix(root.join("www").join("site"))
            .map_err(|_| "fingerprinted asset escaped the site root".to_owned())?;
        let url = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
        manifest_entries.push((entry.key, url));
    }

    // index.html references its own script/style by name; point it at the hashed copies.
    rewrite_page_asset_refs(&site_dir, &page_replacements)?;

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
    let mut page: Vec<(String, String)> = Vec::new();
    let mut store: Vec<(String, String)> = Vec::new();
    for (key, url) in entries {
        if let Some(name) = key.strip_prefix("store/") {
            store.push((name.to_owned(), url.clone()));
        } else if let Some(name) = key.strip_prefix("page/") {
            page.push((name.to_owned(), url.clone()));
        } else if key == "blob" {
            blob = url.clone();
        } else {
            store.push((key.clone(), url.clone()));
        }
    }
    let section = |json: &mut String, name: &str, entries: &[(String, String)], last: bool| {
        json.push_str(&format!("  {}: {{\n", json_string(name)));
        for (i, (name, url)) in entries.iter().enumerate() {
            let comma = if i + 1 < entries.len() { "," } else { "" };
            json.push_str(&format!(
                "    {}: {}{comma}\n",
                json_string(name),
                json_string(url)
            ));
        }
        json.push_str(if last { "  }\n" } else { "  },\n" });
    };
    let mut json = String::from("{\n");
    json.push_str(&format!("  \"blob\": {},\n", json_string(&blob)));
    // The page's own fingerprinted script/style; index.html references them directly, the
    // manifest records them so check-web-vm covers them like every other hashed asset.
    section(&mut json, "page", &page, false);
    section(&mut json, "store", &store, true);
    json.push('}');
    json.push('\n');
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
    build_store_image_filtered(root, target, KERNEL_STORE_COMPONENTS, "store.img")
}

/// Assemble a store image from an explicit component list. The full QEMU kernels bake
/// [`KERNEL_STORE_COMPONENTS`]; the Orange Pi 5 Plus *minimal* image (first-light fast
/// iteration over `fatload`) bakes just enough to run `program=hello`.
fn build_store_image_filtered(
    root: &Path,
    target: &str,
    components: &[(&str, &str)],
    file_name: &str,
) -> Result<PathBuf, String> {
    let mut image: Vec<u8> = Vec::new();
    image.extend_from_slice(b"EO9STOR2");
    image.extend_from_slice(&u32::try_from(components.len()).unwrap().to_le_bytes());
    for (package, shell_name) in components {
        let component_path = build_guest_component(root, package)?;
        let component = std::fs::read(&component_path)
            .map_err(|err| format!("failed to read {}: {err}", component_path.display()))?;
        // Precompile the *executable* form: components whose worlds carry named
        // interface exports (e.g. the virtual-NIC switch's ports) encode an
        // `implements` annotation the pinned wasmtime parser predates; stripping it is
        // behavior-neutral and is exactly what the usermode and kernel compile paths
        // do (eo9-component, `executable_bytes`). The store keeps the full bytes, so
        // the algebra side stays lossless.
        let executable = eo9_component::Component::load(component.clone())
            .map_err(|err| {
                format!("store component `{shell_name}` does not load as an eo9 module: {err:?}")
            })?
            .executable_bytes();
        let artifact_path = precompile_for_kernel(
            root,
            &executable,
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
    let out_path = out_dir.join(file_name);
    let wrote = write_if_different(&out_path, &image)?;
    println!(
        "xtask: assembled store image {} ({} bytes, {} components, target {target}{})",
        out_path.display(),
        image.len(),
        components.len(),
        if wrote { "" } else { ", unchanged" }
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

/// The kernel feature list, with `EO9_KERNEL_FEATURES_EXTRA` appended and
/// `EO9_KERNEL_FEATURES_REMOVE` stripped when set.
///
/// MEASUREMENT BUILDS ONLY: these hooks exist so an A/B evaluation can build the kernel
/// with a vendored feature added or removed without any change to the standard feature
/// lists. Default builds set neither, so `make qemu`, `check-gpu`, and CI are
/// unaffected; nothing in the repo sets them either — they are typed by hand for a
/// measurement run:
///   - append: `EO9_KERNEL_FEATURES_EXTRA=some-feature cargo xtask qemu aarch64`
///   - remove (the `first-poll-inline` escape hatch — the feature is default-on since
///     the GO ruling, docs/spikes/first-poll-inline.md "Default-on"):
///     `EO9_KERNEL_FEATURES_REMOVE=first-poll-inline cargo xtask qemu aarch64 pci disk`
fn kernel_features(base: &str) -> String {
    let mut features: Vec<String> = base.split(',').map(str::to_string).collect();
    if let Ok(extra) = std::env::var("EO9_KERNEL_FEATURES_EXTRA")
        && !extra.trim().is_empty()
    {
        let extra = extra.trim();
        println!(
            "xtask: MEASUREMENT BUILD — appending kernel feature(s) `{extra}` \
             (EO9_KERNEL_FEATURES_EXTRA)"
        );
        features.extend(extra.split(',').map(|f| f.trim().to_string()));
    }
    if let Ok(remove) = std::env::var("EO9_KERNEL_FEATURES_REMOVE")
        && !remove.trim().is_empty()
    {
        let names: Vec<&str> = remove.split(',').map(str::trim).collect();
        println!(
            "xtask: MEASUREMENT BUILD — removing kernel feature(s) `{}` \
             (EO9_KERNEL_FEATURES_REMOVE)",
            names.join(",")
        );
        features.retain(|f| !names.contains(&f.as_str()));
    }
    features.join(",")
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
    write_if_different(&seed_wasm_path, &seed_wasm)?;

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
            kernel_features(
                "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen,first-poll-inline",
            )
            .as_str(),
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

/// Build the Orange Pi 5 Plus (RK3588) board-profile kernel and flatten it into the
/// `booti`-bootable arm64 `Image` (docs/board/orange-pi-5-plus.md). `minimal` bakes a
/// hello-only store for fast first-light iteration over `fatload`; the full variant bakes
/// the standard [`KERNEL_STORE_COMPONENTS`].
fn build_kernel_opi5plus(root: &Path, minimal: bool) -> Result<PathBuf, String> {
    // The same host-precompiled artifacts as the QEMU aarch64 build (identical wasm
    // engine/target config — the board feature changes hardware constants, not codegen).
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
    let seed_wasm_path = root
        .join("kernel")
        .join("target")
        .join("precompiled")
        .join("seed.wasm");
    std::fs::create_dir_all(seed_wasm_path.parent().unwrap())
        .map_err(|err| format!("failed to create precompiled dir: {err}"))?;
    write_if_different(&seed_wasm_path, &seed_wasm)?;

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

    let store_image = if minimal {
        build_store_image_filtered(
            root,
            KERNEL_CHECK_TARGET,
            // hello = the smoke program; lspci = the PCIe bring-up acceptance (the
            // RTL8125s visible through the DW config shim — docs/board/rk3588-pcie.md).
            &[
                ("eo9-example-hello", "hello"),
                ("eo9-example-lspci", "lspci"),
            ],
            "store-opi5plus-min.img",
        )?
    } else {
        build_store_image(root, KERNEL_CHECK_TARGET)?
    };
    let mac_key = ensure_storedisk_mac_key(root)?;

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
            kernel_features(
                "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen,first-poll-inline,\
                 board-opi5plus",
            )
            .as_str(),
        ],
        &[
            ("EO9_SEED_CWASM", seed.as_os_str()),
            ("EO9_SEED_WASM", seed_wasm_path.as_os_str()),
            ("EO9_HELLO_CWASM", hello.as_os_str()),
            ("EO9_SLEEPY_CWASM", sleepy.as_os_str()),
            ("EO9_ENTROPY_SEEDED_CWASM", entropy.as_os_str()),
            ("EO9_STORE_IMAGE", store_image.as_os_str()),
            ("EO9_STOREDISK_MAC_KEY", mac_key.as_os_str()),
        ],
    )?;

    let elf = kernel_dir
        .join("target")
        .join(KERNEL_CHECK_TARGET)
        .join("release")
        .join("eo9-kernel");
    let elf_bytes = std::fs::read(&elf)
        .map_err(|err| format!("failed to read kernel ELF {}: {err}", elf.display()))?;
    let flat = flatten_kernel_elf(&elf_bytes)?;
    // The arm64 Linux Image header booti checks: magic "ARM\x64" at byte 56.
    if flat.len() < 64 || &flat[56..60] != b"ARM\x64" {
        return Err(
            "flattened board image is missing the arm64 Image header (expected the \
             `.text.header` section first — check linker-aarch64-opi5plus.ld)"
                .into(),
        );
    }
    let out = kernel_dir.join("target").join(if minimal {
        "eo9-opi5plus-min.img"
    } else {
        "eo9-opi5plus.img"
    });
    write_if_different(&out, &flat)?;
    println!(
        "xtask: built Orange Pi 5 Plus Image {} ({:.1} MiB)",
        out.display(),
        flat.len() as f64 / (1024.0 * 1024.0)
    );
    println!("xtask: PREFERRED transport: the UART serial-loader stub (boards/opi5-serial-loader)");
    println!("       — mm-poke the stub, `go 0x04000000`, then send_image.py streams this file");
    println!("       to 0x00200000 and jumps. The vendor U-Boot's booti DATA-ABORTS on minimal");
    println!("       images (see .claude/board-bringup/BOOT.md) and is untested for this one;");
    println!("       if trying it anyway from a FAT stick:");
    println!("         usb start");
    println!("         setenv bootargs program=hello   (minimal image: required)");
    println!("         fatload usb 0:1 0x00200000 <file on the stick>");
    println!(
        "         booti 0x00200000 - <fdt_blob from bdinfo>   (fdtcontroladdr is unset in the vendor env)"
    );
    Ok(out)
}

/// Flatten a kernel ELF into the raw binary `booti` expects: every PT_LOAD segment with
/// file contents, laid out at its physical address relative to the lowest one. (.bss and
/// the boot stack carry no file bytes; the Image header's `image_size` reserves them.)
fn flatten_kernel_elf(elf: &[u8]) -> Result<Vec<u8>, String> {
    if elf.len() < 64 || &elf[0..4] != b"\x7fELF" {
        return Err("kernel image is not an ELF".into());
    }
    let read_u64 = |at: usize| u64::from_le_bytes(elf[at..at + 8].try_into().unwrap());
    let read_u32 = |at: usize| u32::from_le_bytes(elf[at..at + 4].try_into().unwrap());
    let read_u16 = |at: usize| u16::from_le_bytes(elf[at..at + 2].try_into().unwrap());
    let phoff = read_u64(32) as usize;
    let phentsize = read_u16(54) as usize;
    let phnum = read_u16(56) as usize;
    let mut segments: Vec<(u64, usize, usize)> = Vec::new(); // (paddr, file offset, len)
    for index in 0..phnum {
        let at = phoff + index * phentsize;
        if elf.len() < at + 56 {
            return Err("kernel ELF program header out of bounds".into());
        }
        if read_u32(at) != 1 {
            continue; // not PT_LOAD
        }
        let p_offset = read_u64(at + 8) as usize;
        let p_paddr = read_u64(at + 24);
        let p_filesz = read_u64(at + 32) as usize;
        if p_filesz == 0 {
            continue;
        }
        if elf.len() < p_offset + p_filesz {
            return Err("kernel ELF segment out of bounds".into());
        }
        segments.push((p_paddr, p_offset, p_filesz));
    }
    if segments.is_empty() {
        return Err("kernel ELF has no loadable segments".into());
    }
    let base = segments.iter().map(|s| s.0).min().unwrap();
    let end = segments.iter().map(|s| s.0 + s.2 as u64).max().unwrap();
    let mut flat = vec![0u8; (end - base) as usize];
    for (paddr, offset, len) in segments {
        let dst = (paddr - base) as usize;
        flat[dst..dst + len].copy_from_slice(&elf[offset..offset + len]);
    }
    Ok(flat)
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
    write_if_different(&seed_wasm_path, &seed_wasm)?;

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
            kernel_features(
                "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen,first-poll-inline",
            )
            .as_str(),
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
    write_if_different(&seed_wasm_path, &seed_wasm)?;

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

    // The MAC key for the persistent store disk's compile cache: artifacts read back from
    // the disk are only deserialized after their keyed-blake3 tag verifies against this
    // key, which is baked into the kernel image (see kernel diskcache). Generated once per
    // checkout and reused so the cache survives kernel rebuilds; never committed.
    let mac_key = ensure_storedisk_mac_key(root)?;

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
            // `wasm-storedisk` (the persistent compile cache behind the `storedisk` boot
            // token) is aarch64-only for now: the kernel's ECAM bring-up is aarch64-virt
            // specific, so the other targets build without it.
            kernel_features(
                "wasm-seed,wasm-hello,wasm-async,wasm-store,wasm-codegen,wasm-storedisk,first-poll-inline",
            )
            .as_str(),
        ],
        &[
            ("EO9_SEED_CWASM", seed.as_os_str()),
            ("EO9_SEED_WASM", seed_wasm_path.as_os_str()),
            ("EO9_HELLO_CWASM", hello.as_os_str()),
            ("EO9_SLEEPY_CWASM", sleepy.as_os_str()),
            ("EO9_ENTROPY_SEEDED_CWASM", entropy.as_os_str()),
            ("EO9_STORE_IMAGE", store_image.as_os_str()),
            ("EO9_STOREDISK_MAC_KEY", mac_key.as_os_str()),
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
/// Bump whenever anything compile-relevant changes in [`precompile_for_kernel`]'s
/// engine configuration (flag changes, or a wasmtime upgrade through the workspace
/// pin): the freshness stamps key on it. Partial safety net for a missed bump:
/// wasmtime embeds its engine version and compatibility-relevant settings (trap
/// model, memory layout, target features) in every artifact and the kernel refuses
/// a mismatch loudly at boot — but settings that only change codegen quality
/// (e.g. opt level) are NOT refused, so a missed bump there silently keeps the old
/// (semantically equivalent) artifacts. The bump discipline is load-bearing for
/// that class; when in doubt, bump.
const PRECOMPILE_CONFIG_REV: u32 = 1;

fn precompile_for_kernel(
    root: &Path,
    component: &[u8],
    what: &str,
    file_name: &str,
    target: &str,
) -> Result<PathBuf, String> {
    let out_dir = kernel_precompiled_dir(root, target);
    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_path = out_dir.join(file_name);
    // Freshness: the artifact is a pure function of the input component bytes, the
    // compilation target, and the engine configuration (keyed by PRECOMPILE_CONFIG_REV).
    let stamp = out_dir.join(format!("{file_name}.stamp"));
    let fingerprint = format!(
        "input={} target={target} config-rev={PRECOMPILE_CONFIG_REV}",
        fingerprint_bytes(component),
    );
    if stamp_fresh(&out_path, &stamp, &fingerprint) {
        return Ok(out_path);
    }
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

    write_if_different(&out_path, &artifact)?;
    write_stamp(&stamp, &fingerprint)?;
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

/// Path of the persistent store-disk image the `storedisk` QEMU argument attaches,
/// creating it (blank, 64 MiB) on first use. The kernel formats a blank disk with eofs and
/// keeps its compile cache on it, so this file is what makes on-target compile results
/// survive across QEMU runs. Distinct from the scratch disk above so the guest-facing
/// `disk` demo (which treats its disk as expendable) never clobbers the kernel's cache.
fn ensure_store_disk(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("kernel").join("target");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let path = dir.join("eo9-store-disk.raw");
    if !path.exists() {
        let file = std::fs::File::create(&path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        file.set_len(64 * 1024 * 1024)
            .map_err(|err| format!("failed to size {}: {err}", path.display()))?;
        println!(
            "xtask: created blank 64 MiB store disk at {}",
            path.display()
        );
    }
    Ok(path)
}

/// Path of the 32-byte MAC key baked into the aarch64 kernel image for the store-disk
/// compile cache (see kernel diskcache): generated from /dev/urandom on first use and
/// reused on later builds so the on-disk cache stays valid across kernel rebuilds. Lives
/// under kernel/target (never committed); deleting it rotates the key, which simply makes
/// the kernel reject and recompile every previously cached artifact.
fn ensure_storedisk_mac_key(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("kernel").join("target");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let path = dir.join("eo9-storedisk-mac.key");
    if !path.exists() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut key = [0u8; 32];
        let mut urandom = std::fs::File::open("/dev/urandom").map_err(|err| {
            format!("failed to open /dev/urandom for the store-disk MAC key: {err}")
        })?;
        std::io::Read::read_exact(&mut urandom, &mut key).map_err(|err| {
            format!("failed to read 32 random bytes for the store-disk MAC key: {err}")
        })?;
        // Owner-only from the moment the file exists (mode 0o600 at create, not chmod'd
        // after); the key also ends up inside the kernel image, so this is hygiene rather
        // than a hard secrecy boundary — see plan/12 D56.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
        file.write_all(&key)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        println!(
            "xtask: generated a store-disk MAC key at {}",
            path.display()
        );
    }
    let metadata = std::fs::metadata(&path)
        .map_err(|err| format!("failed to stat {}: {err}", path.display()))?;
    if metadata.len() != 32 {
        return Err(format!(
            "{} is not 32 bytes; delete it and rebuild to regenerate the store-disk MAC key",
            path.display()
        ));
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
/// `cargo xtask qemu aarch64 pci net`. A bare `gpu` argument attaches a virtio-gpu PCI
/// function pinned at 640x480 plus a QMP control socket, so the `gpu.virtio` driver has
/// a display to claim and `check-gpu` can screendump it — `cargo xtask qemu aarch64 pci gpu`.
/// Adding the bare `display` argument (requires `gpu`) opens QEMU's framebuffer window
/// instead of running headless, with the serial console multiplexed on stdio, so the
/// scanout is visible while you type at the prompt — `cargo xtask qemu aarch64 pci gpu
/// display`, then `gpu.virtio $ draw` (this is what `make gfx` runs).
///
/// A bare `telnet` argument implies `net` and adds a slirp host-forward to the user-mode
/// netdev (`hostfwd=tcp:127.0.0.1:5555-:23`), so a guest telnet daemon listening on port
/// 23 (`telnetd`, plan/09 D44 — cleartext, unauthenticated, dev use only) is reachable
/// from the host as `nc localhost 5555` — `cargo xtask qemu aarch64 pci net telnet`, then
/// `telnetd` at the serial prompt. The forward binds the host's loopback only: the
/// session is unauthenticated, so it must never be reachable from beyond the dev machine.
///
/// A bare `storedisk` argument attaches the *persistent* store-disk image and also stays on
/// the kernel command line: the kernel claims that virtio-blk function for its own
/// disk-backed compile cache (on-target compile results survive reboots) —
/// `cargo xtask qemu aarch64 storedisk`. Don't combine it with the guest-facing `disk`/`pci`
/// flags in the same boot until machine-global device claiming lands.
fn qemu(root: &Path, arch: &str, append: &[String]) -> Result<(), String> {
    let image = build_kernel(root, arch)?;
    let qemu = format!("qemu-system-{arch}");
    println!(
        "xtask: booting {} under {qemu} (serial on stdio; the kernel powers off when done, \
         or press Ctrl-A then X to quit)",
        image.display()
    );
    // EXPERIMENTAL (spike/12-iommu, docs/spikes/iommu.md): a bare `iommu` argument puts an
    // SMMUv3 in front of the PCIe root complex (`-M …,iommu=smmuv3`) so the IOMMU spike can
    // probe what an unconfigured SMMU does to the existing DMA paths. aarch64 only; consumed
    // by xtask (never reaches the kernel command line); not part of any documented flow.
    let attach_iommu = append.iter().any(|argument| argument == "iommu");
    if attach_iommu && arch != "aarch64" {
        return Err("the experimental `iommu` argument is aarch64-only (SMMUv3)".to_string());
    }
    // A bare `gicv3` argument boots the aarch64 machine with a GICv3 (system-register CPU
    // interface + per-PE redistributor) instead of the default GICv2. The kernel detects
    // the version at boot from GICD_PIDR2 and drives whichever it finds
    // (src/arch/aarch64/gic.rs) — this is the QEMU stand-in for real GICv3 hardware like
    // the RK3588's GIC-600 (docs/board/orange-pi-5-plus.md). Consumed by xtask; never
    // reaches the kernel command line.
    let want_gicv3 = append.iter().any(|argument| argument == "gicv3");
    if want_gicv3 && arch != "aarch64" {
        return Err("the `gicv3` argument is aarch64-only".to_string());
    }
    // A bare `hvf` argument runs the aarch64 machine under Apple's Hypervisor.framework
    // instead of TCG (docs/spikes/spawn-latency.md): native-speed guest execution on an
    // Apple Silicon host. Opt-in only — TCG stays the default (deterministic timing
    // characteristics, works on every host, and the long-verified configuration); HVF
    // requires `-cpu host` (the guest sees the host CPU) and is rejected elsewhere by
    // QEMU itself. Consumed by xtask; never reaches the kernel command line.
    let want_hvf = append.iter().any(|argument| argument == "hvf");
    if want_hvf && arch != "aarch64" {
        return Err(
            "the `hvf` argument is aarch64-only (Apple Silicon Hypervisor.framework)".to_string(),
        );
    }
    let aarch64_machine = format!(
        "virt,gic-version={},highmem=off{}",
        if want_gicv3 { "3" } else { "2" },
        if attach_iommu { ",iommu=smmuv3" } else { "" },
    );
    let machine: &[&str] = match arch {
        // GICv2 stays the pinned default (the long-verified configuration); the bare
        // `gicv3` argument switches to the v3 machine, which the kernel's boot-time
        // PIDR2 detection drives with the system-register CPU interface instead
        // (src/arch/aarch64/gic.rs).
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
            aarch64_machine.as_str(),
            "-cpu",
            if want_hvf { "host" } else { "max" },
            "-device",
            "virtio-rng-pci",
        ],
        // Pin the SiFive-style PLIC (`aia=none`) for the same reason: the kernel's
        // interrupt bring-up (src/arch/riscv64/plic.rs) drives the PLIC, not the newer
        // AIA APLIC/IMSIC. The default CPU and QEMU's bundled OpenSBI `-bios` are used.
        // `virtio-rng-pci` mirrors the aarch64 invocation so the eo9:pci capability has
        // the same baseline to enumerate (host bridge, default NIC, virtio-rng); the
        // riscv64 `virt` ECAM is always at its low address, so no `highmem` pin is needed.
        "riscv64" => &["-M", "virt,aia=none", "-device", "virtio-rng-pci"],
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
    // The bare `display` argument opens QEMU's default framebuffer window (cocoa on
    // macOS, gtk/sdl elsewhere) instead of running headless, with the serial console
    // and monitor multiplexed on stdio so the eosh prompt stays in the terminal. It
    // only makes sense with a display device, so it requires `gpu`.
    let want_display = append.iter().any(|argument| argument == "display");
    if want_display && !append.iter().any(|argument| argument == "gpu") {
        return Err(
            "`display` opens a framebuffer window and needs the virtio-gpu device — \
             add the `gpu` argument (e.g. `cargo xtask qemu aarch64 pci gpu display`)"
                .into(),
        );
    }
    let console: &[&str] = if want_display {
        &["-serial", "mon:stdio"]
    } else {
        &["-nographic"]
    };
    let mut args: Vec<std::ffi::OsString> = machine
        .iter()
        .chain(["-smp", "1", "-m", KERNEL_QEMU_MEMORY].iter())
        .chain(console.iter())
        .chain(["-kernel"].iter())
        .copied()
        .map(Into::into)
        .collect();
    if want_hvf {
        args.insert(0, "-accel".into());
        args.insert(1, "hvf".into());
    }
    args.push(image.as_os_str().to_os_string());
    // The bare `disk` and `net` arguments are xtask's: attach the scratch virtio-blk disk
    // / a user-mode virtio-net NIC and keep the tokens off the kernel command line.
    // `storedisk` is both: it attaches the persistent store-disk image *and* stays on the
    // kernel command line, because the kernel itself acts on the token (it claims that
    // virtio-blk function for its compile cache; see kernel diskcache). Combining
    // `storedisk` with the guest-facing `disk`/`pci` flags in one boot is not supported
    // until machine-global device claiming lands — the kernel claims the first virtio-blk
    // function it finds.
    let mut cmdline: Vec<String> = Vec::new();
    let mut attach_disk = false;
    let mut attach_net = false;
    let mut attach_gpu = false;
    let mut net_dump = false;
    let mut telnet_fwd = false;
    let mut attach_store_disk = false;
    for argument in append {
        if argument == "disk" {
            attach_disk = true;
        } else if argument == "net" {
            attach_net = true;
        } else if argument == "telnet" {
            // Host-forward the guest telnet port over slirp (implies `net`), so the
            // shell-over-network stack is reachable from the host: `nc localhost 5555`.
            attach_net = true;
            telnet_fwd = true;
        } else if argument == "gpu" {
            attach_gpu = true;
        } else if argument == "netdump" {
            // Capture the user-net link to kernel/target/eo9-net.pcap (implies `net`),
            // so link-level evidence — e.g. the virtual-NIC switch's per-port MACs in
            // live exchanges — can be read back with tcpdump.
            attach_net = true;
            net_dump = true;
        } else if argument == "iommu" || argument == "gicv3" {
            // Consumed above (machine-type selection); never reaches the kernel command
            // line.
        } else if argument == "hvf" {
            // Consumed above (accelerator selection); never reaches the kernel command
            // line.
        } else if argument == "display" {
            // Consumed above (console/window selection); never reaches the kernel
            // command line.
        } else if argument == "storedisk" {
            attach_store_disk = true;
            cmdline.push(argument.clone());
        } else {
            cmdline.push(argument.clone());
        }
    }
    if attach_store_disk {
        let store = ensure_store_disk(root)?;
        args.push("-drive".into());
        args.push(
            format!(
                "if=none,format=raw,id=eo9storedisk,file={}",
                store.display()
            )
            .into(),
        );
        args.push("-device".into());
        args.push("virtio-blk-pci,drive=eo9storedisk,disable-legacy=on".into());
    }
    if attach_disk {
        let scratch = ensure_scratch_disk(root)?;
        args.push("-drive".into());
        args.push(format!("if=none,format=raw,id=eo9disk,file={}", scratch.display()).into());
        // The scratch disk gets its own iothread: without one, single-threaded TCG
        // processes the whole request synchronously under the queue-notify write, so a
        // request is never in flight from the guest's point of view — unlike any real
        // device. With it, completions post asynchronously and the interrupt wait
        // genuinely waits, which is what makes `cancelcheck`'s mid-flight window
        // observable under QEMU once the kernel's `pci.wait` suspends the calling task
        // instead of blocking host-side (plan/09 D39).
        args.push("-object".into());
        args.push("iothread,id=eo9diskio".into());
        args.push("-device".into());
        args.push("virtio-blk-pci,drive=eo9disk,iothread=eo9diskio,disable-legacy=on".into());
    }
    if attach_net {
        args.push("-netdev".into());
        if telnet_fwd {
            args.push(
                // Loopback only: the forwarded session is cleartext and unauthenticated,
                // so it must never be reachable from beyond the dev machine.
                format!(
                    "user,id=eo9net,hostfwd=tcp:127.0.0.1:{TELNET_HOST_PORT}-:{TELNET_GUEST_PORT}"
                )
                .into(),
            );
        } else {
            args.push("user,id=eo9net".into());
        }
        if net_dump {
            let pcap = root.join("kernel/target/eo9-net.pcap");
            args.push("-object".into());
            args.push(
                format!(
                    "filter-dump,id=eo9dump,netdev=eo9net,file={}",
                    pcap.display()
                )
                .into(),
            );
        }
        args.push("-device".into());
        args.push("virtio-net-pci,netdev=eo9net,disable-legacy=on".into());
    }
    if attach_gpu {
        // A virtio-gpu function at a pinned 640x480 (so the draw demo's pattern — and
        // `check-gpu`'s expected image — are deterministic), plus a QMP socket so a
        // verification driver can `screendump` the scanout while the machine runs.
        // virtio-gpu is modern-only; there is no disable-legacy property to pin.
        args.push("-device".into());
        args.push(format!("virtio-gpu-pci,xres={GPU_XRES},yres={GPU_YRES}").into());
        args.push("-qmp".into());
        args.push(format!("unix:{},server=on,wait=off", gpu_qmp_socket(root).display()).into());
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
///
/// The eo9-bundled-programs bundle drift check (study 11 D9b) is NOT part of this gate yet:
/// `fs.eofs` depends on `eo9-eofs`, a path dependency outside the guest workspace, and
/// cargo bakes that dependency's absolute manifest path into its `-C metadata` hash — so
/// fs-eofs's bytes still differ per checkout even under `--remap-path-prefix`, and a
/// byte-compare gate would go red in every checkout except the one that last refreshed
/// the bundle. Until the metadata residue is solved (plan/01 D15), the drift check runs
/// only in `cargo xtask package` (and stand-alone via `check-components-bundle`), and the
/// bundle is refreshed from the main checkout by convention.
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
    "eo9-bundled-programs",
    "eo9-eofs",
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
    "eo9-bundled-programs",
    "eo9-eofs",
];

fn components_build_dir(root: &Path) -> PathBuf {
    root.join("guest").join("target").join("components")
}

fn components_data_dir(root: &Path) -> PathBuf {
    root.join("crates")
        .join("eo9-bundled-programs")
        .join("data")
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
/// crates/eo9-bundled-programs/data/ and regenerate its index, so the bundle a published `eo9`
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
        "xtask: refreshed crates/eo9-bundled-programs/data: {} components, {} KiB",
        components.len(),
        total / 1024
    );
    Ok(())
}

/// Verify crates/eo9-bundled-programs/data/ matches the freshly built guest components.
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
            "xtask: eo9-bundled-programs bundle matches the built components ({} components)",
            built.len()
        );
        Ok(())
    } else {
        Err(format!(
            "the eo9-bundled-programs bundle is stale: {}; run `cargo xtask refresh-components` and commit the result",
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
    println!("xtask: dry-run-verified leaf crates:");
    for krate in PUBLISH_LEAF_CRATES {
        // Cargo has moved where dry-run .crate files land across versions: classically
        // target/package/<name>-<version>.crate, currently a tmp-crate/ (and sometimes
        // tmp-registry/) subdirectory. Probe the known locations and report which one
        // held the file — and if none did, say so instead of printing "0 KiB" (study 11
        // D10: a swallowed lookup failure made every crate report 0 KiB).
        let package_dir = root.join("target").join("package");
        let file_name = format!("{krate}-{PUBLISH_VERSION}.crate");
        let candidates = [
            package_dir.join(&file_name),
            package_dir.join("tmp-crate").join(&file_name),
            package_dir.join("tmp-registry").join(&file_name),
        ];
        match candidates
            .iter()
            .find_map(|path| std::fs::metadata(path).ok().map(|meta| (path, meta.len())))
        {
            Some((path, size)) => println!(
                "xtask:   {file_name}  {} KiB  ({})",
                size.div_ceil(1024),
                path.strip_prefix(root).unwrap_or(path).display()
            ),
            None => println!(
                "xtask:   {file_name}  size unknown — no .crate file found under {} \
                 (cargo's dry-run output layout may have changed again)",
                package_dir
                    .strip_prefix(root)
                    .unwrap_or(&package_dir)
                    .display()
            ),
        }
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

// ----------------------------------------------------------------------------------------
// check-gpu: headless verification of the display stack (wit/gfx, gpu.virtio, draw).
//
// Boots the aarch64 kernel under QEMU with the `pci` grant and a virtio-gpu pinned at
// GPU_XRES x GPU_YRES, drives the serial eosh prompt like a (paced) human, and after each
// draw run issues a QMP `screendump` and compares the PPM pixel-for-pixel against the
// expected pattern — computed here, independently of the guest (the third verbatim copy
// of the pattern; see guest/examples/draw/src/lib.rs).
// ----------------------------------------------------------------------------------------

/// The geometry the `gpu` flag pins the virtio-gpu to (small enough that the on-target
/// composition draws in a moment; the pattern scales to whatever the mode reports).
const GPU_XRES: u32 = 640;
const GPU_YRES: u32 = 480;

/// How long to wait for the eosh prompt / a draw outcome before declaring the boot hung.
/// On-target compilation of `gpu.virtio $ draw` dominates (tens of seconds under TCG).
const GPU_STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn gpu_qmp_socket(root: &Path) -> PathBuf {
    root.join("kernel").join("target").join("eo9-gpu-qmp.sock")
}

/// The deterministic draw test pattern — a verbatim copy of guest/examples/draw/src/lib.rs
/// (and of tests/eo9-integration/tests/gfx.rs); see the demo's module docs. Keep in
/// lockstep: a drifted copy fails the image comparison this module exists to make.
mod gfx_pattern {
    pub fn pattern_pixel(frame: u32, width: u32, height: u32, x: u32, y: u32) -> (u8, u8, u8) {
        let base = base_pixel(width, height, x, y);
        if frame >= 2 && in_damage_rect(width, height, x, y) {
            return (255 - base.0, 255 - base.1, 255 - base.2);
        }
        base
    }

    fn damage_rect(width: u32, height: u32) -> (u32, u32, u32, u32) {
        (width / 4, height / 4, width / 2, height / 2)
    }

    fn in_damage_rect(width: u32, height: u32, x: u32, y: u32) -> bool {
        let (dx, dy, dw, dh) = damage_rect(width, height);
        x >= dx && x < dx + dw && y >= dy && y < dy + dh
    }

    fn base_pixel(width: u32, height: u32, x: u32, y: u32) -> (u8, u8, u8) {
        if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
            return (255, 255, 255);
        }
        let qw = width / 4;
        let qh = height / 4;
        let in_quarter = |x0: u32, y0: u32| x >= x0 && x < x0 + qw && y >= y0 && y < y0 + qh;
        if in_quarter(width / 8, height / 8) {
            return (255, 32, 32);
        }
        if in_quarter(3 * width / 8, 3 * height / 8) {
            return (32, 255, 32);
        }
        if in_quarter(5 * width / 8, 5 * height / 8) {
            return (32, 32, 255);
        }
        (
            ((x as u64 * 255) / u64::from(width - 1).max(1)) as u8,
            ((y as u64 * 255) / u64::from(height - 1).max(1)) as u8,
            ((x ^ y) & 0xff) as u8,
        )
    }
}

/// Boot, draw (one frame, then two frames), screendump after each, compare both.
fn check_gpu(root: &Path) -> Result<(), String> {
    use std::io::Write as _;
    use std::sync::mpsc;

    let arch = "aarch64";
    let image = build_kernel(root, arch)?;
    let qmp_path = gpu_qmp_socket(root);
    let _ = std::fs::remove_file(&qmp_path);
    let dump1 = root
        .join("kernel")
        .join("target")
        .join("eo9-gpu-frame1.ppm");
    let dump2 = root
        .join("kernel")
        .join("target")
        .join("eo9-gpu-frame2.ppm");
    let _ = std::fs::remove_file(&dump1);
    let _ = std::fs::remove_file(&dump2);

    println!(
        "xtask: check-gpu — booting {} with a {GPU_XRES}x{GPU_YRES} virtio-gpu, driving \
         `gpu.virtio $ draw` at the eosh prompt, screendumping over QMP",
        image.display()
    );

    // The same invocation as `qemu aarch64 pci gpu`, with stdio piped for scripting.
    let mut command = Command::new(format!("qemu-system-{arch}"));
    command
        .current_dir(root)
        .args(["-M", "virt,gic-version=2,highmem=off", "-cpu", "max"])
        .args(["-device", "virtio-rng-pci"])
        .args(["-smp", "1", "-m", KERNEL_QEMU_MEMORY, "-nographic"])
        .arg("-kernel")
        .arg(&image)
        .args(["-append", "pci"])
        .arg("-device")
        .arg(format!("virtio-gpu-pci,xres={GPU_XRES},yres={GPU_YRES}"))
        .arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_path.display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    let mut child = command
        .spawn()
        .map_err(|err| format!("check-gpu: failed to spawn qemu-system-{arch}: {err}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    // Reader thread: forward serial bytes over a channel so waits can time out.
    let (sender, receiver) = mpsc::channel::<u8>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stdout = stdout;
        let mut byte = [0u8; 1];
        while let Ok(n) = stdout.read(&mut byte) {
            if n == 0 || sender.send(byte[0]).is_err() {
                break;
            }
        }
    });

    /// Accumulate serial output until `marker` appears (echoing it through), or time out.
    fn wait_for(receiver: &mpsc::Receiver<u8>, marker: &str, what: &str) -> Result<String, String> {
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + GPU_STEP_TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    format!(
                        "check-gpu: timed out waiting for {what} (last output: …{})",
                        &seen[seen.len().saturating_sub(400)..]
                    )
                })?;
            match receiver.recv_timeout(remaining) {
                Ok(byte) => {
                    seen.push(byte as char);
                    if seen.contains(marker) {
                        return Ok(seen);
                    }
                }
                Err(_) => {
                    return Err(format!(
                        "check-gpu: the serial stream ended or timed out waiting for {what} \
                         (last output: …{})",
                        &seen[seen.len().saturating_sub(400)..]
                    ));
                }
            }
        }
    }

    /// Type a line the way a human would: one byte at a time, slowly. The metal console
    /// drops bytes from fast input (plan/12 D49), and under host CPU contention even
    /// chunked pastes lose characters — so go at genuinely human speed and let the
    /// caller verify the echo before trusting the line went in.
    fn type_line(stdin: &mut std::process::ChildStdin, line: &str) -> Result<(), String> {
        for byte in line.as_bytes() {
            stdin
                .write_all(core::slice::from_ref(byte))
                .map_err(|err| format!("check-gpu: writing to the console: {err}"))?;
            stdin
                .flush()
                .map_err(|err| format!("check-gpu: flushing the console: {err}"))?;
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        stdin
            .write_all(b"\n")
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("check-gpu: writing to the console: {err}"))
    }

    // Boot to the prompt, run the one-frame draw, dump; run the two-frame draw, dump.
    // Any failure kills QEMU before returning (no orphaned VMs holding the pipe open).
    let drive = (|| -> Result<(), String> {
        wait_for(&receiver, "eosh>", "the eosh prompt")?;
        type_line(&mut stdin, "gpu.virtio $ draw")?;
        wait_for(&receiver, "gpu.virtio $ draw", "the one-frame command echo")?;
        let output = wait_for(&receiver, "presented(", "the one-frame draw outcome")?;
        if !output.contains("ok:") {
            return Err(String::from(
                "check-gpu: the one-frame draw did not report ok (see the serial output above)",
            ));
        }
        wait_for(&receiver, "eosh>", "the prompt after the one-frame draw")?;
        qmp_screendump(&qmp_path, &dump1)?;

        type_line(&mut stdin, "gpu.virtio $ draw --frames 2")?;
        wait_for(&receiver, "--frames 2", "the two-frame command echo")?;
        wait_for(&receiver, "presented(", "the two-frame draw outcome")?;
        wait_for(&receiver, "eosh>", "the prompt after the two-frame draw")?;
        qmp_screendump(&qmp_path, &dump2)?;

        type_line(&mut stdin, "exit")?;
        Ok(())
    })();
    if let Err(err) = drive {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    let _ = child.wait();

    compare_ppm(&dump1, 1)?;
    compare_ppm(&dump2, 2)?;
    println!(
        "xtask: check-gpu ok — both screendumps match the expected pattern pixel-for-pixel \
         ({GPU_XRES}x{GPU_YRES}, frame 1 and the frame-2 partial-damage composite)"
    );
    Ok(())
}

/// Issue a QMP `screendump` and wait for its completion response.
fn qmp_screendump(socket: &Path, output: &Path) -> Result<(), String> {
    use std::io::{Read as _, Write as _};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|err| format!("check-gpu: connecting to the QMP socket: {err}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|err| format!("check-gpu: QMP socket timeout: {err}"))?;

    /// Read until `needle` appears in the accumulated response (QMP is line-oriented
    /// JSON; matching the substring is enough for this two-command conversation).
    fn read_until(
        stream: &mut std::os::unix::net::UnixStream,
        needle: &str,
    ) -> Result<String, String> {
        let mut seen = String::new();
        let mut buf = [0u8; 512];
        loop {
            let n = stream
                .read(&mut buf)
                .map_err(|err| format!("check-gpu: reading QMP: {err}"))?;
            if n == 0 {
                return Err(format!("check-gpu: QMP closed early (saw: {seen})"));
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            if seen.contains(needle) {
                return Ok(seen);
            }
            if seen.contains("\"error\"") {
                return Err(format!("check-gpu: QMP reported an error: {seen}"));
            }
        }
    }

    // Greeting → capabilities → screendump → completion.
    read_until(&mut stream, "QMP")?;
    stream
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .map_err(|err| format!("check-gpu: writing QMP: {err}"))?;
    read_until(&mut stream, "\"return\"")?;
    let dump = format!(
        "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":{:?}}}}}\n",
        output.display().to_string()
    );
    stream
        .write_all(dump.as_bytes())
        .map_err(|err| format!("check-gpu: writing QMP: {err}"))?;
    read_until(&mut stream, "\"return\"")?;
    Ok(())
}

/// Compare a QEMU `screendump` PPM against the expected pattern for `frame`.
fn compare_ppm(path: &Path, frame: u32) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|err| {
        format!(
            "check-gpu: reading the screendump {}: {err}",
            path.display()
        )
    })?;

    // P6 header: magic, width, height, maxval — whitespace/comment separated, then one
    // single whitespace byte before the binary RGB triplets.
    let mut cursor = 0usize;
    let mut fields: Vec<u64> = Vec::new();
    if !bytes.starts_with(b"P6") {
        return Err(format!("check-gpu: {} is not a P6 PPM", path.display()));
    }
    cursor += 2;
    while fields.len() < 3 {
        // Skip whitespace and comments.
        while cursor < bytes.len() {
            match bytes[cursor] {
                b' ' | b'\t' | b'\r' | b'\n' => cursor += 1,
                b'#' => {
                    while cursor < bytes.len() && bytes[cursor] != b'\n' {
                        cursor += 1;
                    }
                }
                _ => break,
            }
        }
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if start == cursor {
            return Err(format!(
                "check-gpu: malformed PPM header in {}",
                path.display()
            ));
        }
        let field = std::str::from_utf8(&bytes[start..cursor])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("check-gpu: malformed PPM header in {}", path.display()))?;
        fields.push(field);
    }
    cursor += 1; // the single whitespace after maxval
    let (width, height, maxval) = (fields[0] as u32, fields[1] as u32, fields[2]);
    if (width, height) != (GPU_XRES, GPU_YRES) {
        return Err(format!(
            "check-gpu: {} is {width}x{height}, expected {GPU_XRES}x{GPU_YRES} (is the \
             virtio-gpu xres/yres pin in place?)",
            path.display()
        ));
    }
    if maxval != 255 {
        return Err(format!(
            "check-gpu: {} has maxval {maxval}, expected 255",
            path.display()
        ));
    }
    let pixels = &bytes[cursor..];
    let needed = width as usize * height as usize * 3;
    if pixels.len() < needed {
        return Err(format!(
            "check-gpu: {} is truncated ({} of {needed} pixel bytes)",
            path.display(),
            pixels.len()
        ));
    }

    // Pixel-for-pixel, exact: the pattern is integer-deterministic, the format is
    // xrgb8888 → RGB888 with no scaling or blending anywhere in the path, so any
    // tolerance would only hide bugs (the recorded comparison-tolerance decision).
    /// First mismatching pixel: position, expected RGB, actual RGB.
    type Mismatch = (u32, u32, (u8, u8, u8), (u8, u8, u8));
    let mut mismatches = 0usize;
    let mut first: Option<Mismatch> = None;
    for y in 0..height {
        for x in 0..width {
            let expected = gfx_pattern::pattern_pixel(frame, width, height, x, y);
            let at = ((y * width + x) * 3) as usize;
            let actual = (pixels[at], pixels[at + 1], pixels[at + 2]);
            if actual != expected {
                mismatches += 1;
                if first.is_none() {
                    first = Some((x, y, expected, actual));
                }
            }
        }
    }
    if mismatches > 0 {
        let (x, y, expected, actual) = first.unwrap();
        return Err(format!(
            "check-gpu: frame {frame} screendump {} differs from the expected pattern at \
             {mismatches} pixel(s); first at ({x},{y}): expected {expected:?}, got {actual:?}",
            path.display()
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------------------------
// check-telnet: headless end-to-end verification of the shell-over-network stack
// (net.text + telnetd, plan/09 D44, plan/10 entry 20). Boots with a user-mode NIC plus a
// slirp host-forward, drives `telnetd --sessions 2` at the serial eosh prompt, then acts
// as the network client from the host side: greeting + prompt, `hello`, a concurrent
// second connection refused, `exit` closes cleanly, and a second sequential session is
// served independently. The sessions are cleartext telnet — the stack under test is a
// trusted-LAN/dev tool by design (SSH deferred; see plan/09 D44).
// ----------------------------------------------------------------------------------------

/// The host port the `telnet` qemu flag forwards (slirp hostfwd) to the guest.
const TELNET_HOST_PORT: u16 = 5555;
/// The guest port telnetd listens on (net.text's documented default).
const TELNET_GUEST_PORT: u16 = 23;
/// Serial-step timeout. The dominant cost is the one-time on-target compile of the fused
/// four-component session (`net.virtio $ net.l4.over-l2 $ net.text $ eosh`) under TCG —
/// comparable compositions have taken minutes (plan/09 D42's 200 s six-component fusion).
const TELNET_STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// How long one host connect attempt may wait for the session prompt before retrying.
const TELNET_PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long the host client keeps retrying its connection (covers the between-sessions
/// gap: respawn, NIC re-claim, listen bring-up).
const TELNET_CONNECT_RETRY: std::time::Duration = std::time::Duration::from_secs(180);

/// Boot, serve two telnet sessions, validate both plus the concurrent refusal.
fn check_telnet(root: &Path) -> Result<(), String> {
    use std::io::Write as _;
    use std::net::TcpStream;
    use std::sync::mpsc;

    let arch = "aarch64";
    let image = build_kernel(root, arch)?;

    println!(
        "xtask: check-telnet — booting {} with a user-mode NIC \
         (hostfwd tcp:127.0.0.1:{TELNET_HOST_PORT}-:{TELNET_GUEST_PORT}), driving \
         `telnetd --sessions 2` at the eosh prompt, then connecting from the host",
        image.display()
    );

    // The same invocation as `qemu aarch64 pci net telnet`, with stdio piped for scripting.
    let mut command = Command::new(format!("qemu-system-{arch}"));
    command
        .current_dir(root)
        .args(["-M", "virt,gic-version=2,highmem=off", "-cpu", "max"])
        .args(["-device", "virtio-rng-pci"])
        .args(["-smp", "1", "-m", KERNEL_QEMU_MEMORY, "-nographic"])
        .arg("-kernel")
        .arg(&image)
        .args(["-append", "pci"])
        .arg("-netdev")
        .arg(format!(
            "user,id=eo9net,hostfwd=tcp:127.0.0.1:{TELNET_HOST_PORT}-:{TELNET_GUEST_PORT}"
        ))
        .args(["-device", "virtio-net-pci,netdev=eo9net,disable-legacy=on"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    let mut child = command
        .spawn()
        .map_err(|err| format!("check-telnet: failed to spawn qemu-system-{arch}: {err}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    // Reader thread: forward serial bytes over a channel so waits can time out.
    let (sender, receiver) = mpsc::channel::<u8>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stdout = stdout;
        let mut byte = [0u8; 1];
        while let Ok(n) = stdout.read(&mut byte) {
            if n == 0 || sender.send(byte[0]).is_err() {
                break;
            }
        }
    });

    /// Accumulate serial output until `marker` appears, or time out.
    fn wait_for(receiver: &mpsc::Receiver<u8>, marker: &str, what: &str) -> Result<String, String> {
        let mut seen = String::new();
        let deadline = std::time::Instant::now() + TELNET_STEP_TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    format!(
                        "check-telnet: timed out waiting for {what} (last serial output: …{})",
                        &seen[seen.len().saturating_sub(400)..]
                    )
                })?;
            match receiver.recv_timeout(remaining) {
                Ok(byte) => {
                    seen.push(byte as char);
                    if seen.contains(marker) {
                        return Ok(seen);
                    }
                }
                Err(_) => {
                    return Err(format!(
                        "check-telnet: the serial stream ended or timed out waiting for {what} \
                         (last serial output: …{})",
                        &seen[seen.len().saturating_sub(400)..]
                    ));
                }
            }
        }
    }

    /// Type a line at the serial console the way a human would (plan/12 D49: the metal
    /// console drops bytes from fast input).
    fn type_line(stdin: &mut std::process::ChildStdin, line: &str) -> Result<(), String> {
        for byte in line.as_bytes() {
            stdin
                .write_all(core::slice::from_ref(byte))
                .map_err(|err| format!("check-telnet: writing to the console: {err}"))?;
            stdin
                .flush()
                .map_err(|err| format!("check-telnet: flushing the console: {err}"))?;
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        stdin
            .write_all(b"\n")
            .and_then(|()| stdin.flush())
            .map_err(|err| format!("check-telnet: writing to the console: {err}"))
    }

    /// Read from the socket until `marker` appears, or the per-step deadline passes.
    fn read_until(
        stream: &mut TcpStream,
        marker: &str,
        deadline: std::time::Duration,
        what: &str,
    ) -> Result<String, String> {
        use std::io::Read as _;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        let start = std::time::Instant::now();
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while start.elapsed() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => {
                    return Err(format!(
                        "check-telnet: the connection closed waiting for {what} (saw: {seen:?})"
                    ));
                }
                Ok(n) => {
                    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if seen.contains(marker) {
                        return Ok(seen);
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => {
                    return Err(format!(
                        "check-telnet: reading the socket waiting for {what}: {err} (saw: {seen:?})"
                    ));
                }
            }
        }
        Err(format!(
            "check-telnet: timed out waiting for {what} on the socket (saw: {seen:?})"
        ))
    }

    /// Read until the peer closes (EOF or reset both count — slirp surfaces a guest-side
    /// close either way); everything seen comes back.
    fn read_to_eof(
        stream: &mut TcpStream,
        deadline: std::time::Duration,
        what: &str,
    ) -> Result<String, String> {
        use std::io::Read as _;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        let start = std::time::Instant::now();
        let mut seen = String::new();
        let mut buf = [0u8; 1024];
        while start.elapsed() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => return Ok(seen),
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    return Ok(seen);
                }
                Err(err) => {
                    return Err(format!(
                        "check-telnet: reading the socket waiting for {what}: {err}"
                    ));
                }
            }
        }
        Err(format!(
            "check-telnet: timed out waiting for {what} (the peer never closed; saw: {seen:?})"
        ))
    }

    /// Connect to the forwarded port and wait for a session prompt, retrying the whole
    /// connection while the guest stack is still coming up (or between sessions, when it
    /// is briefly down — slirp accepts the host side either way, so a connection that
    /// stalls or dies without a prompt is dropped and retried).
    fn connect_session(what: &str) -> Result<(TcpStream, String), String> {
        let start = std::time::Instant::now();
        let mut last = String::from("no connection attempt completed");
        while start.elapsed() < TELNET_CONNECT_RETRY {
            match TcpStream::connect(("127.0.0.1", TELNET_HOST_PORT)) {
                Ok(mut stream) => {
                    match read_until(&mut stream, "eosh> ", TELNET_PROMPT_TIMEOUT, what) {
                        Ok(transcript) => return Ok((stream, transcript)),
                        Err(err) => {
                            last = err;
                            drop(stream);
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                }
                Err(err) => {
                    last = format!("connect: {err}");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        Err(format!(
            "check-telnet: could not reach a session prompt for {what} within \
             {TELNET_CONNECT_RETRY:?} (last attempt: {last})"
        ))
    }

    // Drive the whole conversation; any failure kills QEMU before returning.
    let drive = (|| -> Result<(), String> {
        wait_for(&receiver, "eosh>", "the eosh prompt")?;
        type_line(&mut stdin, "telnetd --sessions 2")?;
        wait_for(&receiver, "telnetd --sessions 2", "the command echo")?;
        // The one-time fused-session compile happens here (the long pole under TCG).
        wait_for(
            &receiver,
            "session 1: waiting for a connection",
            "telnetd to start serving",
        )?;

        // ----- session 1 -------------------------------------------------------------
        let (mut s1, greeting) = connect_session("session 1")?;
        if !greeting.contains("cleartext telnet session") {
            return Err(format!(
                "check-telnet: the session greeting (security banner) is missing: {greeting:?}"
            ));
        }
        if !greeting.contains("eosh") {
            return Err(format!(
                "check-telnet: the eosh banner is missing from the session: {greeting:?}"
            ));
        }
        println!("\n----- check-telnet: session 1 transcript (connect) -----\n{greeting}");

        s1.write_all(b"hello\r\n")
            .map_err(|err| format!("check-telnet: writing `hello` to the socket: {err}"))?;
        let hello_out = read_until(
            &mut s1,
            "eosh> ",
            TELNET_STEP_TIMEOUT,
            "the prompt after `hello`",
        )?;
        if !hello_out.contains("ok: greeted") {
            return Err(format!(
                "check-telnet: `hello` over the socket did not report `ok: greeted`: {hello_out:?}"
            ));
        }
        println!("----- check-telnet: session 1 transcript (hello) -----\n{hello_out}");
        // The child's own stdout lands on the machine console (the recorded per-task
        // text gap, plan/10 entry 20) — verify it arrived there.
        wait_for(
            &receiver,
            "Hello, world.",
            "hello's stdout on the serial console",
        )?;

        // ----- a concurrent second connection is refused ------------------------------
        // net.text dropped its listener after accepting, so the transport answers the
        // SYN with a RST and slirp closes the host side. Nothing of a session may appear.
        let mut s2 = TcpStream::connect(("127.0.0.1", TELNET_HOST_PORT))
            .map_err(|err| format!("check-telnet: second connect (refusal probe): {err}"))?;
        let refused = read_to_eof(
            &mut s2,
            std::time::Duration::from_secs(20),
            "the concurrent connection to be refused",
        )?;
        if refused.contains("eosh> ") {
            return Err(format!(
                "check-telnet: a concurrent second session was served (sessions must be \
                 sequential): {refused:?}"
            ));
        }
        println!(
            "----- check-telnet: concurrent connection refused (closed by the guest; \
             {} bytes seen) -----",
            refused.len()
        );

        // ----- `exit` closes the connection -------------------------------------------
        s1.write_all(b"exit\r\n")
            .map_err(|err| format!("check-telnet: writing `exit` to the socket: {err}"))?;
        let close_out = read_to_eof(
            &mut s1,
            std::time::Duration::from_secs(30),
            "the connection to close after `exit`",
        )?;
        if !close_out.contains("session closed") {
            return Err(format!(
                "check-telnet: the goodbye line is missing after `exit`: {close_out:?}"
            ));
        }
        println!("----- check-telnet: session 1 transcript (exit) -----\n{close_out}");
        wait_for(
            &receiver,
            "session 1 ended",
            "telnetd's session-1 narration",
        )?;

        // ----- session 2: sequential independence -------------------------------------
        wait_for(
            &receiver,
            "session 2: waiting for a connection",
            "telnetd to serve session 2",
        )?;
        let (mut s3, greeting2) = connect_session("session 2")?;
        if !greeting2.contains("cleartext telnet session") {
            return Err(format!(
                "check-telnet: session 2's greeting is missing: {greeting2:?}"
            ));
        }
        println!("----- check-telnet: session 2 transcript (connect) -----\n{greeting2}");
        s3.write_all(b"exit\r\n")
            .map_err(|err| format!("check-telnet: writing `exit` to session 2: {err}"))?;
        let close2 = read_to_eof(
            &mut s3,
            std::time::Duration::from_secs(30),
            "session 2 to close after `exit`",
        )?;
        if !close2.contains("session closed") {
            return Err(format!(
                "check-telnet: session 2's goodbye line is missing: {close2:?}"
            ));
        }
        println!("----- check-telnet: session 2 transcript (exit) -----\n{close2}");

        // ----- telnetd finishes; the machine winds down --------------------------------
        wait_for(
            &receiver,
            "served 2 session(s); exiting",
            "telnetd to finish",
        )?;
        wait_for(&receiver, "served(2)", "telnetd's outcome at the console")?;
        wait_for(&receiver, "eosh>", "the console prompt after telnetd")?;
        type_line(&mut stdin, "exit")?;
        Ok(())
    })();
    if let Err(err) = drive {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    let _ = child.wait();

    println!(
        "xtask: check-telnet ok — two sequential sessions served over \
         localhost:{TELNET_HOST_PORT} (greeting + eosh banner + prompt, `hello` → \
         `ok: greeted`, concurrent connection refused, `exit` closed both cleanly)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// firstpoll-ab: the A/B gate for the vendored first-poll-inline feature
// ---------------------------------------------------------------------------

/// `cargo xtask firstpoll-ab [--rounds N] [--gate-only]` — the standing regression gate
/// for the vendored `component-model-async-first-poll` feature and for any future change
/// to the vendored async machinery (docs/spikes/first-poll-inline.md).
///
/// Phase 1, the gate: build and run the standalone A/B workspace (tests/firstpoll-ab) in
/// both arms. The suites themselves encode the semantic-identity contract — the
/// async-hardening matrix and the real-chain suites are included by `#[path]` and must
/// pass with identical outcomes in both arms, while the eager-guest pins are arm-specific
/// (`eager_guest_off.rs` pins today's queued semantics, `eager_guest_on.rs` pins exactly
/// the three intended wall-row flips). Both arms green = semantic identity PASS.
///
/// Phase 2, timing (skipped by `--gate-only`): run the bench measurements `--rounds`
/// times per arm, interleaved A/B/A/B so slow drift in host load lands on both arms
/// alike, and report per-shape medians with min..max spread plus the host load context
/// (`uptime`) sampled around the rounds. Cargo caches both arms' artifacts side by side
/// (the feature set is part of the unit hash), so only the first round pays a build.
fn firstpoll_ab(root: &Path, rounds: u32, gate_only: bool) -> Result<(), String> {
    use std::collections::BTreeMap;

    let dir = root.join("tests").join("firstpoll-ab");
    // The chain suites compose the real guest stubs; refresh the artifacts first
    // (running against stale components has bitten before — see plan/01 Decisions).
    build_guest(root)?;

    println!(
        "xtask: firstpoll-ab — arm A (first-poll-inline OFF): the hardening matrix, the \
         eager-guest pins, the real-chain suites"
    );
    run(&dir, "cargo", ["test", "--release"])?;
    println!(
        "xtask: firstpoll-ab — arm B (first-poll-inline ON): the same suites; the three \
         wall rows asserted RETURNED"
    );
    run(
        &dir,
        "cargo",
        ["test", "--release", "--features", "first-poll-inline"],
    )?;
    println!(
        "xtask: firstpoll-ab: semantic identity PASS — both arms green (matrix and chain \
         suites identical, eager-guest pins per arm)"
    );
    if gate_only {
        return Ok(());
    }

    // Timing rounds. Keyed by (shape, arm-on); each bench invocation contributes one
    // per-iteration sample per shape (the bench's own loop already amortizes spawn+run
    // over its iterations).
    let load_before = host_load();
    let mut samples: BTreeMap<(String, bool), Vec<f64>> = BTreeMap::new();
    for round in 1..=rounds {
        for arm_on in [false, true] {
            let mut args = vec!["test", "--release", "--test", "bench"];
            if arm_on {
                args.extend(["--features", "first-poll-inline"]);
            }
            args.extend(["--", "--ignored", "--nocapture", "--test-threads=1"]);
            let output = run_capture(&dir, "cargo", &args)?;
            let parsed = parse_bench_lines(&output);
            if parsed.is_empty() {
                return Err(format!(
                    "firstpoll-ab: the bench run printed no parsable timing lines \
                     (round {round}, feature {}); raw output:\n{output}",
                    if arm_on { "on" } else { "off" }
                ));
            }
            for (shape, nanoseconds) in parsed {
                samples
                    .entry((shape, arm_on))
                    .or_default()
                    .push(nanoseconds);
            }
        }
        println!("xtask: firstpoll-ab: timing round {round}/{rounds} done");
    }
    let load_after = host_load();

    // The table: per shape, median (min..max) per arm and the median-to-median delta.
    let shapes: Vec<String> = samples
        .keys()
        .map(|(shape, _)| shape.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    println!();
    println!(
        "xtask: firstpoll-ab timing — {rounds} interleaved round(s) per arm, per-iteration \
         medians (min..max):"
    );
    println!("  load before: {load_before}");
    println!("  load after:  {load_after}");
    println!();
    println!(
        "  {:<28} | {:>26} | {:>26} | {:>7}",
        "shape", "off: median (min..max)", "on: median (min..max)", "delta"
    );
    for shape in &shapes {
        let off = samples.get(&(shape.clone(), false));
        let on = samples.get(&(shape.clone(), true));
        let (Some(off), Some(on)) = (off, on) else {
            println!("  {shape:<28} | (missing one arm — bench output incomplete)");
            continue;
        };
        let (off_median, off_min, off_max) = stats(off);
        let (on_median, on_min, on_max) = stats(on);
        let delta = (on_median - off_median) / off_median * 100.0;
        println!(
            "  {:<28} | {:>26} | {:>26} | {:>+6.1}%",
            shape,
            format!(
                "{} ({}..{})",
                format_nanoseconds(off_median),
                format_nanoseconds(off_min),
                format_nanoseconds(off_max)
            ),
            format!(
                "{} ({}..{})",
                format_nanoseconds(on_median),
                format_nanoseconds(on_min),
                format_nanoseconds(on_max)
            ),
            delta,
        );
    }
    println!();
    println!(
        "xtask: firstpoll-ab done — gate PASS, timing above (negative delta = the inline \
         arm is faster; treat single-digit deltas as noise unless the spreads separate)"
    );
    Ok(())
}

/// Like [`run`], but capture stdout for parsing while stderr (cargo's build progress)
/// streams through to the terminal. Fails on a non-zero exit status with the captured
/// stdout included, so test failures stay readable.
fn run_capture<I, S>(dir: &Path, program: &str, args: I) -> Result<String, String>
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
    let output = Command::new(program)
        .args(&args)
        .current_dir(dir)
        .env_remove("RUSTUP_TOOLCHAIN")
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|err| format!("failed to run `{program} {shown}`: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "`{program} {shown}` failed with {} in {}; stdout:\n{stdout}",
            output.status,
            dir.display()
        ))
    }
}

/// Extract `(shape, per-iteration nanoseconds)` pairs from the bench output lines, which
/// look like `[first-poll-inline OFF] eager chain depth 1: 200 runs in 26.4ms
/// (132.27µs/run)` (the per-iteration figure is the parenthesized one). Under
/// `--nocapture` the libtest harness prints `test name ... ` onto the same line before
/// the bench's own println, so the marker is matched anywhere in the line, not at the
/// start.
fn parse_bench_lines(output: &str) -> Vec<(String, f64)> {
    let mut parsed = Vec::new();
    for line in output.lines() {
        let Some(at) = line.find("[first-poll-inline") else {
            continue;
        };
        let rest = &line[at + "[first-poll-inline".len()..];
        let Some((_arm, rest)) = rest.split_once("] ") else {
            continue;
        };
        let Some((shape, rest)) = rest.split_once(':') else {
            continue;
        };
        let Some(open) = rest.rfind('(') else {
            continue;
        };
        let Some(slash) = rest[open + 1..].find('/') else {
            continue;
        };
        if let Some(nanoseconds) = parse_duration_nanoseconds(&rest[open + 1..open + 1 + slash]) {
            parsed.push((shape.trim().to_string(), nanoseconds));
        }
    }
    parsed
}

/// Parse a `Duration`'s `Debug` rendering ("980ns", "132.27µs", "1.35ms", "4.2s") into
/// nanoseconds. Suffix order matters: "ns"/"µs"/"ms" before the bare "s".
fn parse_duration_nanoseconds(text: &str) -> Option<f64> {
    let text = text.trim();
    for (suffix, scale) in [
        ("ns", 1.0),
        ("µs", 1e3),
        ("us", 1e3),
        ("ms", 1e6),
        ("s", 1e9),
    ] {
        if let Some(number) = text.strip_suffix(suffix) {
            return number.trim().parse::<f64>().ok().map(|value| value * scale);
        }
    }
    None
}

/// Median, min, max of a non-empty sample set (nanoseconds).
fn stats(samples: &[f64]) -> (f64, f64, f64) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = if sorted.len() % 2 == 1 {
        sorted[sorted.len() / 2]
    } else {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    };
    (median, sorted[0], sorted[sorted.len() - 1])
}

/// Render nanoseconds in the most readable unit (one decimal).
fn format_nanoseconds(nanoseconds: f64) -> String {
    if nanoseconds >= 1e9 {
        format!("{:.2}s", nanoseconds / 1e9)
    } else if nanoseconds >= 1e6 {
        format!("{:.2}ms", nanoseconds / 1e6)
    } else if nanoseconds >= 1e3 {
        format!("{:.1}µs", nanoseconds / 1e3)
    } else {
        format!("{nanoseconds:.0}ns")
    }
}

/// The host load context for measurement output: `uptime`'s one-liner (load averages),
/// best-effort — measurements on a shared machine are only as honest as their caveats.
fn host_load() -> String {
    Command::new("uptime")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "(load unavailable)".to_string())
}
