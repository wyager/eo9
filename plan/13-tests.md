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
(record here)
