# 01 — Workspace, toolchain, CI

## Scope
Repo scaffolding so every other area can start: workspaces, toolchain pins, build orchestration, lint/format,
CI skeleton. No product code.

## Deliverables
- Three build roots:
  - Host workspace at repo root (`crates/*`, `xtask`).
  - `guest/` workspace building wasm components (via `cargo component` or `wit-bindgen` + `wasm-tools
    component new` — pick one, document it, wire it into xtask). Targets `wasm32-wasip2` (or
    `wasm32-unknown-unknown` + adapter if the SDK goes no_std — see plan 07).
  - `kernel/` workspace (no_std, custom targets).
- `rust-toolchain.toml` per workspace — a pinned nightly is fine (and preferred) everywhere; pinned
  `wasmtime` / `wasm-tools` versions in a single workspace-level `[workspace.dependencies]` table so every
  crate uses the same versions.
- `xtask` (or `justfile`): `build`, `test`, `build-guest`, `build-kernel <arch>`, `qemu <arch>`, `fmt`, `lint`,
  and `ci` (the gate reviewer agents run before merging).
- CI is local-only for now: `xtask ci` = fmt + clippy + host tests + guest build (+ QEMU smoke once plan 12
  lands). No hosted CI.
- `.gitignore`, LICENSE placeholder, rustfmt/clippy config (default + `-D warnings`).

## Dependencies
None. Everything else depends on this.

## Milestones
1. Workspaces exist, `xtask build && xtask test` green on an empty skeleton (one placeholder crate each).
2. CI runs the same commands.

## Notes / constraints
- Keep the dependency tree minimal (PLAN.md ground rule 3); `xtask` over heavyweight build systems.
- Decide and document the component-building flow once; plans 07/09/10 follow it.

## Decisions

1. **Toolchain pin.** All three workspaces pin `nightly-2026-05-25` (rustc 1.98.0-nightly, 423e3d252) via a
   `rust-toolchain.toml` at each workspace root (repo root, `guest/`, `kernel/`). rustup resolves the
   toolchain per directory, so each file lists only the targets that workspace needs (guest:
   `wasm32-unknown-unknown`; kernel: `aarch64-unknown-none`, `riscv64gc-unknown-none-elf`,
   `x86_64-unknown-none`); keep the `channel` line identical across the three files when bumping.
2. **Dependency pins.** The authoritative table is `[workspace.dependencies]` in the repo-root `Cargo.toml`:
   `wasmtime 45.0.0`; wasm-tools family (`wit-parser`, `wit-component`, `wasm-encoder`, `wasmparser`,
   `wasm-wave`) `0.250.0` (matches the installed wasm-tools CLI 1.250.0); `wac-graph 0.10.0` (matches wac-cli
   0.10.0); `wit-bindgen 0.57.1` (matches wit-bindgen-cli 0.57.1); `blake3 1.8.5`. Known, accepted skew:
   wasmtime 45 internally uses the 0.248 family and wac-graph 0.10 the 0.247 family — those stay private to
   them. `guest/Cargo.toml` mirrors the `wit-bindgen` pin (keep in sync with the root table). Lockfiles are
   committed for all three workspaces.
3. **Guest component build flow: wit-bindgen + wasm-tools, not cargo-component.** Guest crates use the
   `wit_bindgen::generate!` proc macro, build as `cdylib` for `wasm32-unknown-unknown`, and
   `xtask build-guest` componentizes each one with `wasm-tools component new` (then `wasm-tools validate`)
   into `guest/target/components/<package>.wasm` (release profile). Rationale: no second build front-end or
   extra package metadata; no WASI/adapter in the import set, which matches plan 07's
   no_std-and-only-`eo9:*`-imports direction; the componentize step stays explicit and scriptable.
   `guest/.cargo/config.toml` defaults builds in that workspace to the wasm target. New component crates get
   added to the `GUEST_COMPONENTS` list in `xtask/src/main.rs`.
4. **CI gate.** `cargo run -p xtask -- ci` (alias: `cargo xtask ci`) = `fmt --check` + `clippy -D warnings` +
   build + test + `build-guest`, across all three workspaces. `-D warnings` is enforced by xtask's clippy
   invocation (not hard-coded into the sources) so local iteration isn't blocked by warnings; rustfmt uses
   default style via a single `rustfmt.toml` at the repo root (covers all workspaces). CI is local-only per
   the planner decision.
