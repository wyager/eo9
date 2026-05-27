# Upstreaming feasibility: the wasmtime no_std component-model-async patch

**Scope.** This report covers our locally modified copy of the `wasmtime` crate itself
(`kernel/vendor/wasmtime`, pinned at 45.0.0, applied via `[patch.crates-io]` in the kernel
workspace only). The other vendored crates (wasmtime-environ, wasmtime-cranelift,
cranelift-frontend, wasmtime-unwinder, and the component-algebra closure) are covered by
separate reports. This is preparation only: per the owner ruling, nothing is sent upstream
until Eo9 has a compelling MVP.

**Ground truth.** All claims below are from a fresh diff of `kernel/vendor/wasmtime` against
the pristine crates.io `wasmtime-45.0.0` sources (2026-05-27): 21 source files changed,
+294/−79 lines total, falling into two separable contributions. No fiberless-callback
execution path has been added locally (that idea was estimated and deferred).

---

## 1. What we changed and why

### Contribution A — `component-model-async` without `std` (~271 changed lines, 15 files)

Why: every Eo9 guest uses the Component Model async ABI (async `main`/`configure`, async
I/O), and the bare-metal kernel is `no_std + alloc`. Upstream gates the whole CM-async host
machinery on `std`, but the std surface it actually uses is small and mostly incidental
(documented in `kernel/vendor/README.md` and `plan/12-kernel.md` D14–D16).

| Piece | Files | Nature |
|---|---|---|
| Feature graph | `Cargo.toml` (8 lines) | `component-model-async` no longer requires `std`/`futures/std`; the `futures` items used (oneshot, `FuturesUnordered`, `StreamExt`) exist under `futures/alloc`. |
| core/alloc swaps | `concurrent.rs`, `concurrent/{table,abort,future_stream_any}.rs`, `futures_and_streams{,/buffers}.rs`, `linker.rs`, `vm.rs`, `vm/component/libcalls.rs` (~131 lines) | `std::` paths that are really `core`/`alloc`; `std::collections::HashSet` → `crate::hash_set`; `std::sync::{Arc, Mutex}` → `alloc::sync::Arc` + `crate::sync::Mutex`; a ~30-line private `VecCursor` replaces `std::io::Cursor`; the `std::io::Read`/`Write` convenience impls become `cfg(feature = "std")`; two `oneshot::Canceled` conversions construct errors explicitly (the `Error` impl on `Canceled` is std-only). |
| `crate::sync::Mutex` | `sync_nostd.rs` (+68), `sync_std.rs` (+4) | On std: a re-export. On no_std: a small non-blocking mutex (panic-on-contention `lock`, `WouldBlock` `try_lock`), mirroring the file's existing single-threaded philosophy. |
| Concurrent TLS via the custom platform | `concurrent/tls.rs` (41), `vm/sys/custom/{capi,mod}.rs` (19) | Without `std` there is no `thread_local!`; the one pointer the concurrent machinery stores moves to an embedder-provided slot reached through **two new custom-platform symbols, `wasmtime_concurrent_tls_get/set`**, with the same contract as the existing `wasmtime_tls_get/set` used by trap handling. Gated on `component-model-async`; the std build is unchanged. |

Notable non-change: `wasmtime-fiber` needed nothing — upstream already ships the no_std
backend with the aarch64 stack switch, which is why async guests work on our kernel.

### Contribution B — `std`-gating of the compile path inside the `wasmtime` crate (~71 lines, 6 files)

Added later by the on-target-codegen rung: `compile.rs`, `compile/{code_builder,runtime,stratify}.rs`,
`config.rs`, `engine.rs` — mechanical core/alloc swaps, file-reading `CodeBuilder` methods and
`Config::emit_clif` gated behind `feature = "std"` (paths carried as `String`/`&str`), and the
`cranelift` feature usable without `std` on our build. This piece only makes sense upstream as
part of the larger "no_std Cranelift/compile" story (tracked in the wasmtime-environ /
wasmtime-cranelift report) and should ride with that series, not with Contribution A.

---

## 2. Upstream receptivity

