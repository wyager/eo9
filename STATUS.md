# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design).

_Last updated: 2026-05-26, master at 7821fbf. Headline: **the bare-metal MVP is complete — at the eosh prompt
on bare metal you can compose an arbitrary capability expression and the machine fuses it with the real
algebra, compiles it on-target with Cranelift, and runs it** (`entropy.seeded $ cruncher … → ok:
digest(…)`). This run delivered the whole bare-metal track — boot, MMU, kernel providers, sync + async
guests, baked-in store, boot-to-interactive-eosh, on-target Cranelift codegen, and now interactive
composition — plus, in usermode, eo9:pci, shell tab-completion + capability-aware `env`, the interactive
prompt fix, configure for async-API providers, the `eo9-embed` embeddable-runtime library, the xtask
test-ordering fix, the eo9.org `/try` in-browser demo, and the wasm32+Pulley embed spike. What remains on
metal is hardening, not capability (GIC, fuel, sched, the other arches). /try v2 deferred; nothing pushed._

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
- Invoker-side provider configuration via the algebra now covers providers with freestanding sync **or
  async** APIs: `configure(time.frozen, …) $ configure(entropy.seeded, seed=…) $ program` — the fully
  invoker-configured deterministic environment — works end to end and is byte-identical across runs
  (resource-owning providers like fs.memfs still configure-by-composition only; see GAPS).
- `eo9 shell`: interactive eosh REPL with tab completion (builtins, session-resolvable names, `eo9:*`
  interface refs, paths under the granted `--fs-root` only) and a capability-aware `env`: the session's
  grants, what children receive, and `env <program>` marking each import satisfied / always-available /
  optional-absent / would-be-refused. Exec is granted to the shell only; provider flags bind `configure`.
- The out-of-box demo flow: bare `eo9` boots to the shell and, on an empty store, seeds ~22 components
  embedded in the binary (hello, the stubs, eosh itself); `eo9 <name-or-path> [--flags]` is an implicit run.
- **Bare metal (aarch64/QEMU) — boots to an interactive shell:** `cargo xtask build-kernel aarch64 &&
  cargo xtask qemu aarch64` boots Eo9 on the `virt` machine straight into an **interactive eosh prompt over
  serial**. The unmodified eosh runs against kernel-side fs (a read-only `/bin` view of a baked-in 7-program
  store image), exec, and root providers; a user can `hello --name metal --excited true`, `cruncher`,
  `outcomes --mode fail`, `env`, `describe`, and `exit` (clean PSCI power-off). Children receive text/time/
  entropy only — never fs or exec (an fs-needing program is refused at instantiation). MMU on; PL011 text,
  PL031/generic-timer time, seeded entropy. Async works: a guest suspends across a 50 ms sleep against the
  kernel timer; the unmodified `entropy.seeded` stub runs through its async-lifted `configure`. CM-async runs
  on the no_std kernel via a minimal vendored wasmtime patch (15 files, ~329 lines, kernel-workspace-only).
  Headless modes: `cargo xtask qemu aarch64 demo` (the m1–m3 sequence) and `program=<name> [k=v …]` both
  self-power-off; the no-argument boot is interactive and does not self-terminate.
- **On-target codegen + interactive composition on bare metal:** with the `wasm-codegen` feature, the kernel
  compiles WebAssembly to native aarch64 *on the machine* with Cranelift (`Component::new`, code published
  through real I-cache/D-cache maintenance), and the interactive eosh exposes the full algebra: at the prompt
  `entropy.seeded $ cruncher --seed 9 --rounds 200000` is fused by the real `eo9_component::compose`,
  compiled on-target, and run → `ok: digest(14341732361190694547)` (no baked artifact exists for the fused
  result — it is genuinely composed and compiled on the machine). `$`/`&`/`only`/`configure` all work; plain
  baked programs keep the AOT fast path. Achieved by vendoring + de-std'ing the wasmtime/cranelift compile
  layers and the component-algebra closure (wit-parser, wac-types, wac-graph, wit-component, wasm-wave) under
  kernel/vendor, and making `eo9-component` no_std in place behind a default-on `std` feature (usermode
  byte-identical) — all provenance-reviewed: no codegen/algebra/encoder logic changed. Feature off by default
  so CI stays lean; kernel ELF ~23 MB with it on (incl. debug info). Remaining is hardening: GIC (executor
  still polls), child fuel, eo9-sched, a determinism bit-compare, friendlier child missing-fs errors.
