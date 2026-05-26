# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design).

_Last updated: 2026-05-26, master at 0dc0fb5. The latest wave is fully merged: eo9:pci, shell tab-completion
+ capability-aware `env`, configure for async-API providers, kernel milestone 3 (component-model-async on
no_std; async guests on bare metal), the xtask test-ordering fix, and the eo9.org `/try` in-browser demo.
No area branches are currently in flight; the next wave is pending three owner decisions (see Next up)._

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
- **Bare metal (aarch64/QEMU):** `cargo xtask build-kernel aarch64 && cargo xtask qemu aarch64` boots Eo9 on
  the `virt` machine — MMU on, kernel root providers (PL011 text, PL031/generic-timer time, seeded entropy),
  the real `eo9-example-hello` ending in `outcome = success(greeted)`, **plus async guests**: a component
  that suspends across a 50 ms sleep against the kernel timer, and the unmodified `entropy.seeded` stub
  configured through its async-lifted `configure`. Component-model-async runs on the no_std kernel via a
  minimal vendored wasmtime patch (15 files, ~329 lines, kernel-workspace-only; see GAPS for upstreaming).
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
| Website + server + /try in-browser demo | `www/` | complete, deployable; /try v2 (eosh in browser) pending |
| Bare-metal kernel (aarch64: boot, heap, timer, MMU, kernel providers, sync + async guests on metal, vendored CM-async no_std patch) | `kernel/` | milestones 1–3 merged; boot-to-eosh and on-target codegen next; riscv64/x86_64 not started |

## In progress right now

- Nothing on area branches; all of the last wave is merged. Next dispatches are queued behind the owner
  decisions below.

## Next up (rough order)

1. Kernel ladder: boot-to-eosh on metal — read-only store image + cmdline program selection, GIC (stop
   busy-polling), UART RX, fuel on metal, eo9-sched adoption — then the wasmtime-environ/cranelift no_std
   port for **on-target codegen (required for MVP; Pulley only as a stopgap)**; then riscv64/x86_64 and the
   QEMU test tier.
2. Owner decisions pending: (a) configure for resource-owning providers — grow the binder (resource proxying)
   vs a runtime-assisted configuration path vs park; (b) /try v2 — the real eosh REPL in the browser
   (JS exec host + HTTP-backed store) — go/no-go; (c) whether/who to offer the wasmtime no_std CM-async
   patch upstream.
3. Demo packaging: ship prebuilt components with the published crate so `cargo install eo9; eo9` works
   without a checkout.
4. Bundle milestone: `eo9-embed` library + `eo9 bundle` (native executables for other OSes).
5. eo9:pci follow-ups: `pci.deny`/`pci.filtered` stubs (area 09); a kernel/QEMU virtio-over-PCI provider as
   the first real consumer; dma-buffer ↔ `eo9:io` buffer story.
6. Exec follow-ups: guest-facing `resume`/fuel donation (E5); net provider linking, `net.loopback`,
   Message API; eofs milestone 2+ (provider, mkfs, store-on-eofs, content hashes).
7. Housekeeping: push to origin, crates.io name, Message/perf/threads API design.

See `GAPS.md` for known limitations and deferred decisions.