- **There is an explicit, active no_std program.** The umbrella issue is
  [bytecodealliance/wasmtime#8341 "Add no_std support to Wasmtime"](https://github.com/bytecodealliance/wasmtime/issues/8341),
  and the official platform-support page documents the no_std embedding story and the custom
  platform layer ([docs.wasmtime.dev/stability-platform-support.html](https://docs.wasmtime.dev/stability-platform-support.html)).
  That page currently lists the features that work without `std` as
  `runtime, gc, component-model, pulley, async, debug, debug-builtins, demangle, anyhow` —
  i.e. plain `component-model` and even `async` (fibers) are already no_std, and
  `component-model-async` is the conspicuous gap our patch fills. Filling a documented gap in
  an existing initiative is the most favorable upstreaming posture there is.
- **Our approach matches upstream idioms.** The patch reuses the crate's own abstractions
  (`crate::sync`, `crate::hash_set`, `cfg(feature = "std")` gates, the custom platform
  `capi.rs` symbol table) rather than inventing new ones; the new TLS pair is modeled
  one-for-one on the existing `wasmtime_tls_get/set`. The platform-support doc states the
  custom platform header is "not guaranteed to be stable", so adding symbols there is an
  accepted kind of change (and PRs for platform support are explicitly welcomed).
- **Maintainer landscape.** The CM-async ("concurrent") implementation is primarily authored
  and reviewed by the core wasmtime maintainers (the component-async work is driven by the
  same people behind [dicej/component-async-demo](https://github.com/dicej/component-async-demo)
  and the upstream component-model async test-suite work,
  [WebAssembly/component-model#571](https://github.com/WebAssembly/component-model/issues/571)).
  Our 2026-05-26 survey of `main` found no open issue or PR proposing no_std CM-async, so we
  would not be colliding with in-flight work — but also no pre-existing demand signal beyond
  the umbrella no_std issue.
- **Process.** This does not rise to RFC level (no public API redesign); it is a normal PR
  series against `main` with a feature-graph change, plus an addition to `wasmtime-platform.h`
  (generated from `capi.rs`). The bar will be CI coverage and not regressing std builds.

## 3. Gaps between "works for us" and "mergeable upstream"

1. **Version drift.** We are on 45.0.0; upstream PRs land on `main`, and the CM-async
   internals are under active churn (cancellation/starvation/priority work, e.g. #13196,
   #13269, #13040, #12357 in our survey). The *shape* of our changes (imports, feature graph,
   TLS shim, sync_nostd Mutex) ports straightforwardly, but every line lands in files that
   have moved; expect a genuine rebase, not a cherry-pick.
2. **CI / tests.** Upstream will require their no_std builder to cover
   `component-model-async` (today it deliberately excludes it). We have no tests inside the
   wasmtime tree — our evidence is an out-of-tree embedding (the Eo9 kernel). A mergeable PR
   needs at least: the no_std build job extended to the new feature combination, and ideally a
   `min-platform`-style example exercising an async-lifted call without `std`.
3. **Design points that may get bikeshedded.**
   - The **panic-on-contention `Mutex`** in `sync_nostd.rs`: fine for single-threaded
     embeddings (and consistent with that file's philosophy), but reviewers may ask whether
     the concurrent machinery should instead require an embedder-provided lock, or whether
     `lock()` should spin.
   - The **second TLS slot**: reviewers may prefer folding it into the existing
     `wasmtime_tls_*` slot (one pointer struct) rather than adding a second symbol pair, to
     keep the platform header minimal.
   - The `VecCursor` and explicit `Canceled` error construction are uncontroversial.
4. **Platforms we ignored.** We only build/test `aarch64-unknown-none`. Upstream will want
   the feature combination to at least compile for their other no_std CI targets.
5. **Contribution B is sequenced behind a bigger story.** The compile-path std-gating only
   matters once the no_std Cranelift/compile work (where upstream is already active —
   cranelift no_std PRs #12222/#12236/#12947/#13401/#13479, environ refactors #12507/#12565,
   no_std CI gate #12812) reaches the `wasmtime` crate; submit it with that series.

## 4. Effort estimate and plan

| Piece | Size | Risk | Notes |
|---|---|---|---|
| A1: feature graph + core/alloc/Cursor/Canceled swaps | 2–3 days | Low | Mechanical; mostly re-finding the lines on `main`. |
| A2: `sync_nostd::Mutex` | 0.5–1 day | Low–Med | Small code, possible design discussion. |
| A3: concurrent TLS via custom platform (new symbols + header regen) | 1–2 days | Med | The one genuinely new design surface; alternative designs possible. |
| A4: CI + min-platform-style async example upstream expects | 2–4 days | Med | The real cost of mergeability. |
| Review/iteration overhead (1–2 rounds) | 2–4 days | Med | CM-async code is actively evolving; review attention should be available. |
| **Contribution A total** | **~8–14 engineer-days** | | Best done as 2–3 stacked PRs (A1+A2, then A3, then CI/example). |
| Contribution B (wasmtime-crate slice of no_std compile) | 1–2 days *if* riding the larger codegen series | Low | Pointless standalone. |

**Cost of not upstreaming:** the patch must be re-applied on every wasmtime upgrade. Given the
churn rate in `concurrent.rs`/`futures_and_streams.rs`, expect roughly 1–3 days per major
version bump just for this crate (plus re-AOT of all artifacts, which a bump costs us anyway).
Two or three skipped versions cost more than upstreaming once.

**What we keep locally regardless:** the kernel's embedder side (the two TLS statics, the
`CustomCodeMemory` publisher, engine tunables) — those are ours by design and not upstreamable.

## 5. Recommendation

1. **Hold until the MVP ruling is satisfied** (current owner position), but treat
   Contribution A as the *first* thing we upstream when we do — it is small, self-contained,
   fills a documented gap in an active upstream initiative, and deletes our highest-churn
   vendored file set.
2. **Order:** A1+A2 → A3 → (CI/example) as a stacked series; Contribution B later, attached to
   the no_std-compile series alongside the wasmtime-environ/wasmtime-cranelift work.
3. **Free prep we can do now:** keep `kernel/vendor/README.md` change-grouping current (it is,
   and it is already written in reviewer-ready form); when we next bump wasmtime versions,
   rebase the patch rather than re-deriving it, and note any upstream file moves; consider
   adding a tiny in-tree test in our kernel that exercises exactly the surface the upstream
   example would need (so the eventual PR's example is a port, not new work).
4. **Engage lightly before the PR:** when the time comes, a short note on the umbrella
   no_std issue (#8341) describing the intended series is the cheapest way to surface design
   preferences (one TLS slot vs two; Mutex semantics) before code review.
