# 03 — Component algebra (`crates/eo9-component`)

## Scope
The pure, unprivileged value algebra on components, as a host library: load/save/describe, `$` (compose),
`&` (extend), `only` (restrict), `rename`. No execution, no I/O policy — math on bytes.

## Spec references
"Composition and the `$` operator" (incl. Algebraic properties), "Environments and the `&` operator",
"Capability slots, `rename`, and `with`", "The capability algebra", "Programs as values",
"Execution APIs → component-algebra".

## Deliverables
- `eo9-component` crate (std, host). Public API mirrors the WIT `component-algebra` interface so plan 04 can
  expose it to guests with a thin shim:
  - `load(bytes) -> Component` (validate; classify kind binary/provider per the main/configure rule),
    `save`, `describe` (imports as slots: name, interface, version, required/optional; exports; main arg
    signature extracted from the component type).
  - `compose(p, c)` — match by slot name, seal matched imports, drop unmatched provider exports (layering),
    result exports = consumer's.
  - `extend(x, y)` — `&`: wire y's imports from x's exports, right-biased export union, import union minus
    satisfied.
  - `restrict(c, allow)` — `only`: error listing required residual imports outside `allow` (match by
    interface type, semver rule); seal optional residuals outside `allow` as absent. Sealing may synthesize
    the trivial absent adapter inline (observationally identical to composing `X.none`) to avoid a store
    dependency — note this in docs.
  - `rename(c, from, to)` — slot relabeling (imports and exports), implemented as a generated forwarding
    adapter or direct re-encoding, whichever is cleaner.
- Semver matching helper (same-major, ≥ minor.patch) used by compose/restrict.
- Law tests (see plan 13): sealing, residual formula, kind preservation, `&` associativity + action law
  `(x & y) $ c ≡ x $ y $ c`, `only` idempotence/intersection, rename round-trip. "≡" = observational
  equality on describe() plus behavior under a test runtime where feasible.

## Implementation notes
- Build on `wit-parser`/`wit-component`/`wasm-encoder`; evaluate `wasm-compose` / `wac-graph` for the wiring
  step before writing custom linking. Fusion/optimization is *not* this crate's job (the runtime/compiler
  handles inlining); this crate only produces correctly-wired composed components.
- Determinism: same inputs must produce byte-identical outputs (the store keys on this — plan 06).

## Dependencies
01, 02. Consumed by: 04 (runtime exposes it as the guest-facing interface), 06 (hashing composition DAGs),
10 (eosh calls it via WIT), 13.

## Milestones
1. load/describe/save + kind classification, with tests against `guest/examples`.
2. `compose` + semver matching + sealing tests (enough for I2's `$`).
3. `extend`, `restrict`, `rename` + full law suite.

## Decisions
(record here)