5. **Kernel scaffolding.** The placeholder kernel crate is `#![cfg_attr(not(test), no_std)]`; xtask builds and
   clippy-checks the kernel workspace for `aarch64-unknown-none` (to keep it honestly no_std) and runs its
   unit tests on the host triple. `build-kernel <arch>` and `qemu <arch>` validate the arch
   (aarch64/riscv64/x86_64) and then fail with an explicit "not implemented yet (area 12)" error.
6. **Workspace layout.** Three Cargo workspaces: host at the repo root (`crates/*` + `xtask`, with
   `exclude = ["guest", "kernel"]`), `guest/`, `kernel/`; edition 2024, resolver 3, `publish = false`
   everywhere. Placeholder crates `eo9-placeholder`, `eo9-guest-placeholder`, `eo9-kernel-placeholder` are
   replaced as their areas land.
7. **xtask is dependency-free** (std-only argument parsing and `std::process`). Child cargo invocations drop
   `RUSTUP_TOOLCHAIN` so each workspace's own `rust-toolchain.toml` governs, and `cargo test` is not run in
   the guest workspace (no wasm test runner; guest code is exercised by host-side integration tests).
8. **License: MIT** (owner decision; the repo `LICENSE` file is the MIT text). The guest and www workspace
   manifests carry `license = "MIT"`; the root host workspace manifest does not yet — add it when touching
   the root manifest next.
9. **Guest components are refreshed before tests run.** Host integration tests consume the prebuilt
   components under `guest/target/components` and only rebuild *missing* ones, so running tests against a
   stale tree after a guest source change produced false failures (it bit a reviewer on master). Both the
   `ci` gate and the standalone `test` subcommand now run `build-guest` before the test step; the gate order
   is fmt → lint → build → build-guest → test. The no-change overhead is ~1.5–2 s (an incremental no-op
   guest build plus re-componentize/validate of every component); a content-hash freshness check was
   deliberately not added — predictable ordering over cleverness.
10. **Top-level Makefile as the human entry point.** A fresh machine failed `cargo xtask build-guest`
    because the `wasm-tools` CLI was not installed and the spawn error was opaque; the README also assumed
    host tools that were never listed. The repo now carries a plain POSIX `Makefile` with `setup` / `shell` /
    `www` / `www-build` / `qemu` / `ci` targets — thin wrappers over `cargo xtask` that check for the tools
    they need (`wasm-tools`, `qemu-system-aarch64`) and point at `make setup` instead of dying with a raw
    spawn error. `make setup` is idempotent: it adds the wasm32 target on the root-pinned toolchain (guest/
    and kernel/ declare their own targets in their `rust-toolchain.toml`), installs `wasm-tools` if missing,
    and prints a prerequisite summary. `make www` serves the committed site directly (the demo assets are
    committed and reproducible); `make www-build` is the rebuild-from-source variant. README gained a
    "Quick start" table + a Prerequisites note. A `cargo xtask doctor` subcommand and friendlier xtask
    spawn errors remain a queued follow-up (xtask was owned by other work when this landed).
11. **`cargo xtask doctor` + friendly missing-tool errors** (the D10 follow-up). `doctor` checks the host
    prerequisites — rustup, the pinned nightly (informational; rustup auto-installs it), the
    `wasm32-unknown-unknown` target on the root pin, the `aarch64-unknown-none` target (informational;
    kernel/rust-toolchain.toml auto-adds it), the `wasm-tools` CLI (warning, not failing, when its version is
    outside the pinned 1.250.x family), plus optional `qemu-system-aarch64` and node ≥ 25 for the /vm
    harnesses — and exits non-zero with a per-item install hint when a required tool is missing. `make setup`
    ends by printing the doctor summary. Every external tool xtask spawns now maps the could-not-spawn
    (ENOENT) case to "`<tool>` not found — run `make setup` (or `cargo xtask doctor`)" instead of a raw
    `os error 2`, closing the opaque-failure path from D10.
