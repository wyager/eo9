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

1. **Wiring mechanism.** `compose`/`extend`/`restrict`-sealing are built on `wac-graph` 0.10 (register the
   operands as packages, instantiate, wire arguments, encode); slot-name matching and the semver rule are
   ours, wac supplies the type-checked wiring, residual-import merging, and type hoisting. `rename` is direct
   re-encoding of the outer import/export sections (no wrapper layer). Every operation re-validates and
   re-classifies its output via `load`, so a `Component` value is always a well-formed Eo9 module.
2. **`implements` name annotations.** wit-component 0.250 encodes named slots (`import system-fs: eo9:fs/fs`)
   with a `(implements "...")` annotation on the extern name; that annotation is what lets `describe` report
   the interface identity of a plain-named slot, and `rename` emits it when relabeling a default slot.
   wac-graph 0.10 (wasm-tools 0.247 family) rejects that encoding, so the algebra strips the annotations
   before wiring and re-attaches them to the result's own imports/exports afterwards (they are purely
   descriptive; wiring and validation never depend on them). **Escalation:** check whether wasmtime 45
   (0.248 family) accepts `implements` names before area 04 instantiates components with named slots.
3. **Kind classification.** Binary = exports the `main` function and no interfaces; provider = everything
   else that exports only interfaces, types, and optionally `configure` (the empty component is the identity
   provider). Components exporting both `main` and interfaces/`configure`, or any other bare function, are
   rejected as `not-an-eo9-module`.
4. **Describe surface.** The import list contains every interface import, including the types-only
   `eo9:*/types` interfaces that `use` drags in (describe is honest about the component's real imports);
   `required` is derived from the `-optional` interface-name suffix. Slot names and `import-need.interface`
   are versionless; the version is its own field.
5. **Semver rule for 0.x.** The spec defines same-major / >= minor.patch but is silent on pre-1.0. We follow
   the wasm-tools/wac/cargo convention: `0.minor` is the compatibility track (0.1.2 satisfies 0.1.0; 0.2.0
   does not satisfy 0.1.0) and `0.0.x` matches only exactly. Pre-release versions match only exactly.
   **Escalation:** spec should state the 0.x interpretation explicitly.
6. **`restrict` details.** Allow-list entries match by interface name (admitting the `-optional` flavor); a
   version-pinned entry admits imports it could itself satisfy under the semver rule. Types-only interfaces
   (no functions) carry no authority and are always admitted. Optional residuals outside the list are sealed
   by synthesizing the absent provider inline (a generated component whose `default()` returns `none`),
   observationally identical to composing `X.none` — no store dependency. Only the mechanically-derived
   `-optional` shape (nullary option-returning accessors) is sealable; anything else is treated as an error.
7. **Subsumption not yet implemented.** The spec's rule that an export of `X` also satisfies an import of
   `X-optional` needs a mechanically derived `some(·)` adapter; `compose` currently matches slot names only,
   so that adapter is future work (likely shared with the loader in area 04).
8. **Error surface.** The Rust API mirrors the WIT error variants, plus an `internal(string)`-style case on
   rename/restrict errors that the WIT currently lacks (the underlying encoding machinery can fail).
   **Escalation:** consider adding `internal(string)` to `rename-error` and `restrict-error` in wit/exec.
9. **Test fixtures.** Built in-process from WIT text (wit-parser + `wit_component::dummy_module` +
   ComponentEncoder), so this area does not depend on area 07; fixtures cover a self-contained `fix:kit`
   vocabulary plus the real `eo9:text`/`eo9:entropy` packages. "≡" in law tests is observational equality on
   `describe()` (slot sets + kind + args); behavioral equivalence under a runtime is deferred to plan 13.
10. **Dependencies.** Only pinned workspace crates are used (wasmparser, wasm-encoder with its `wasmparser`
    feature, wit-parser, wit-component, wac-graph, and — for `configure` — wasm-wave); the `dummy-module`
    feature of wit-component is enabled for dev-dependencies only. No semver crate: the crate carries a
    ~40-line version parser implementing exactly the spec rule.
11. **`configure` (compose-time binding).** `configure(provider, args)` finds the provider's single exported
    `*-config` interface, WAVE-parses each named arg against `configure`'s declared parameter types
    (wasm-wave `value` types; the same approach the runtime uses for `main` args), and synthesizes a small
    *binder* component that imports that config interface and calls `configure` with the baked-in constants
    from its start function — i.e. exactly once, at instantiation, before any export of the wrapper can be
    called — trapping if `configure` returns an error. Binder and provider are wired with the usual wac
    machinery and every provider export except the config interface is re-exported, so the result is an
    ordinary provider with the config surface sealed away (a second `configure` reports
    `no-config-interface`). Supported parameter types for baking: scalars, `char`, `string`, and enums;
    anything richer is an error for now. Multiple config interfaces on one provider are rejected.
    `describe` now also reports a provider's args from its `*-config` interface (previously only from a
    world-level `configure` export). **Escalations:** (a) behavioral verification is area 13's — in
    particular, the binder calls a sync-lowered, async-lifted `configure` during component instantiation,
    and the Component Model's instantiation/reentrancy rules (wasmtime's `may_enter`/`task` bookkeeping for
    async lifts) may require runtime-side accommodation or a follow-up (e.g. the runtime invoking an explicit
    init export after instantiation instead); (b) the `configure-error` cases implemented here
    (not-a-provider / no-config-interface / unknown-argument / missing-argument / invalid-argument /
    internal) are the reference for the WIT mirror area 02 is adding.
