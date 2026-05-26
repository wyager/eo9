# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-26 (master at 0dc0fb5, after the pci / shell-ux / configure-async / kernel-m3 /
xtask-order / web-try wave)._

## Decisions pending with the owner

- _(no open owner decisions right now — /try v2 was settled, see below.)_
- **Compose-time vs run-time provider parameters.** Changing a seed currently changes the composed artifact
  and forces a recompile, same as changing a structural choice. Owner parked the "late-bound parameter"
  idea until there is a clean design; revisit if deterministic sweeps start thrashing the compile cache.

## Settled directions (recorded so they're not re-litigated)

- **/try v2 (wasm32 real-stack browser blob): deferred** (owner ruling 2026-05-26). /try v1 already runs real
  async components in the browser via jco + JSPI, so the demo exists; the real-stack blob would cost month-
  plus (the infeasible-drop-in-backend → fiberless-callback-surgery problem above) and is not MVP-critical.
  Keep v1; revisit the fiberless work — or upstream it — after the MVP. The wasm32 findings live in
  plan/15 D15–20; `eo9-embed` (area 16) remains the shared foundation for whenever it's picked back up.
- **No upstreaming until a compelling MVP** (owner ruling 2026-05-26). The no_std CM-async patch and the new
  cranelift no_std fork stay as in-tree vendored forks under kernel/vendor; revisit offering anything to
  wasmtime/cranelift upstream only once Eo9 has a compelling MVP.
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
- **On-target codegen still blocked by std requirements upstream:** wasmtime's `cranelift` feature and
  wasmtime-environ's `compile` feature require `std`. CM-async is solved on metal (vendored patch); the
  compile layers are the remaining rung — port them for no_std+alloc (required for MVP; Pulley stopgap
  only). Good news from the 2026-05-26 upstream survey: cranelift no_std work is actively landing upstream
  (cranelift-codegen/isle/frontend no_std PRs #12222/#12236/#12947/#13401/#13479, wasmtime-environ refactors
  #12507/#12565, a no_std CI gate #12812) — the port should build on those rather than duplicate them.
  (plan/12 D8, D14, D16)
- **Wasmtime version bumps are not free:** CM-async internals are churning upstream, so any future bump off
  45 requires re-verifying the binder's ABI-constants block (and the kernel executor's mirrored encodings)
  and re-AOT-ing all cached/baked artifacts.
- **Kernel current limits (post boot-to-eosh):** the bare-metal shell can *run* baked-in store programs but
  cannot *compose* them — `compile` is an AOT-artifact lookup, so `$`/`&` return a clean "arrives with
  on-target codegen" error; composition on metal unlocks with the codegen rung. Also: executor and read-line
  still busy-poll (no GIC); no child fuel yet (compile-relevant — lands with re-precompiled artifacts +
  scheduler work); eo9-sched not yet adopted; children lack io/buffers + fs/types wiring, so an fs-needing
  child gets the raw linker missing-import error instead of the friendly missing-fs story; no session
  manifest for headless runs. Behavioral note: the no-argument boot is now interactive and does not
  self-power-off — automation uses `demo` or `program=<name>`. (plan/12 D22–25)
- **Kernel hardening debt:** identity-map MMU without D/I-cache maintenance on code publication or W^X for
  wasm code pages (QEMU tolerates it, real hardware will not); polled timer; exceptions are fatal.
  (plan/12 D3–4)

### Website / in-browser demo
- **/try is v1**: real example components + grant/revoke demo, explicitly not eosh; stub composition in the
  browser is blocked by an upstream js-component-bindgen TDZ bug (issue text drafted in plan/15 D11); JSPI is
  required for async-main programs (Chromium fine; Safari/Firefox to re-check); keystrokes typed while a run
  is in flight are dropped. (plan/15 D8–14)

## Minor nits / housekeeping

- `eo9:exec/args` (types-only) is linked only when exec is granted, contra the types-always-available
  convention; `requires_fs` pre-check counts a types-only fs import as requiring a grant.
- Guest-level kill-then-wait test (through `eo9:exec/task`) deferred; host-level covered.
- Direct io-buffer-cap runtime unit test lives in the integration suite only.
- plan/04 D12 still describes the (now fixed) binder trap; update to point at plan/03 D12–13.
- Empty per-process exec-copy directories are never cleaned from the temp dir.
- Scheduler crate (`eo9-sched`) not yet adopted by the CLI drive loop (single-task loop suffices so far).
- Root host workspace manifest lacks a `license = "MIT"` field (guest/www have it; LICENSE file is MIT).
- Nothing has been pushed to origin yet.
