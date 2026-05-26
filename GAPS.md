# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-25 (master at a9e3669)._

## Design decisions deliberately parked

- **Compose-time vs run-time provider parameters.** Changing a seed currently changes the composed artifact
  and forces a recompile, same as changing a structural choice. Owner parked the "late-bound parameter"
  idea until there is a clean design; revisit if deterministic sweeps start thrashing the compile cache.
- **Content-only vs layout-dependent eofs node hashes.** eofs `stat` hashes are Merkle roots over the
  physical layout; the spec's fs hash queries may want content-only identity (format v2 field). Decide when
  eofs milestone 2 specifies the `eo9:fs` hash surface. (plan/14 Decisions 4)
- **Component-typed arguments** (`interpret (…)`): spec says components cross boundaries as bytes; the
  concrete convention is undesigned. Revisit when something consumes it. (plan/10 Decisions 6b)
- **Exec-copy cleanup / Santa alert noise / crates.io name** — operational niceties, owner-facing.

## Functional gaps (implementation exists, coverage incomplete)

- **`configure` rejects resource-owning / async-API providers** (fs.memfs-, disk.mem-, time.frozen-, net-style):
  the bind-on-first-use binder only forwards freestanding sync APIs today; such providers fail with a clean
  error. Practical impact: invoker-side configuration currently works only for `entropy.seeded`/`perf.null`,
  so the fully invoker-configured deterministic environment is not yet possible — the highest-value algebra
  follow-up. Unblock: async-capable forwarders + resource proxying in the binder, or a runtime-side
  configuration path. (plan/03 D12)
- **Guest-facing `resume` unsupported (E5):** children are fuel-sliced out of the parent's own donation, so a
  guest scheduler cannot direct CPU itself and long-running children throttle the shell. Unblock: upstream
  wasmtime support or an embedder-brokered donation design. (plan/04 D11/E5)
- **Fuel-quantum resume shim:** fuel accounting is quantum-granular (10k) because wasmtime 45 cannot park a
  fiber at fuel exhaustion; clean fix is upstream. (plan/04 D2/E3)
- **Runtime links no disk/net interfaces yet**; perf is a placeholder; Message API unstarted (blocks
  `text.capture`, pipes, parent↔child channels).
- **`net.loopback` stub** needs wit-bindgen inter-task-wakeup plus host-side concurrent-task support.
- **Codegen determinism not verified bit-for-bit** across processes/machines; store cache keys carry
  `compiler_deterministic = false` until it is. (plan/04 D3, plan/06 Decision 8)
- **fs path containment is canonicalize-then-operate** (TOCTOU window vs a racing host process); proper fix
  is openat2/`RESOLVE_BENEATH`-style walks post-MVP. (plan/08 Decisions 7)
- **Configure binder leans on the CM async ABI's packed subtask-status encoding** — must track wasmtime revs.
  (plan/03 D12)
- **Indirect-params ABI case** (>16 flattened configure params) rejected gracefully, not supported.

## Minor nits / housekeeping

- `eo9:exec/args` (types-only) is linked only when exec is granted, contra the types-always-available
  convention; `requires_fs` pre-check counts a types-only fs import as requiring a grant.
- Guest-level kill-then-wait test (through `eo9:exec/task`) deferred; host-level covered.
- Direct io-buffer-cap runtime unit test lives in the integration suite only.
- xtask: run build-guest before the test step to remove the stale-component hazard for manual `cargo test`.
- plan/04 D12 still describes the (now fixed) binder trap; update to point at plan/03 D12.
- Empty per-process exec-copy directories are never cleaned from the temp dir.
- Scheduler crate (`eo9-sched`) not yet adopted by the CLI drive loop (single-task loop suffices so far).
- Nothing has been pushed to origin yet.
