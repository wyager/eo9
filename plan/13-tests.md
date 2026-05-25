# 13 — Test suite (`tests/`, plus per-crate tests)

## Scope
The cross-cutting test strategy and the two integration suites the spec asks for (usermode and in-QEMU).
Per-crate unit tests belong to their areas; this area owns the shared fixtures, the integration harnesses,
and the CI gates.

## Spec references
"Test Suite" deliverable, the algebraic laws throughout (Composition, `&`, `only`, slots), the capability
rules, determinism claims, kill/linearity contract.

## Deliverables
- **Law tests** (with plan 03): property-style tests for every law stated in the spec — sealing, residual
  formula, kind preservation/layering, `&` associativity + identity + action law, `only` idempotence and
  intersection, rename round-trips, "drop of something never imported is a no-op".
- **Capability tests** (usermode, drives the `eo9` binary): a dropped/sealed capability cannot be re-granted
  from outside; `only` fails before run with the right error; `net.deny` fails in-band in the program's own
  error vocabulary; optional-import programs observe absence; `with` wires two fs slots to different
  providers.
- **Determinism tests**: the deterministic environment (`fs.memfs & time.frozen & entropy.seeded`, fuel
  scheduling, deterministic policy) produces byte-identical outcomes and identical store/compile-cache hashes
  across repeated runs and across machines (CI matrix).
- **Concurrency tests**: `many-reads` under load; fuel-conservation accounting; kill mid-I/O leaves no leaks
  (linearity contract), verified with runtime instrumentation.
- **Usermode integration harness**: golden-transcript runner for eosh sessions; example-program matrix.
- **QEMU harness**: boot each arch image, drive serial with expect-style scripts, assert on output; smoke
  tier (hello) in CI, fuller tier nightly/local.
- CI gates per integration milestone (I1–I5) so regressions are visible area-by-area.

## Dependencies
Everything; starts alongside Phase 1 (law tests) and grows with each milestone.

## Milestones
1. Law-test framework + fixtures from `guest/examples` (with plan 03's milestone 2).
2. Usermode harness + capability + determinism suites (gates I2).
3. Concurrency suite (gates I3); QEMU harness (gates I4/I5).

## Decisions

1. **Package layout.** The integration suite is a new workspace member `tests/eo9-integration`
   (package `eo9-integration`), registered in the root workspace so `cargo xtask ci` picks it up with
   the normal fmt/lint/build/test gates. The library target is the reusable harness — `fixtures`
   (building executable components in-process) and `run` (compiling with the pinned engine and driving
   a task to its outcome under given root providers) — and the suites live in
   `tests/{harness,capabilities,determinism,kill}.rs`.
2. **Fixture strategy: WIT + hand-written core module, built in-process.** Executable fixtures are
   built from WIT text plus a small hand-written core-module WAT (legacy canonical-ABI names), joined
   with `wit_component::embed_component_metadata` + `ComponentEncoder` — the same pipeline real guest
   components go through, so the encoding is exactly what the algebra and the runtime expect. This
   generalizes area 03's dummy-module fixtures to fixtures with behaviour, and avoids depending on
   `guest/` artifacts (ci runs `test` before `build-guest`) or on area 09's stubs (built in parallel).
   Two vocabularies: a self-contained, resource-free `eo9-tests:cap` package (`answer`,
   `answer-optional`, `store`) for the capability suite, and fixtures against the real `eo9:text` /
   `eo9:entropy` / `eo9:time` packages for the ambient-context and determinism tests. The
   kill/linearity guest is raw component WAT (it needs the CM async built-ins to park on a host
   future; `Image::compile` accepts WAT directly).
3. **New workspace pin: `wat`.** Assembling the hand-written core modules needs the wasm text-format
   parser, so `wat = "1.250.0"` (wasm-tools family, same 250 release train, already in the lockfile
   transitively) was added to the root pin table and is used only by this package. Flagged for area
   01 / the planner since the pin table is area 01's file.
4. **Milestone-1 test groups** (17 tests total): *capabilities* (8) — sealing vs. an outer provider
   and vs. the ambient root providers (plus the loader-rule rejection when unsealed); `only` failing
   before run and naming the offenders; `only` admitting capabilities satisfied inside the gate;
   `only` sealing optional residuals with absence observed at run time; absence ≡ `none`-provider
   composition and presence through the same `-optional` import; a deny-style provider failing
   in-band into the program's own failure variant; `with`-style rename wiring two slots of one
   interface to different providers. *determinism* (4) — byte-identical outcomes and captured output
   across runs under frozen time + seeded entropy + fixed WAVE args; byte-identical
   compose/extend/rename/restrict outputs; byte-identical fixture builds; store cache keys stable and
   sensitive to every field, keyed off real algebra outputs. *kill/linearity* (2) — killing a task
   blocked on a provider future leaks nothing observable (the in-flight op and its buffer are
   dropped, the provider's backend completes quietly afterwards), plus the un-killed contrast run.
   *harness* (3) — fixture self-checks (every fixture builds, validates, and classifies correctly).
5. **Reported gaps (not worked around).** (a) `eo9_runtime::Image` exposes no serialized bytes (image
   serialization is deferred in plan/04 § D7), so byte-identity of `Image::compile` output cannot be
   asserted through the public API yet; behavioural determinism of compile-and-execute is covered
   instead, and the byte-level check should be added when image serialization / the cache hook-up
   lands. (b) The loader rule's "missing *optional* import is auto-sealed with `X.none`" is not
   implemented in the runtime's spawn path (only root providers are linked), so optional absence is
   exercised via composition and `only`; flag for areas 04/11. (c) Cross-machine / cross-process
   codegen determinism is out of scope for this suite (store-cache concern, areas 04/06) — everything
   here is in-process, same-machine determinism.
6. **Deferred to milestone 2.** Adopting area 09's stub provider components (`fs.memfs`,
   `time.frozen`, `entropy.seeded`, …) in place of the in-memory host providers and the fixture
   providers; golden-transcript CLI tests driving area 11's `eo9` binary (and eosh sessions);
   the `many-reads` concurrency soak and fuel-conservation accounting (gates I3); the QEMU harness
   tier (gates I4/I5). Law-level property tests stay with area 03
   (`crates/eo9-component/tests/algebra.rs`); this package covers the runtime-observable semantics.
