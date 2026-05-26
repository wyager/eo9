# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design).

_Last updated: 2026-05-25, master at the configure-binder-fix merge (a9e3669)._

## Works today (usermode, on master, CI-gated)

- `eo9 run <name-or-path> [--flags]` — real components end to end: WAVE-typed flags checked against the
  program's signature, three-way outcomes (`success`/`failure`/`abnormal`) with exit codes 0/1/2/3,
  store-resolved dotted names or host paths, immutable `open-exec` (APFS clonefile, refuse-by-default on
  non-COW), memory limits, compile cache whose hits launch from the cached image with zero codegen.
- Filesystem access is opt-in: `--fs-root <dir>` grants a rooted fs capability; without it, fs-requiring
  programs are refused with a clear message and fs-optional programs observe absence.
- `eo9 store add|ls|gc`, `eo9 describe`, `eo9 compile`; store + cache under `~/.eo9/store`.
- Example programs: `hello`, `outcomes`, `cruncher`, `readwrite` (fs round-trip).
- Deterministic execution proven on real components: `time.frozen $ entropy.seeded $ fs.memfs $ program`
  (and the `&` form) runs byte-identically and is sealed against ambient providers (integration suite).
- Invoker-side provider configuration: `configure(entropy.seeded, seed=…)` via the algebra works end to end
  (program observes the seed, no program-side setup).
- The eo9.org website (`www/`): static site + logo + standalone Rust server with built-in ACME TLS.
- `cargo xtask ci` — one gate over the host, guest, and kernel workspaces.

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages, capability conventions, async ops) | `wit/` | v0 complete; message/perf are placeholders |
| Component algebra: `$`, `&`, `only`, `rename`, `configure`, describe/load/save | `crates/eo9-component` | complete incl. law tests; configure limited (see GAPS) |
| Runtime: fuel-metered resumable tasks, WAVE args/outcomes, caps, fs/io + text/time/entropy linking, exec provider, image serialization | `crates/eo9-runtime` | usermode-complete for current scope |
| Scheduler (no_std, conserved fuel, deterministic policy) | `crates/eo9-sched` | complete for single-core; not yet adopted by the CLI loop |
| Module store + compile cache (content-addressed, hash-keyed) | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk, clone-first open-exec) | `crates/eo9-providers-unix` | complete; net deferred |
| eofs core (CoW/Merkle, lz4-by-default, snapshots, crash-consistency) | `crates/eofs-core` | engine complete; provider/mkfs not started |
| Guest SDK + 17 stub providers (none family, seeded, memfs, frozen/fuzzy clocks, deny/readonly, …) | `guest/` | complete for current WIT; loopback/capture deferred |
| eosh (full grammar, evaluator, component) | `guest/eosh` | parser/evaluator done; execution wiring in progress |
| Integration suites (capability laws, determinism, kill/linearity, CLI transcripts) | `tests/eo9-integration` | 30 tests; QEMU tier not started |
| Usermode binary `eo9` | `crates/eo9` | run/store/describe/compile/cache done; `shell` in progress |
| Website + server | `www/` | complete, deployable |

Also working now: **`eo9 shell`** — an interactive eosh REPL (and `-c` one-shot mode) with the exec
capability granted to the shell only, store-backed bare-name resolution via a session bin view, provider
flags binding `configure` at compose time, and children receiving exactly the session's root providers.
(Caveat: invoker-side `configure` currently works only for providers with sync, freestanding APIs — in
practice `entropy.seeded`/`perf.null`; see GAPS.md.)

## In progress right now

- **Bare-metal kernel spike** (area 12): aarch64 boot + serial + heap on QEMU, wasm-on-target feasibility.
- **Demo polish** (area 11): bare `eo9` → shell, `eo9 <file>` → implicit run, embedded-component store
  seeding (the `cargo install eo9; eo9` experience).

## Next up (rough order)

1. Demo milestone: store seeding from components embedded in the `eo9` binary, CLI defaults (bare `eo9` →
   shell, path → implicit run), a few demo tools. Then `cargo install eo9; eo9` works.
2. Bundle milestone: `eo9-embed` library + `eo9 bundle` (native executables for other OSes).
3. Exec follow-ups: guest-facing `resume`/fuel donation (E5), configure for resource-owning providers.
4. Net: unix net provider linking, `net.loopback`, Message API (unblocks `text.capture`, pipes).
5. eofs milestone 2+: the `eo9:fs` provider component, `eofs.mkfs`, store-on-eofs, content-hash queries.
6. Bare metal (area 12): not started — kernel spike, QEMU images, boot-to-shell; plus the QEMU test tier.
7. Housekeeping: push to origin, crates.io name, Message/perf/threads API design.

See `GAPS.md` for known limitations and deferred decisions.
