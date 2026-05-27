# Upstreaming feasibility: no_std forks of the component-tooling crates

_Status report, 2026-05-27. Covers the five component-tooling crates vendored under
`kernel/vendor/` for the bare-metal component algebra (plan/12 Decisions 30–35):
`wit-parser` 0.250, `wit-component` 0.250, `wasm-wave` 0.250 (wasm-tools repo),
`wac-types` 0.10, `wac-graph` 0.10 (wac repo). The wasmtime/cranelift forks are covered
separately. Per the owner ruling, nothing is offered upstream until Eo9 has a compelling
MVP; this report is the prep so that decision can be executed quickly when made._

Every claim about our changes below is grounded in a diff of the vendored copy against the
pristine crates.io source of the same version (local registry). Line counts are total
changed lines (added + removed) in `src/`.

## Executive summary

| Crate | Our delta | Upstream appetite | Effort to a mergeable PR | Verdict |
|---|---|---|---|---|
| wit-parser 0.250 | 68 lines, 1 file: no_std `decode(&[u8])` path; `decoding` feature no longer requires `std` | **High** — upstream landed wit-parser no_std themselves (PR #2415, merged Jan 2026) | ~1 day | **Upstream first** — smallest, most natural follow-up |
| wasm-wave 0.250 | Manifest-only: `wit` feature no longer force-enables `std` | High — upstream landed wasm-wave no_std (PR #2401) | ~0.5 day | **Upstream** (may already be moot on their main — check first) |
| wit-component 0.250 | 333 lines, 16 files: `#![no_std]`+alloc, hashbrown/indexmap hashers, producers/metadata sections behind features, serde deps dropped | Plausible but **needs a design conversation** (the wasm-metadata question) | 3–7 days | Upstream **after** raising the wasm-metadata approach |
| wac-types 0.10 | 90 lines, 9 files: no_std/alloc, hashbrown, `std` feature; anyhow 1.0.100, indexmap 2.14 | Unknown — repo is in maintenance mode, no no_std signal | 2–4 days (bundles dep bumps) | Hold; ask first via an issue when ready |
| wac-graph 0.10 | 43 lines, 4 files: no_std/alloc; petgraph 0.6.4→0.8.3, thiserror 1→2; producers write behind a `metadata` feature | Same as wac-types | 2–4 days (same PR) | Hold; same issue |

Bottom line: roughly **half of our fork surface is already blessed upstream in spirit** —
wasm-tools merged no_std support for wit-parser and wasm-wave in early 2026, and our deltas
to those two crates are small completions of that exact work. The genuinely new
contributions are wit-component no_std (where the right design probably runs through
wasm-metadata, see §3) and the wac crates (where the blocker is project tempo, not code).

## 1. What we changed, per crate

**Pure no_std enablement (the bulk; the part upstream is most likely to want):**

- **wit-parser** — the crate was *already* `#![no_std]` upstream; the gap was that the
  `decoding` feature (the binary `decode()` path eo9-component uses) still required `std`
  because decoding was written against `std::io::Read`. Our change splits it: a no_std
  `ComponentInfo::from_bytes(&[u8])` drives the parser with `eof = true` over a complete
  slice, `decode()` routes through it, and the streaming `from_reader`/`decode_reader`
  wrappers are `#[cfg(feature = "std")]`. Plus the Decision-28-style fix: `decoding` no
  longer hardcodes `wasmparser/std`; wit-parser's own `std` feature forwards it.
- **wit-component** — `#![no_std]` + `extern crate alloc`, an alloc prelude module,
  `IndexMap`/`IndexSet` aliases with a no_std default hasher (indexmap's default
  `RandomState` is std-only), `core::fmt` swaps, explicit drop ordering in `linking.rs`
  where hashbrown's lack of std's `#[may_dangle]` dropck eyepatch otherwise keeps
  self-borrowing maps alive across a `self`-move, and the unused `serde`/`serde_json`
  dependencies removed. No encoder logic changed (verified in the merge review).
- **wac-types / wac-graph** — `#![no_std]` + alloc, `std::collections` → hashbrown,
  crate-level IndexMap aliases (same pattern), `Package::from_file` std-gated, errors via
  `core::error::Error`. The type-checker and instantiation-graph logic are untouched.
- **wasm-wave** — nothing in `src/`; the `wit` feature simply no longer force-enables
  `std` (it now forwards `wit-parser?/std` instead), which is what lets
  `value::resolve_wit_type` work in a no_std build.
- **eo9-component** (our own crate, for context): made no_std-capable in place behind a
  default-on `std` feature; this is what defines the exact feature surface we need from the
  five crates above (`decoding` without std, wasm-wave `wit` without std, etc.).

**Dependency modernization (separable, but not optional for no_std):**

- `thiserror` 1.x → 2 in the wac crates (1.x has no `core::error::Error` path, so this is a
  hard requirement for no_std there, not a preference).
- `petgraph` 0.6.4 → 0.8.3 in wac-graph (0.6 is std-only; 0.8 builds no_std with
  `default-features = false` + `stable_graph`, and wac-graph's whole API surface compiled
  against it with zero source changes).
- `anyhow` ≥ 1.0.100 and `indexmap` 2.14 in the wac crates (older anyhow lacks the no_std
  `core::error::Error` From impl; indexmap needed `default-features = false`).

**The wasm-metadata question (the one genuine design divergence):**

- Upstream `wit-component` and `wac-graph` unconditionally depend on `wasm-metadata` to
  write the informational `producers` (and package-metadata) custom sections. Our vendored
  copies make that path optional behind an **off-by-default** `metadata` feature
  (wit-component grows a no-op `Producers` shim so signatures still compile), because at
  the time the closure was built wasm-metadata looked un-portable.
- Re-checking wasm-metadata 0.250's manifest changes that picture: **all of its heavy
  dependencies (clap, flate2, url, spdx, serde_json, auditable-serde) are already optional**,
  behind default-on `oci`/`serde` features; the always-on deps are just anyhow, indexmap,
  wasmparser, wasm-encoder — all no_std-capable. So upstream's preferred resolution is
  plausibly "make wasm-metadata itself no_std under `default-features = false`" rather than
  "make the producers path optional in wit-component", and our shim would shrink to a
  feature-forwarding line. This is exactly the design conversation to have before writing
  the wit-component PR.

## 2. Upstream landscape

**wasm-tools (wit-parser, wit-component, wasm-metadata, wasm-wave live here).** Very
active; releases roughly monthly in lockstep with wasmtime (the 0.250/1.250 family is
current and is what wasmtime 45 uses). no_std is an explicitly accepted direction:
"Make `wit-parser` support `no_std`" (PR #2415) merged January 2026 with review by
alexcrichton (the changes mirror ours in style — `std` feature, hashbrown, fs code moved
behind a gated module), and "Make `wasm-wave` support `no_std`" (PR #2401) shipped in the
same release window (v1.244–1.245). wasmparser/wasm-encoder have had no_std for much
longer (it is what made our kernel work possible at all). No release notes mention
wit-component or wasm-metadata no_std, and we found no open PR for them — that is the
open ground our fork covers. Drift risk for the files we touched is moderate: wit-parser's
`decoding.rs` and wit-component's encoder are actively developed, so a fork carried for
months will need rebasing; the conceptual changes, however, are small and re-applicable.

**wac (wac-types, wac-graph).** Much slower tempo: 0.10.0 is the latest release and the
`main` branch still pins the same dependency set we forked from — wasmparser/wasm-encoder/
wasm-metadata/wit-parser/wit-component **0.247**, petgraph 0.6.4, thiserror 1.0.58,
indexmap 2.2.6. Two consequences. First, **our "stay on the 0.247 family" choice is not
technical debt against upstream — it is exactly where upstream HEAD is**, so a PR rebases
cleanly today. Second, a no_std PR to wac unavoidably bundles the dependency bumps
(petgraph 0.8, thiserror 2) because the old versions cannot do no_std — more review
surface for a project in maintenance mode. There is no visible no_std demand in the wac
issue tracker; the case we would make is "this is what it takes to run wac-graph inside a
wasm/no_std host", which is a niche but real constituency (us, and anyone embedding
composition in a runtime).

**wasm-wave** is part of the wasm-tools workspace/release train (same 0.250 versioning),
so its half-line of remaining delta rides the same process as wit-parser.

## 3. Gaps between our fork and a mergeable PR

1. **Tests/CI.** wasm-tools gates no_std crates with a `cargo check --no-default-features`
   style job; our changes ship no new tests. A PR needs: a no_std build check for the
   affected crate (wit-parser already has one — extend it to cover `decoding`), and for
   wit-component at least one encode round-trip exercised under the no_std feature set
   (can run on a std host with `default-features = false`, the way we test eo9-component).
2. **The wasm-metadata design call** (§1). Decide with upstream: no_std wasm-metadata
   (preferred, smaller wit-component diff) vs an optional producers path. Our off-by-default
   `metadata` feature would likely need to become default-on (or disappear) upstream —
   omitting producers by default changes observable output and upstream will not want that.
3. **Version-family churn for wac.** Upstream wac will eventually jump 0.247 → current
   wasm-tools; wasmparser 0.250 reshaped the component type model (`ComponentItem`,
   removal of `component_entity_type_of_import/export`), which is precisely the decoder
   port we declined to do (plan/12 D33). If upstream does that bump before taking a no_std
   PR, our wac patches must be re-validated on top of their port — the de-std edits
   themselves are family-independent, so this is re-verification, not a rewrite.
4. **Feature naming/shape.** Our forwarding `std` features mostly mirror upstream
   conventions already (that was deliberate); the additions to review are wit-component's
   `metadata`/`wit-package-metadata` split and wit-parser's `decoding`-without-`std`, both
   of which need an upstream opinion on naming and defaults.
5. **Hygiene.** Drop our vendored-copy header comments, restore doc wording, split commits
   per concern (no_std enablement vs dependency bumps vs feature plumbing) so each is
   reviewable in isolation.

## 4. Effort and carrying cost

| PR | Content | Estimate | Notes |
|---|---|---|---|
| wasm-tools #1 | wit-parser: no_std `decoding` (`from_bytes`) | ~1 day | Direct follow-up to their own PR #2415; smallest, highest-confidence |
| wasm-tools #2 | wasm-wave: `wit` feature without forced `std` | ~0.5 day | Check their main first — may already be fixed |
| wasm-tools #3 | wit-component no_std (+ wasm-metadata no_std or optional-producers, per the design call) | 3–7 days | The real one; do after #1 establishes contact/credibility |
| wac | wac-types + wac-graph no_std + dep bumps | 2–4 days | One PR or two; slower review loop expected |

Carrying cost if we do nothing: wasm-tools moves monthly, so the wit-parser/wit-component
forks accrue rebase work every time we bump the family (each bump so far has been
mechanical but nonzero — and the 0.247→0.250 type-model change shows a bump can
occasionally be expensive). The wac forks are cheap to carry while upstream stays at 0.247,
and become expensive exactly when upstream modernizes — which is also the moment an
upstream no_std PR gets easier. Net: upstreaming the wasm-tools pieces is what buys down
recurring cost; the wac pieces are more "when convenient".

## 5. Recommendation

Consistent with the owner ruling (nothing public until a compelling MVP):

1. **Now (no upstream contact):** keep this report current; add the missing
   `kernel/vendor/README.md` section documenting the five algebra crates (the wasmtime/
   cranelift sections exist, the algebra ones are currently only in plan/12 D30–35); keep
   our `std`-feature shapes aligned with upstream conventions as we touch them.
2. **First moves when the MVP gate opens:** wasm-tools wit-parser `decoding` PR, then the
   wasm-wave manifest tweak (or confirm it is moot). Small, welcome, and they establish the
   relationship for the bigger one.
3. **Then:** open a wasm-tools issue laying out the wit-component/wasm-metadata design
   question before writing that PR.
4. **wac:** open an issue offering the no_std + dep-modernization work and gauge appetite;
   only invest the PR effort if a maintainer engages, otherwise keep carrying the fork
   (cheap while upstream stays on 0.247).
5. **Ordering by value:** wit-parser and wit-component are the highest-value upstreams
   (broadly useful to anyone embedding component tooling in constrained environments, and
   they retire the fastest-moving part of our fork). The wac crates are lower value and
   lower urgency; wasm-wave is a freebie.

### Sources

- Vendored copies vs pristine crates.io sources: `kernel/vendor/{wit-parser,wit-component,wasm-wave,wac-types,wac-graph}` diffed against the local cargo registry copies of the same versions (this repo, 2026-05-27).
- plan/12-kernel.md Decisions 30–35; kernel/vendor/README.md (wasmtime/cranelift sections).
- wasm-tools releases (no_std entries for wit-parser/wasm-wave in v1.244–1.245): https://github.com/bytecodealliance/wasm-tools/releases
- "Make `wit-parser` support `no_std`" (merged Jan 2026): https://github.com/bytecodealliance/wasm-tools/pull/2415
- "Make `wasm-wave` support `no_std`": https://github.com/bytecodealliance/wasm-tools/pull/2401
- wac upstream dependency pins (main branch Cargo.toml — 0.247 family, petgraph 0.6.4, thiserror 1.0.58): https://github.com/bytecodealliance/wac
- wasm-metadata 0.250.0 manifest (heavy deps all optional behind `oci`/`serde`): local registry copy.
