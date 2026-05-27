# Upstreaming feasibility: the no_std compile-layer fork (on-target codegen)

**Scope.** The four compile-layer crates vendored under `kernel/vendor/` for the on-target-codegen rung
(plan/12 Decisions 26–29): `wasmtime-environ` 45.0.0, `wasmtime-internal-cranelift` 45.0.0 (vendored as
`wasmtime-cranelift/`), `cranelift-frontend` 0.132.0, and `wasmtime-internal-unwinder` 45.0.0. The vendored
`wasmtime` runtime patch (CM-async on no_std) and the component-algebra closure (wit-parser, wac-*,
wit-component, wasm-wave) are separate forks covered by their own reports. All numbers below are measured
diffs of the vendored trees against pristine crates.io 45.0.0 / 0.132.0 sources from the local registry.

**Standing constraint.** Owner ruling (2026-05-26, GAPS.md "Settled directions"): nothing is offered upstream
until Eo9 has a compelling MVP. This report is planning input only; it proposes no public activity.

---

## 1. What we changed, per crate

What the fork does overall: make the **compile path** (wasm→CLIF translation, Cranelift driving, object
emission, component/FACT translation) build for `no_std + alloc` (`aarch64-unknown-none`), so the kernel can
compile components on the machine. `cranelift-codegen` 0.132 itself needed **zero changes** — it is already
`#![no_std]`-capable behind its `core` feature and is consumed straight from the registry with
`default-features = false, features = ["core", "host-arch", "pulley"]`. The fork is entirely in the layers
above it and in dependency feature edges.

| Crate | Files touched | Changed lines (±, src) | Manifest lines | Nature |
|---|---|---|---|---|
| `wasmtime-environ` 45.0.0 | 16 src files | ~114 | ~8 | `compile` feature no longer pulls `std`; `wasmprinter` (→ std-only `termcolor`) dropped from `compile` (its single Trace-level dump replaced by a byte-count log); `wasm-encoder` taken `default-features = false`; mechanical `std::`→`core`/`alloc`/`hashbrown` swaps in `compile/`, `fact/`, `component/`; `IndexMap` given an explicit hashbrown hasher; one `HashMap::get` made `&K`-explicit (hashbrown `Equivalent` doesn't deref-coerce); a declaration-order fix in the component inliner (hashbrown lacks std's `#[may_dangle]` dropck eyepatch); `WasmFileInfo.path` / `clif_dir` switched from `PathBuf`/`&Path` to `String`/`&str`. |
| `wasmtime-internal-cranelift` 45.0.0 | 41 src files + 1 new | ~254 (+ 81-line `sync.rs`) | ~27 | `#![no_std]` + `#[macro_use] extern crate alloc` + re-export of `wasmtime_environ::prelude`; per-module `use crate::*;` (a large share of the 41 files contain *only* that line — e.g. `bounds_checks.rs` is exactly the two-line import, no logic change); `std::`→`core`/`alloc`/`hashbrown`; a small non-blocking spinlock `sync::Mutex` for the compiler-context pool; the CLIF-dump `std::fs` write gated behind `feature = "std"` (path types now `String`/`&str`); std-only `cranelift_codegen::timing` calls dropped from two log lines; `cranelift-native` made optional and the host-flag-inference call gated (the kernel always sets an explicit target triple); dependency features de-std'd (`cranelift-codegen` → `core`/`unwind`/`host-arch`, `gimli` → `read`, `object` → `write_core`, `itertools` → `use_alloc`, `thiserror` no defaults). |
| `cranelift-frontend` 0.132.0 | 3 src files | ~19 | ~6 | Vendored mainly to change its `cranelift-codegen` dependency from `features = ["std", …]` to the `core` profile (the feature-unification trap: the hardcoded edge drags `cranelift-codegen/std` → `gimli/std` + `cranelift-control/fuzz` → `arbitrary` onto the target). Plus `#[macro_use] extern crate alloc` and a few `std::`→`core::` swaps the upstream no_std path had missed. |
| `wasmtime-internal-unwinder` 45.0.0 | 0 src files | 0 | ~3 | Manifest-only: drop the hardcoded `std` from its `cranelift-codegen` dependency. |

