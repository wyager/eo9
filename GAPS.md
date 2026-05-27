# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-26 (master at 0dc0fb5, after the pci / shell-ux / configure-async / kernel-m3 /
xtask-order / web-try wave)._

## Decisions pending with the owner

- **User-study findings (2026-05-27)** — full triage in `docs/user-studies/00-synthesis.md`; every item is
  dispositioned fix-now / tracked / owner-decision. Open owner decisions from it: (1) unconfigured-provider
  semantics (defaults vs refuse-before-run vs hybrid — the "used before configure" trap); (2) package-level
  `only eo9:text` shorthand vs requiring full interface refs; (3) a `describe` wiring/layer view for
  auditing interposed attenuators; (4) roadmap ordering — metal preemption/fuel next, and real-board
  bring-up vs riscv64/x86_64; (5) outcome-line placement (planner default: move to stderr + flag) and
  entropy-opt-in for children if wanted. Tracked items added below; fix-now items are being dispatched.
- **Compose-time vs run-time provider parameters.** Changing a seed currently changes the composed artifact
  and forces a recompile, same as changing a structural choice. Owner parked the "late-bound parameter"
  idea until there is a clean design; revisit if deterministic sweeps start thrashing the compile cache.

## Settled directions (recorded so they're not re-litigated)

- **/try v2 (wasm32 real-stack browser blob): deferred** (owner ruling 2026-05-26). /try v1 already runs real
  async components in the browser via jco + JSPI, so the demo exists; the real-stack blob would cost month-
  plus (the infeasible-drop-in-backend → fiberless-callback-surgery problem above) and is not MVP-critical.
  Keep v1; revisit the fiberless work — or upstream it — after the MVP. The wasm32 findings live in
  plan/15 D15–20; `eo9-embed` (area 16) remains the shared foundation for whenever it's picked back up.
- **No upstreaming until a compelling MVP** (owner ruling 2026-05-26). All ten vendored forks stay in-tree
  under kernel/vendor until then. Per-family feasibility reports are written (`docs/upstreaming/*.md`,
  2026-05-27): the CM-async no_std relaxation is the highest-value first PR (~8–14 days, upstream has an
  explicit no_std program); the environ/cranelift compile-layer no_std port is genuinely novel (upstream's
  push covers the code generator, not the compile drivers); wit-parser/wasm-wave are tiny completions of
  already-merged upstream no_std work; wit-component should go via a "make wasm-metadata no_std" design
  issue; wac needs an appetite-check issue first. Free prep recorded in the reports (keep vendor README
  current — it's missing the algebra-crate section — and rebase rather than re-derive on version bumps).
- **On-target codegen: fork cranelift now** (owner ruling 2026-05-26), do not wait for upstream's in-flight
  no_std work to finish. Vendor/fork the compile layers (cranelift-codegen and the wasm→CLIF + emission /
  loader path) into a no_std+alloc state under kernel/vendor; build on upstream's no_std PRs where they
  help, but don't block on them. In progress on area/12-codegen. (plan/12)

## Design decisions deliberately parked

- **Configure for resource-owning providers** (fs.memfs, disk.mem, net-style): owner ruling (2026-05-26) —
  parked until there are concrete consumers (likely the net/disk provider work) so the strategy can be
  evaluated against real needs. Options on the table: grow the binder (export-side resource proxying +
  variant-shaped `task.return` reloading, plan/03 D13) vs a runtime-assisted configuration path. fs.memfs's
  `configure` takes no args, so the deterministic environment loses nothing today.
- **Content-only vs layout-dependent eofs node hashes.** eofs `stat` hashes are Merkle roots over the
  physical layout; the spec's fs hash queries may want content-only identity (format v2 field). Decide when
  eofs milestone 2 specifies the `eo9:fs` hash surface. (plan/14 Decisions 4)
- **Component-typed arguments** (`interpret (…)`): spec says components cross boundaries as bytes; the
  concrete convention is undesigned. Revisit when something consumes it. (plan/10 Decisions 6b)
- **dma-buffer ↔ `eo9:io` buffer relationship** (eo9:pci): a real driver will want to hand DMA'd contents up
  as io buffers without a copy; needs a conversion or a unified buffer story when the first real PCI
  provider exists. (plan/02 Decision 14)
- **Exec-copy cleanup / Santa alert noise / crates.io name** — operational niceties, owner-facing.

## Functional gaps (implementation exists, coverage incomplete)

### Configure / algebra
- **`configure` rejects resource-owning providers** (fs.memfs, disk.mem, net-style) with a clean,
  regression-tested error; freestanding sync and async APIs (entropy.seeded, perf.null, time.frozen,
  time.monotonic-stub, time.fuzzy) are configurable. (plan/03 D13; decision pending above)
- Binder caveats: it depends on wasmtime 45's CM async ABI encodings (packed subtask status + callback/event
  codes — isolated in one constants block in configure.rs); the suspended-subtask path is implemented per the
  ABI but not yet exercised end-to-end (no configurable provider blocks today); cancelling an in-flight
  forwarded call traps; async functions with >4 flat params or variant/record/tuple/flags results are
  rejected with clear errors; the >16-flattened-param indirect-args case is rejected, not supported.

### Runtime / providers (usermode)
- **Guest-facing `resume` unsupported (E5):** children are fuel-sliced out of the parent's own donation, so a
  guest scheduler cannot direct CPU itself and long-running children throttle the shell. Unblock: upstream
  wasmtime support or an embedder-brokered donation design. (plan/04 D11/E5)
- **Fuel-quantum resume shim:** fuel accounting is quantum-granular (10k) because wasmtime 45 cannot park a
  fiber at fuel exhaustion; clean fix is upstream. (plan/04 D2/E3)
