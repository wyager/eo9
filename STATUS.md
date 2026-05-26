# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design).

_Last updated: 2026-05-26, master at the kernel-spike merge (27bb1af); one finished branch
(`area/12-async-on-metal`, the real hello example on bare metal) is awaiting review/merge._

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
- The out-of-box demo flow: bare `eo9` boots to the shell and, on an empty store, seeds ~22 components
  embedded in the binary (hello, the stubs, eosh itself); `eo9 <name-or-path> [--flags]` is an implicit run.
- **Bare metal (aarch64/QEMU):** `cargo xtask build-kernel aarch64 && cargo xtask qemu aarch64` boots Eo9 on
  the `virt` machine — serial banner, heap, timer, and a host-AOT'd wasm component executing on the metal.
  (The follow-up branch running the real `hello` example against kernel-side text/time/entropy providers is
  finished and awaiting merge.)
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
| Bare-metal kernel (aarch64 spike: boot, heap, timer, MMU, wasm-on-metal, xtask build/qemu) | `kernel/` | spike merged; real-example branch pending merge; riscv64/x86_64 not started |

Also working now: **`eo9 shell`** — an interactive eosh REPL (and `-c` one-shot mode) with the exec
capability granted to the shell only, store-backed bare-name resolution via a session bin view, provider
flags binding `configure` at compose time, and children receiving exactly the session's root providers.
(Caveat: invoker-side `configure` currently works only for providers with sync, freestanding APIs — in
practice `entropy.seeded`/`perf.null`; see GAPS.md.)

## In progress right now

- **Kernel milestone 2** (`area/12-async-on-metal`): finished on its branch — the unmodified `hello` example
  runs on bare metal against kernel text/time/entropy providers — review/merge pending (delayed only by
  agent-infrastructure stalls, not by the work).

## Next up (rough order)

1. Bare metal, owner-approved ladder: CM-async no_std support (upstream-friendly) → boot-to-shell on metal
   with host-AOT → the wasmtime-environ/cranelift no_std port for **on-target codegen (required for MVP;
   Pulley only as a stopgap)**; then riscv64/x86_64 ports and the QEMU test tier.
2. Shell UX: tab-completion over session names/builtins; richer `env` (show granted capabilities, what
   children receive, a program's absent/optional imports).
3. Configure-binder extension so async/resource-owning providers (time.frozen, fs.memfs, …) can be
   invoker-configured — unlocks the fully invoker-configured deterministic environment.
4. Demo packaging: ship prebuilt components with the published crate so `cargo install eo9; eo9` works
   without a checkout; xtask build-guest-before-test ordering fix.
5. Bundle milestone: `eo9-embed` library + `eo9 bundle` (native executables for other OSes).
6. Exec follow-ups: guest-facing `resume`/fuel donation (E5); net provider linking, `net.loopback`,
   Message API; eofs milestone 2+ (provider, mkfs, store-on-eofs, content hashes).
7. Housekeeping: push to origin, crates.io name, Message/perf/threads API design.

See `GAPS.md` for known limitations and deferred decisions.