No codegen logic, instruction selection, register allocation, optimization passes, or verifier/bounds-check/
trap-handling code is changed in any crate (verified at merge review). All of it is gated behind the kernel's
off-by-default `wasm-codegen` feature and applied via `[patch.crates-io]` in the kernel workspace only.

## 2. Overlap with upstream's in-flight no_std work

Upstream's official position today ([platform-support docs](https://docs.wasmtime.dev/stability-platform-support.html),
tracking issue [#8341](https://github.com/bytecodealliance/wasmtime/issues/8341)): the *runtime* is no_std-capable,
but **"Cranelift or Winch are not currently supported"** on no_std — i.e. no_std embeddings are expected to be
AOT-only. Per our 2026-05-26 survey (recorded in GAPS.md/plan), upstream is actively making the *code generator*
no_std-capable — cranelift-codegen partial no_std (#12222), cranelift-isle (#12236), cranelift-assembler-x64
(#12235), a cranelift-codegen no_std CI gate (#12812), cranelift-frontend fixes (#12947), "Restore `#![no_std]`
support for cranelift" (#13401, closed 2026-05-18), riscv64 no_std build fix (#13479), plus no_std-friendly
wasmtime-environ refactors (#12507, #12565); a 2026-04 release already advertises cranelift-codegen compiling
for no_std targets. What upstream is **not** doing (no PR, issue, or stated plan found) is making the layers
that *drive* the code generator — wasmtime-environ's `compile` feature, wasmtime-cranelift, and the dependent
feature edges — no_std. That is exactly where our fork lives.

Per-delta classification:

| Our change | Upstream status | Verdict |
|---|---|---|
| cranelift-codegen no_std itself | Upstream's own active work (we needed zero changes) | Already theirs — nothing to send |
| `cranelift-frontend` std→core dep edge + small core swaps | #12947/#13401 touch frontend no_std; the hardcoded dep edge may already be fixed on main | **Likely redundant soon** — check main before sending; if still present it's a trivial, obviously-correct PR |
| `wasmtime-internal-unwinder` manifest line | Same class as above | Likely redundant soon; trivial if not |
| `wasmtime-environ`: `compile` without `std`, hashbrown/prelude swaps, wasmprinter drop | No upstream work found on no_std `compile`; #12507/#12565 are friendly refactors, not enablement | **Genuinely novel** — the core contribution |
| `wasmtime-environ`: `clif_dir`/`WasmFileInfo.path` → `String` | Touches a public-ish builder API; upstream may prefer a `cfg(std)` gate or their own design for debug output | Novel but **design-sensitive** — expect bikeshedding |
| `wasmtime-internal-cranelift`: `#![no_std]` + the whole port (incl. `sync::Mutex`, cranelift-native gating, gimli/object alloc-only paths, debug-info transform under no_std) | No upstream work found | **Genuinely novel** — the bulk of the value, and the bulk of the review burden (DWARF transform + component compiler under no_std is a large surface to convince reviewers of) |
| Dropping std-only `timing`/`wasmprinter` conveniences | Upstream would likely want these `cfg`-gated rather than removed | Needs reshaping before sending |

Bottom line: roughly the two small crates' worth of our fork (~25 lines) is likely to be obsoleted by upstream
on its own; the ~480 lines across wasmtime-environ + wasmtime-cranelift are a real contribution upstream has
not started, sitting squarely on their stated roadmap direction (#8341) — historically receptive territory.

## 3. Gaps between "works for us" and "mergeable upstream"

- **Version drift.** Our fork is against 45.0.0; upstream PRs target `main` (46 branches ~mid-June). The
  compile-layer files do drift (the 0.247→0.250+ wasmparser churn, ongoing component-compiler work), so the
  port must be re-applied to main, not submitted as-is. Mostly mechanical, but it is a re-do, not a rebase.
- **CI story.** Upstream gates no_std claims with build jobs (e.g. the cranelift-codegen no_std CI gate
  #12812). A mergeable PR needs an equivalent check — e.g. `cargo check -p wasmtime --no-default-features
  --features compile,cranelift --target <no_std target>` — added to their CI matrix, plus keeping the
  `std` build byte-identical. We have exactly one downstream consumer (our kernel); upstream will want the
  gate in-tree.
- **Feature/naming choices.** Upstream may prefer `compile` to *imply* a new `compile-std` (or keep `std` and
  add `compile-core`) rather than our silent relaxation; the CLIF-dump path (`clif_dir`) and the dropped
  `timing`/`wasmprinter` conveniences will likely need to stay available behind `cfg(feature = "std")` instead
  of being removed; the `String`-vs-`Path` API change needs their sign-off.
- **The spinlock `sync::Mutex`.** Fine in a kernel, questionable as an upstream default (panic-on-contention).
  Upstream would probably want it expressed via their existing `sync_nostd` philosophy in the wasmtime crate
  or a documented platform hook, not a new module in wasmtime-cranelift.
- **Review surface.** The wasmtime-cranelift port touches the DWARF/debug transform and the component
  compiler; reviewers will want assurance (tests or structured argument) that the no_std path doesn't fork
  behavior. Our merge-review provenance write-ups (plan/12 D26–29, kernel/vendor/README.md) are most of that
  argument already.

## 4. Effort and maintenance trade-off

| Path | Effort (rough) | Notes |
|---|---|---|
| Upstream the two manifest-edge crates (frontend, unwinder) | ½ day total, if still needed on main | Check main first; may already be fixed |
| Upstream wasmtime-environ `compile` no_std | 2–4 days | Re-apply to main, add CI check, negotiate the path-type/feature-gating choices |
| Upstream wasmtime-cranelift no_std | 5–10 days | The big one: re-apply to main (drifted files), reshape Mutex/timing/clif-dump per upstream taste, CI, review iterations |
| Do nothing and rebase the fork on each pin bump | ~1–3 days per bump | The port re-applies mostly mechanically, but the CM-async ABI churn already makes bumps deliberate events for us; each bump re-pays a slice of this fork |
| Wait-and-drop | n/a | Not realistic for environ/cranelift: upstream has no visible plan to do this part; only the two manifest crates are likely to self-resolve |

Carrying cost today is low (the fork is frozen against our 45 pin and CI-gated); the cost concentrates at
version bumps. Upstreaming the environ + cranelift port is the only way that cost goes to zero, and it also
deletes ~4.8 MB of vendored source from the repo.

## 5. Recommendation

1. **Hold, per the owner ruling** — nothing goes out until the MVP is compelling. Nothing in this fork is
   time-critical to send; the only external clock is upstream drift making the eventual re-apply slightly
   larger.
2. **When the MVP gate opens, send in this order:** (a) the `cranelift-frontend`/`unwinder` dependency-edge
   fixes if main still needs them (trivial, builds goodwill and a CI precedent); (b) `wasmtime-environ`
   `compile`-without-`std` (small, self-contained, unlocks every downstream embedder who wants on-target
   codegen); (c) the `wasmtime-cranelift` no_std port (the substantial one — open an issue referencing #8341
   first to agree on feature naming, the CLIF-dump/Path questions, and the Mutex story before sending code).
   Bundle (b)+(c) conceptually with the CM-async no_std runtime patch (separate report) as one "no_std
   on-target compilation" narrative — it is a much stronger pitch together.
3. **Free prep we can do now (no public activity):** keep kernel/vendor/README.md's per-file change log
   current (it is the PR description); when we next bump the wasmtime pin, structure the re-apply as clean
   per-concern commits (feature edges / mechanical swaps / API-shape changes) so the eventual PR series falls
   out of our own history; and re-check upstream main for the frontend/unwinder edges before assuming we need
   to send them.

Sources: [Wasmtime platform support](https://docs.wasmtime.dev/stability-platform-support.html) ·
[Add no_std support to Wasmtime #8341](https://github.com/bytecodealliance/wasmtime/issues/8341) ·
[wasmtime releases](https://github.com/bytecodealliance/wasmtime/releases) · 2026-05-26 upstream survey
(GAPS.md / plan/12, PR numbers #12222 #12235 #12236 #12507 #12565 #12812 #12947 #13401 #13479) ·
measured diffs of kernel/vendor/* vs pristine registry sources (this report, §1).