- **Runtime links no disk/net/pci interfaces yet**; perf is a placeholder; Message API unstarted (blocks
  `text.capture`, pipes, parent↔child channels).
- **`net.loopback` stub** needs wit-bindgen inter-task-wakeup plus host-side concurrent-task support;
  `pci.deny`/`pci.filtered` stubs not yet implemented (area 09).
- **Codegen determinism not verified bit-for-bit** across processes/machines; store cache keys carry
  `compiler_deterministic = false` until it is. (plan/04 D3, plan/06 Decision 8)
- **fs path containment is canonicalize-then-operate** (TOCTOU window vs a racing host process); proper fix
  is openat2/`RESOLVE_BENEATH`-style walks post-MVP. (plan/08 Decisions 7)
- **Shell `env` reads a session-manifest file** as an interim stand-in for a real WIT introspection surface
  (e.g. `eo9:exec/session.grants()`); the raw-mode TTY editor path is not exercised in CI (manual check
  recommended); a child's unterminated output line is not repainted while editing. (plan/10 D9, plan/11 D12)

### Bare metal
- **On-target codegen + interactive composition: DONE on metal.** The kernel compiles components to native
  aarch64 on-target with Cranelift and the interactive eosh runs the full algebra (`$`/`&`/`only`/`configure`)
  fused by the real `eo9_component` and compiled on the machine (merged 7821fbf). Achieved by vendoring +
  de-std'ing the compile layers and the algebra closure under kernel/vendor and making `eo9-component`
  no_std in place (provenance-reviewed clean; usermode byte-identical). Residual: determinism of on-target
  codegen not yet bit-compared (output is reproducible by seed); kernel algebra errors map to each variant's
  `Internal(String)` rather than the specific WIT variants. (plan/12 D26–35)
- **Wasmtime version bumps are not free:** CM-async internals are churning upstream, so any future bump off
  45 requires re-verifying the binder's ABI-constants block (and the kernel executor's mirrored encodings)
  and re-AOT-ing all cached/baked artifacts.
- **Kernel current limits (post bare-metal-MVP):** capability is complete (compose + on-target compile +
  run from the interactive shell); what remains is hardening. Executor and read-line still busy-poll (no
  GIC, no WFI idle); no child fuel yet (lands with the scheduler work); eo9-sched not yet adopted; children
  lack io/buffers + fs/types wiring, so an fs-needing child gets the raw linker missing-import error instead
  of the friendly missing-fs story; no session manifest for headless runs; W^X for JIT code pages still TODO
  (cache maintenance is done); kernel ELF ~23 MB with `wasm-codegen` on (incl. debug info). Behavioral note:
  the no-argument boot is interactive and does not self-power-off — automation uses `demo` or `program=<name>`.
  (plan/12 D22–35)
- **Kernel hardening debt:** identity-map MMU without D/I-cache maintenance on code publication or W^X for
  wasm code pages (QEMU tolerates it, real hardware will not); polled timer; exceptions are fatal.
  (plan/12 D3–4)

### Website / in-browser demo
- **/try is v1**: real example components + grant/revoke demo, explicitly not eosh; stub composition in the
  browser is blocked by an upstream js-component-bindgen TDZ bug (issue text drafted in plan/15 D11); JSPI is
  required for async-main programs (Chromium fine; Safari/Firefox to re-check); keystrokes typed while a run
  is in flight are dropped. (plan/15 D8–14)

## Tracked from the user studies (2026-05-27, see docs/user-studies/00-synthesis.md)

- Debugging: source-line backtraces and a documented debugger workflow (panic-message preservation and the
  --debug-info cache-key bug are fix-now items).
- Security: signed/authenticated stores and provider provenance; hostile-component CI suite + fuzzing of the
  fs provider and ABI boundary; symlink-target-existence oracle (align Denied/NotFound); openat2-style fs
  resolution remains the real TOCTOU fix (fd re-verification is the dispatched interim).
- Metal: child fuel + eo9-sched (preemption) promoted to the next kernel milestone pending owner confirm;
  writable storage + fused-artifact cache on metal; instrumentation (peak heap during on-target compile,
  compose/compile/run timing split, cache-hit reasons); on-target vs host-AOT codegen quality parity check;
  fused-composition cache-hit investigation in usermode.
- Usermode UX: `eo9 new` scaffold and per-package guest builds; optional/defaulted args (existing WAVE-binder
  gap, priority bumped); spawn-time grant visibility for children.

## Minor nits / housekeeping

- `eo9:exec/args` (types-only) is linked only when exec is granted, contra the types-always-available
  convention; `requires_fs` pre-check counts a types-only fs import as requiring a grant.
- Guest-level kill-then-wait test (through `eo9:exec/task`) deferred; host-level covered.
- Direct io-buffer-cap runtime unit test lives in the integration suite only.
- plan/04 D12 still describes the (now fixed) binder trap; update to point at plan/03 D12–13.
- Empty per-process exec-copy directories are never cleaned from the temp dir.
- Scheduler crate (`eo9-sched`) not yet adopted by the CLI drive loop (single-task loop suffices so far).
- Root host workspace manifest lacks a `license = "MIT"` field (guest/www have it; LICENSE file is MIT).
- `eo9-embed::render_outcome` maps abnormal (trapped and killed alike) to exit code 2 — 0/1/2 vs the CLI's
  0/1/2/3 (killed=3); fine as a library convenience (the embedder owns exit mapping), align if it matters.
- `eo9-embed` follow-ups (plan/16): consolidate the `eo9` binary onto eo9-embed to delete the duplicated
  completion→future bridge (D1); engine reuse + optional compile-cache integration (D5); an
  exec-through-Host end-to-end test (D6).
- Nothing has been pushed to origin yet.
