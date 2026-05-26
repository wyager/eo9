# Eo9 Implementation Plan (master)

This is the master plan for building the Eo9 MVP described in `SPEC.md`. Work is split into sub-areas, each
with its own plan file under `plan/`. Sub-areas are designed to be worked on in parallel by independent
agents, with the planner acting as mediator and integrator.

**Read order for any sub-agent:** `SPEC.md` (all of it) → this file → your area's `plan/NN-*.md`.

## Ground rules for sub-agents

1. `SPEC.md` is the source of truth. The three guiding principles and "no hacks or shortcuts" are sacred;
   every other design choice is provisional — but **changing the design is the planner's call**: if your area
   hits a sticking point that wants a spec change, stop and escalate with a concrete proposal. Do not edit
   `SPEC.md` yourself.
2. Stay inside your area's crates/directories. Cross-area contracts (WIT packages, shared traits) change only
   via the planner.
3. Keep dependencies minimal. Approved foundation deps: `wasmtime`, the `wasm-tools` family (`wit-parser`,
   `wit-component`, `wasm-encoder`, `wasm-compose`/`wac-graph`), `wit-bindgen`, `wasm-wave`, `blake3`.
   No tokio, no heavy frameworks. Anything else: ask.
4. Everything gets tests. Algebraic laws from the spec become property tests (see `plan/13-tests.md`).
5. Record decisions you make inside your area in a `## Decisions` section at the bottom of your plan file.
6. Small, reviewable commits with plain descriptive messages.

## Repository layout (target)

```
eo9/
  SPEC.md  PLAN.md  plan/
  wit/                  # eo9:* WIT packages — the interface source of truth      (02)
  crates/               # host-side Rust workspace
    eo9-component/      # component algebra: load/describe/$/&/only/rename        (03)
    eo9-runtime/        # wasmtime embedding; compile/task/async host side        (04)
    eo9-sched/          # no_std scheduler shared with bare metal                 (05)
    eo9-store/          # content-addressed store + compile cache                 (06)
    eo9-providers-unix/ # root providers backed by the host OS                    (08)
    eo9/                # the usermode `eo9` binary (CLI)                         (11)
  guest/                # guest-side workspace (wasm components)
    eo9-guest/          # guest SDK: bindings, async shim, outcome helpers        (07)
    stubs/              # standard stub providers (fs.memfs, time.frozen, …)      (09)
    eosh/               # the shell, itself an Eo9 program                        (10)
    examples/           # hello, cruncher, …                                      (07)
  kernel/               # bare-metal workspace: core + arch ports + QEMU scripts  (12)
  tests/                # cross-area integration tests (usermode + QEMU)          (13)
  xtask/ or justfile    # build orchestration across the three workspaces         (01)
```

## Sub-areas

| #  | Area                | Plan file                   | Primary output                         | Depends on |
|----|---------------------|-----------------------------|----------------------------------------|------------|
| 01 | Workspace & CI      | plan/01-workspace.md        | repo scaffolding, toolchains, CI       | —          |
| 02 | WIT interfaces      | plan/02-wit.md              | `wit/` packages                        | 01         |
| 03 | Component algebra   | plan/03-component-algebra.md| `eo9-component`                        | 01, 02     |
| 04 | Runtime             | plan/04-runtime.md          | `eo9-runtime`                          | 01, 02, 03, 05 |
| 05 | Scheduler           | plan/05-scheduler.md        | `eo9-sched` (no_std)                   | 01         |
| 06 | Store & cache       | plan/06-store.md            | `eo9-store`                            | 01, (03)   |
| 07 | Guest SDK & examples| plan/07-guest-sdk.md        | `guest/eo9-guest`, `guest/examples`    | 01, 02     |
| 08 | Unix root providers | plan/08-providers-unix.md   | `eo9-providers-unix`                   | 01, 02, 04 |
| 09 | Stub providers      | plan/09-providers-stubs.md  | `guest/stubs/*`                        | 02, 07     |
| 10 | eosh                | plan/10-eosh.md             | `guest/eosh`                           | 02, 07     |
| 11 | Usermode binary     | plan/11-usermode.md         | `crates/eo9`                           | 03–08      |
| 12 | Bare metal / QEMU   | plan/12-kernel.md           | `kernel/`, bootable images             | 04, 05, 06 |
| 13 | Test suite          | plan/13-tests.md            | `tests/`, CI gates                     | all        |
| 14 | Native fs (eofs)   | plan/14-eofs.md            | `crates/eofs-core`, guest provider    | 01, 02, 07 |

