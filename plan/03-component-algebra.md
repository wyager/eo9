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
    *binder* component. Binder and provider are wired with the usual wac machinery; the wrapper re-exports
    the provider's API surface with the config interface sealed away (a second `configure` reports
    `no-config-interface`). Supported parameter types for baking: scalars, `char`, `string`, and enums;
    anything richer is an error for now. Multiple config interfaces on one provider are rejected.
    `describe` now also reports a provider's args from its `*-config` interface (previously only from a
    world-level `configure` export). The `configure-error` cases implemented here (not-a-provider /
    no-config-interface / unknown-argument / missing-argument / invalid-argument / internal) are the
    reference for the WIT mirror area 02 is adding.
12. **`configure` binder is bind-on-first-use (root cause of the instantiation trap).** The first binder
    design called `configure` from the binder module's core `start` section; that traps under wasmtime 45
    with "uninitialized element": wit-component lowers an import that needs memory/realloc *indirectly*,
    through a funcref-table shim filled in by a fix-up instance only after the main module is instantiated,
    so a start-section call goes through a not-yet-initialized table slot. Deferring the call to
    `_initialize` fixes the table ordering but then hits the Component Model's concurrency rules
    ("cannot block a synchronous task"): `configure` is an async-lifted export, and neither instantiation
    nor a synchronous consumer task may make a blocking (sync-lowered) call to it. The binder therefore now
    imports the provider's API interfaces as well and re-exports them through gating forwarders: the first
    forwarded call *async-lowers* `configure` with the baked-in constants (accepting only an
    immediately-completed result; blocking or an error traps), marks the provider configured, and every call
    then forwards flat values unchanged (releasing lent borrow handles per the canonical ABI). Configuration
    thus still happens exactly once, before any observable API use, inside the consumer's own task — which
    is what wasmtime's rules permit — and the end-to-end check
    (`eo9-runtime/tests/exec_api.rs::algebra_configured_composition_observes_the_seeded_stream`) proves the
    baked-in seed is observed with no program-side configuration. **Limitations (escalate as needed):**
    providers whose API interfaces define their own resources (fs-, net-style interfaces), have non-
    freestanding or async API functions, or nest borrows inside parameters are rejected by `configure` for
    now — binding those needs either resource proxying in the binder or runtime-side configuration.
    *(Partially superseded by D13: async freestanding API functions are now forwarded; interfaces that
    define their own resources are still rejected.)*
