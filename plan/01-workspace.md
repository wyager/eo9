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
(record here)