## Phases

- **Phase 0 (serial):** 01 workspace, then 02 WIT v0. Everything else keys off these.
- **Phase 1 (parallel):** 03 component algebra, 04 runtime, 05 scheduler, 06 store, 07 guest SDK,
  08 unix providers. These only touch `wit/` read-only and their own crates.
- **Phase 2 (parallel):** 09 stubs, 10 eosh, 11 usermode integration, 13 tests build-out, 14 eofs core
  (format + library; its provider component and kernel adoption land alongside Phase 3).
- **Phase 3:** 12 bare metal (one arch first, then the other two), QEMU test suite.

## Integration milestones

- **I0** — workspaces build, CI green, `wit/` validates with wasm-tools.
- **I1** — `eo9 run examples/hello` works end to end in usermode: compile via Wasmtime, typed args in,
  typed outcome out, text/time/entropy served by unix root providers.
- **I2** — the algebra is real: `$`, `&`, `only`, `rename`/`with` work from eosh running under usermode eo9;
  standard stubs exist; the deterministic environment (`fs.memfs & time.frozen & entropy.seeded`) produces
  bit-identical runs; store + compile cache hit on second launch.
- **I3** — concurrency at scale in usermode: a program with thousands of in-flight disk/net ops; fuel
  accounting and `resume`-based user-level scheduling demonstrated; kill/linearity behavior tested.
- **I4** — bare metal: first arch boots in QEMU, runs a headless program, output over serial (on-target
  codegen lands as the immediately following kernel milestone).
- **I5** — all three arches boot to eosh; usermode + QEMU test suites green in CI.
- **Demo** (usermode showcase; slots in after I2, independent of bare metal) — `cargo install eo9; eo9` drops the
  user into a usermode Eo9 "VM": bare `eo9` boots to eosh against the host-OS-backed root providers with a
  store seeded from components bundled inside the binary (eosh, the standard stubs, the examples, and a few
  small demo tools), and `eo9 <file> [args…]` runs an arbitrary component from the host filesystem (implicit
  `run`). Pieces: runtime exec-to-guests, eosh store-backed resolution, CLI defaults (bare → shell,
  path → run), store seeding from embedded components, demo tools.

## Review & merge workflow

- Area agents work on a branch or worktree scoped to their area and keep commits small.
- When an area milestone is ready, a **reviewer agent** (not the planner) reviews the diff against SPEC.md and
  the area plan, runs `xtask ci`, requests fixes, and merges to master.
- The planner stays out of diff-level work: it receives short summaries and gets involved only for escalated
  design questions, changes to `wit/` or other cross-area contracts, and integration milestones.

## Decisions (planner ↔ user)

1. Git workflow: spec + plans live on master; area agents on branches/worktrees; reviewer agents review and
   merge (see Review & merge workflow above).
2. Bare-metal execution: **on-target codegen is part of the MVP.** The kernel is no_std **+ alloc** (heap from
   day one), which is what makes carrying Cranelift feasible. Host-side AOT/cross-compilation is kept only as
   a dev convenience and bootstrap seed; interpreter-on-metal is a fallback/diagnostic path, not the strategy.
3. QEMU arch order: aarch64 → riscv64 → x86_64.
4. eosh is compiled from Rust to a wasm component; it is not an OS builtin.
5. Nightly Rust is fine anywhere it is useful.
6. CI is local-only for now (`xtask ci`); no hosted CI.