13. **The binder forwards async API functions (async-callback lifts + async-lowered calls).** Each forwarded
    function follows its own ABI: sync functions keep the flat passthrough (now with a per-call result
    buffer instead of the shared scratch area), and `async` functions are re-exported as async (callback)
    lifts that async-lower the provider call. An immediately-completed provider call is returned within the
    same task (`task.return`, then exit); a provider that genuinely suspends is parked — the subtask joins a
    fresh per-call waitable set, the per-call frame (subtask, set, lent borrows, result area) is recorded in
    the task-local context slot, and the callback completes the call when the subtask's "returned" event
    arrives. A new in-flight counter makes the bump allocator sound under concurrent calls (it is only reset
    when nothing is in flight), and lent borrows are released after the forwarded call completes, per the
    canonical ABI. The configuration gate itself is unchanged. Supported async shapes: freestanding
    functions whose parameters flatten to at most four values (the async-lower limit) and whose results are
    nothing, scalars, enums, shared-resource handles, strings, or lists; variant-shaped results
    (`option`/`result`/variants, e.g. every fs/disk operation) need discriminant-dependent reloading for
    `task.return` and are rejected with a clear error. In practice this makes the time-shaped providers
    (`time.frozen`, `time.monotonic-stub`, `time.fuzzy`) invoker-configurable; with `entropy.seeded` that is
    enough for the invoker-configured deterministic environment
    (`tests/eo9-integration/tests/invoker_configured_env.rs`: the guest imports no config interfaces, its
    `sleep` through the configured frozen clock completes, runs are byte-identical, ambient providers do not
    leak, and the `&` form agrees with the `$` chain). The suspended path is implemented per the canonical
    ABI (constants mirrored from wasmtime 45's `concurrent.rs`) but is not yet exercised end-to-end — no
    configurable provider blocks today; `configure(time.fuzzy, …)` over a real clock will be the first.
    **Remaining limitations (escalate as needed):** (a) interfaces that define their own resources (fs,
    disk, net, io) still need export-side resource proxying (`[resource-new]`/`[resource-rep]`/`[dtor]`
    wrappers whose representation is the provider's handle) *and* variant-result reloading — those are the
    two concrete blockers for `configure(fs.memfs)` / `configure(disk.mem)`; (b) cancellation of an
    in-flight forwarded call is unsupported and traps (a caller that drops a pending future would hit it;
    none of the current guests cancel); (c) the binder now also leans on the packed callback-code and
    subtask-event encodings of the CM async ABI, kept in one constants block at the top of `configure.rs`.

14. **Guest-leaf layering of `fs.overlay` needed no algebra change.** The earlier per-slot-types plan (a
    named `types` import per slot) is not expressible in WIT text — an import item cannot re-bind its `use`
    dependencies to a chosen sibling import — so the owner approved moving the root-handle resource into the
    `fs` interface itself (plan/02 Decision 15). With that, `rename`/`with`/`compose` wire two independent
    fs leaves into the overlay's `upper`/`lower` slots unchanged: each named import mints its own root-handle
    type, the existing slot-name matching does the rest, and the previously failing construction
    (`with memfs-A as upper, memfs-B as lower $ fs.overlay $ readwrite`) now composes, encodes, and
    validates (covered by `tests/eo9-integration/tests/overlay.rs`). The describe surface gained an
    `authority_free` flag on `ImportNeed` (computed structurally: an imported interface with no functions),
    which the CLI/embed `requires_fs` checks now respect.

15. **Configured interposition (middleware-over-provider, both configured) still traps — root cause
    characterized, fix deferred.** The PL user-study finding 1 (`time.frozen --… $ time.fuzzy --… $ hello`
    and the `&` form trap; each provider works alone; the unconfigured chain works) is the configuration
    binder's gate: it async-lowers the provider's `configure` and requires eager completion, and a
    `configure` that itself calls through another composed provider (time.fuzzy's `configure` obtains the
    underlying clock handle from the layer below) does not complete eagerly under wasmtime 45 in that
    nesting. The gate cannot simply wait: it runs inside synchronously-lifted forwarders, and a sync-lifted
    task may not block (verified empirically — a park-and-wait gate produces "cannot block a synchronous
    task before returning", and sync-lowering the `configure` call instead breaks the previously-working
    single-binder cases). The real fix is making the binder fully event-driven (async-callback lifts for
    every forwarder with a two-phase configure-then-forward state machine that saves the original call's
    arguments across the wait) or runtime-level support; both exceed this pass. Recorded as an ignored
    regression test (`tests/eo9-integration/tests/interposition.rs`, the two configured cases) with the
    plain-default chain kept active as a guard; the shape stays listed in GAPS until the binder rework.
16. **Compose diagnostics, the split-identity wiring rule, and executable bytes.** Three changes from the
    same study: (a) `compose` no longer wires a types-only (authority-free) import from a provider that
    does not also satisfy the package's authority interface — wiring just the types splits the package's
    nominal resource identity between two implementers and the encoded composition fails validation (the
    `X.none $ consumer` shape; `time.none`/`text.none`/`entropy.none` were still affected after the fs
    move). Such imports stay residual, which is what the drop law wants anyway. (b) `compose_checked` is the
    new entry point reporting `ComposeWarning::ProviderExportsUnused` when a provider contributes nothing
    (the spec-promised dead-layer warning; `compose` keeps its signature and discards warnings), and a
    provider that offers only `X-config` for a required `X` is refused with an "apply `configure(…)`" hint
    (the SPEC export-shape rule). Surfacing the warning in eosh/the CLI needs the host-side exec WIT to
    carry it — follow-up for areas 02/04/10. (c) `Component::executable_bytes()` strips the purely
    descriptive `implements` extern-name annotations so a renamed-but-residual slot (e.g.
    `rename eo9:time/time wallclock $ hello`) yields an artifact the pinned runtime can parse; `bytes()`
    keeps the annotation so describe/round-tripping stay lossless. The runtime/CLI/kernel compile paths
    should adopt `executable_bytes()` (one-line change each, outside this area) — until they do, running a
    renamed-residual artifact still fails with the parse error. Covered by
    `tests/eo9-integration/tests/compose_diagnostics.rs` and the corpus soundness test below.