- The eo9.org website (`www/`): static site + logo + standalone Rust server with built-in ACME TLS, and the
  `/try` page — real example components (hello, outcomes, cruncher, readwrite incl. async/JSPI) transpiled
  at build time and run client-side in the visitor's browser, with a live grant/revoke capability demo.
  Honest labeling: it is a launcher, not eosh (the in-browser eosh REPL is the planned v2).
- `cargo xtask ci` — one gate over the host, guest, and kernel workspaces; guest components are rebuilt
  before the test step (stale-component hazard closed).

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages incl. `eo9:pci`, capability conventions, async ops) | `wit/` | v0 complete; message/perf are placeholders |
| Component algebra: `$`, `&`, `only`, `rename`, `configure`, describe/load/save | `crates/eo9-component` | complete incl. law tests; configure covers sync+async APIs, not resource-owning providers (GAPS) |
| Runtime: fuel-metered resumable tasks, WAVE args/outcomes, caps, fs/io + text/time/entropy linking, exec provider, image serialization | `crates/eo9-runtime` | usermode-complete for current scope |
| Scheduler (no_std, conserved fuel, deterministic policy) | `crates/eo9-sched` | complete for single-core; not yet adopted by the CLI loop or kernel |
| Module store + compile cache (content-addressed, hash-keyed) | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk, clone-first open-exec) | `crates/eo9-providers-unix` | complete; net deferred |
| eofs core (CoW/Merkle, lz4-by-default, snapshots, crash-consistency) | `crates/eofs-core` | engine complete; provider/mkfs not started |
| Guest SDK + 18 stub providers (none family incl. pci-none, seeded, memfs, frozen/fuzzy clocks, deny/readonly, …) | `guest/` | complete for current WIT; pci.deny/filtered, loopback, capture deferred |
| eosh (full grammar, evaluator, env/envinfo, component) | `guest/eosh` | done for current scope; runs as the `eo9 shell` |
| Integration suites (capability laws, determinism, invoker-configured env, kill/linearity, CLI transcripts) | `tests/eo9-integration` + `crates/eo9/tests` | 30+ tests; QEMU tier not started |
| Usermode binary `eo9` | `crates/eo9` | run/store/describe/compile/cache/shell/demo-seeding done |
| Embeddable runtime (`Eo9` builder, Sandbox + Host backends behind a `ProviderSource` seam, safe capability defaults) | `crates/eo9-embed` | complete; foundation for `eo9 bundle` and the deferred wasm32 backend; sandbox-only builds with `--no-default-features` |
| Website + server + /try in-browser demo | `www/` | complete, deployable; /try v2 (eosh in browser) pending |
| Bare-metal kernel (aarch64: boot, heap, timer, MMU, kernel providers, sync + async guests, baked-in store, boot-to-interactive-eosh, on-target Cranelift codegen, **interactive composition via the real algebra**, vendored CM-async + compile-layer + algebra no_std forks) | `kernel/` | bare-metal MVP complete; remaining is hardening only — GIC/fuel/eo9-sched, determinism bit-compare, friendlier child errors, then riscv64/x86_64 + QEMU test tier |

## In progress right now

- Nothing on area branches; all of the last run is merged. The bare-metal MVP is complete; remaining work is
  hardening and breadth, queued below.

## Next up (rough order)

1. Kernel hardening toward "more than a spike": GIC + interrupts (stop the busy-poll, WFI idle), child fuel +
   eo9-sched adoption, io/buffers + fs/types wiring for children (friendly missing-fs story), W^X for code
   pages, a determinism bit-compare of on-target codegen; then riscv64/x86_64 ports and the QEMU test tier.
2. Demo packaging: ship prebuilt components with the published crate so `cargo install eo9; eo9` works
   without a checkout.
3. Bundle milestone: `eo9 bundle` (native executables for other OSes) on top of `eo9-embed`.
4. eo9:pci follow-ups: `pci.deny`/`pci.filtered` stubs (area 09); a kernel/QEMU virtio-over-PCI provider as
   the first real consumer; dma-buffer ↔ `eo9:io` buffer story.
5. Exec follow-ups: guest-facing `resume`/fuel donation (E5); net provider linking, `net.loopback`,
   Message API; eofs milestone 2+ (provider, mkfs, store-on-eofs, content hashes).
6. Housekeeping: push to origin, crates.io name, Message/perf/threads API design.

_Settled (see GAPS): /try v2 (the wasm32 real-stack browser blob) is deferred — v1 already demos async
components in the browser, and the blob is month-plus + not MVP-critical; on-target codegen forks cranelift
now rather than waiting for upstream; upstreaming anything is deferred until a compelling MVP._

See `GAPS.md` for known limitations and deferred decisions.
