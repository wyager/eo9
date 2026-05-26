# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-26 (master at 0dc0fb5, after the pci / shell-ux / configure-async / kernel-m3 /
xtask-order / web-try wave)._

## Decisions pending with the owner

- **Configure for resource-owning providers** (fs.memfs, disk.mem, net-style): the binder now forwards
  freestanding sync and async APIs, but interface-owned resources need export-side resource proxying plus
  variant-shaped `task.return` reloading — both mechanical but substantial codegen (plan/03 D13). Options:
  grow the binder, add a runtime-assisted configuration path, or park (note: fs.memfs's `configure` takes no
  args, so the deterministic environment loses nothing today).
- **Upstreaming the wasmtime no_std CM-async patch** (kernel/vendor/wasmtime: 15 files, ~329 lines,
  documented in kernel/vendor/README.md and plan/12 D16): upstream-shaped and worth offering so the vendored
  copy can be dropped; filing it is public activity — owner to decide whether/when/who.
- **/try v2 — eosh in the browser**: prerequisites are a JS exec host (algebra over the transpiled-component
  graph, spawn/wait, WAVE checking), an HTTP-backed store, the upstream js-component-bindgen TDZ fix (issue
  text drafted in plan/15 D11 for the owner to file), a Safari/Firefox JSPI re-check, and a call on how
  faithful compile/fuel semantics must be before the page may call it "eosh". Go/no-go pending.
- **Compose-time vs run-time provider parameters.** Changing a seed currently changes the composed artifact
  and forces a recompile, same as changing a structural choice. Owner parked the "late-bound parameter"
  idea until there is a clean design; revisit if deterministic sweeps start thrashing the compile cache.

## Design decisions deliberately parked

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
  only). (plan/12 D8, D14, D16)
- **Kernel current limits:** executor busy-polls until GIC interrupt handling lands; `text.read-line`
  reports EOF (no UART RX); fuel not yet enabled on metal; no read-only store image or cmdline program
  selection yet (hello's args are fixed in the kernel); eo9-sched not yet adopted. These are the
  boot-to-eosh-on-metal work items. (plan/12 D17–18)
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
