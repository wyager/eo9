# 02 — WIT interface packages

## Scope
Author the `eo9:*` WIT packages under `wit/`. This directory is the machine-readable half of the spec: every
other area consumes it read-only. Changes after v0 freeze go through the planner.

## Spec references
SPEC.md: "Eo9 API design", "WASM runtime", "The capability algebra", "Capability slots", "Execution APIs",
and each API section under Deliverables.

## Packages (v0, start everything at 0.1.0)
- `eo9:io` — `buffers` (resource `buffer`, constructor, len).
- `eo9:disk` — offset-addressed read/write with owned-buffer round-trip (per spec example; propose renaming
  the spec's `fs-impl` to `disk-impl` — escalate, don't silently diverge).
- `eo9:fs` — MVP surface: open/read/write/list/stat, `open-exec -> immutable-handle`; content-hash queries
  stubbed as TODO.
- `eo9:net` — MVP surface: TCP connect/listen/send/recv + UDP, owned-buffer round-trip.
- `eo9:text` — std{in,out,err} read/write.
- `eo9:message` — minimal typed channel (send/recv of `list<u8>` + a typed envelope later). Keep tiny.
- `eo9:entropy`, `eo9:time` (wall + monotonic), `eo9:perf` (placeholder only).
- `eo9:exec` — `component-algebra`, `compile`, `task` interfaces as sketched in SPEC.md (import-need record,
  spawn-limits, resume/runnable/wait/kill, program-outcome).
- `eo9:sandbox` — policy worlds (`pure`, `no-net`) as import-only worlds.
- Per-API **stub worlds** (the wasm ones): `none` for every API; `deny`/`loopback`/`memfs`/`readonly`/
  `frozen`/`monotonic-stub`/`fuzzy`/`seeded`/`null`/`mem` per the "Standard stubs" lines in the spec.
  (Implementations live in plan 09; the worlds live here.)

## Conventions to encode (from the spec)
- Every API interface: root `resource X-impl` + `default: func() -> X-impl` accessor; ops take
  `borrow<X-impl>`.
- Every API gets a hand-written `-optional` interface flavor (`default -> option<X-impl>`). Write them by
  hand for MVP; add a CI consistency check (flavor matches base interface) rather than a generator.
- Owned-buffer round-trip for disk/net; buffer returned on success and error.
- Per-API error variants; `denied` cases only where that API has a `deny` stub.
- `future<T>` / `future` per the Component Model async syntax supported by the pinned toolchain.

## Key risks / first tasks
1. Verify what the pinned `wit-parser`/`wasm-tools` accept for: (a) CM async types in WIT, (b) named imports
   of the same interface under two slot names (`import scratch-fs: eo9:fs/fs@0.1.0`). Report findings to the
   planner immediately — these two determine whether the spec's dialect needs adjusting or a small
   preprocessing step.
2. Keep every package `wasm-tools component wit` round-trippable; add that as a CI check.

## Dependencies
01. Consumed by every other area.

## Milestones
1. `eo9:io`, `eo9:text`, `eo9:time`, `eo9:entropy`, `eo9:exec` validate (enough for integration milestone I1).
2. `eo9:disk`, `eo9:fs`, `eo9:net`, stub worlds, `eo9:sandbox`, `-optional` flavors (enough for I2).
3. `eo9:message`, remaining TODO surfaces; v0 freeze.

## Decisions

Toolchain findings (wasm-tools 1.250.0, wit-bindgen-cli 0.57.1):

1. **Async types (risk a): supported.** `future<T>`, bare `future`, and `stream<T>` in interface function
   results parse, print, encode to binary, and round-trip exactly as the spec's disk sketch writes them.
   Binary validation needs the `cm-async` feature (`wasm-tools validate --features cm-async`); the default
   feature set rejects with "`future` requires the component model async feature". wit-bindgen 0.57.1
   generates Rust for worlds using `future` without extra flags. `own<buffer>` is accepted as input but
   normalizes to bare `buffer` (own is the default for resource params/results); `own` is also a reserved
   word (unusable as an identifier). The authored packages write `buffer` directly.
2. **Named slots (risk b): supported as written.** `import system-fs: eo9:fs/fs@0.1.0;` plus
   `import scratch-fs: eo9:fs/fs@0.1.0;` in one world validates and round-trips. No spec adjustment needed.
3. **Layout.** One directory per package under `wit/` (a directory may contain only one root package;
   a flat dir of peer packages or a file of only braced packages is rejected). Cross-package references
   are resolved via `deps/` symlinks to sibling package dirs (e.g. `wit/disk/deps/io -> ../../io`).
   `wit/check.sh` runs parse → binary encode → validate (`--features cm-async`) → round-trip per package.
4. **Root resource lives in a per-package `types` interface** (`eo9:X/types { resource x-impl }`), not inside
   the API interface as originally sketched. Reason: with the in-interface encoding, any world importing
   `X-optional` (or a `none` stub exporting it) is elaborated by wit-parser to also import the *full
   required* `X` interface, because `use X.{x-impl}` drags the owning interface in — defeating the
   optional-capability and `X.none` sealing semantics. With the split, optional importers pick up only the
   authority-free `eo9:X/types` import. Accessor pattern (`default()`), `borrow<x-impl>` ops, and the
   `-optional` flavor are otherwise exactly per spec. **Accepted by the planner/owner; SPEC.md now uses
   this pattern.**
5. **Same move in eo9:exec:** `resource image` lives in a types-only `images` interface so importing `task`
   does not implicitly import the `compile` authority. `component` stays in `component-algebra` per the
   sketch (it is unprivileged, so the implicit import is harmless).
6. **Keyword collisions** (tooling-forced spellings): `import-need.interface` → `%interface`;
   component-algebra `rename` takes `old-name`/`new-name` (`from` is a keyword); fs directory listing is
   `list-directory` (`list` is a keyword).
7. **Stub worlds** live as sibling worlds in each API package. Self-contained stubs (`none`, `deny`, `memfs`,
   `mem`, `loopback`, `frozen`, `monotonic-stub`, `seeded`, `null`, `capture`) export `types` + the API so
   they have zero `eo9:*` imports; attenuating stubs (`fs.readonly`, `disk.readonly`, `time.fuzzy`) import
   and export the API (shared handle types with the underlying provider). Configurable stubs export an
   inline `configure: func(...) -> result<_, string>` (seeded: seed; frozen/monotonic-stub: start/step;
   fuzzy: granularity; disk.mem: size).
8. **Additions beyond the spec sketches (escalated, then accepted):** `buffer.read`/`buffer.write`
   byte accessors on `eo9:io/buffers` (without them guests cannot move data at all); a `rename` function in
   `component-algebra`; minimal invented shapes for types the spec references but does not define
   (`component-info`, `interface-ref`, error variants, `compile-opts`, `named-arg`, `wave-value`,
   `program-outcome` as WAVE value + WIT type text) — these will be absorbed into the spec as they firm up.
9. **Policy worlds** (`eo9:sandbox`): `pure` is the empty world; `no-net` imports the base flavor of every
   standard API except net (an entry admits both flavors per the spec rule, enforced by area 03), including
   the exec interfaces — `only` restricts, it never grants.
10. **Escalations — planner/owner rulings applied:**
    (a) eo9:disk root resource renamed `fs-impl` → `disk-impl`; eo9:disk is raw block-device access only
        (no filesystem semantics — paths/metadata/hashes are eo9:fs's domain), docs updated to match.
    (b) decision 4's types-interface encoding: accepted, spec updated.
    (c) buffer byte accessors: accepted, kept.
    (d) `task.spawn` now takes `borrow<image>` — one cached image, many spawns.
    (e) exec interfaces get no `-optional` flavors / stub worlds: confirmed, unchanged.
    (f) `import-need` gained a `slot` field (slot name, defaulting to the interface name).
    (g) eo9:message remains deferred to milestone 3 per the plan.
11. **Follow-up batch (SPEC commits 133898c, e5be983):**
    (a) `program-outcome` is now the flat three-way variant `success(wave-value) | failure(wave-value) |
        abnormal(abnormal-exit)` with `variant abnormal-exit { trapped(string), killed }`; `task.wait`/`kill`
        docs updated (kill typically resolves `abnormal(killed)`).
    (b) `internal(string)` added to `rename-error` and `restrict-error` (mirrors area 03's Rust API).
    (c) Entrypoints are async: every provider/stub `configure` is `async func`; the eo9:exec package doc
        notes that binary `main` exports are `async func` by convention. Interface ops that already return
        `future<T>` were left as they are (only entrypoints were in scope).
    (d) `configure` returns the provider's root handle. **Encoding note:** a bare world-level
        `export configure: async func(...) -> result<x-impl, …>` cannot express this correctly — a
        world-level `use types.{x-impl}` always binds to an *import* of `eo9:X/types`, and a component
        cannot mint handles of an imported resource type (verified: wit-bindgen generates an
        unimplementable signature, and the stub world stops being self-contained). So `configure` lives in
        a small per-world config interface (`eo9:X/<world>-config`, e.g. `eo9:entropy/seeded-config`)
        exported alongside `types` and the API; all exports then share the provider's own resource type and
        `default()` hands out exactly the handle `configure` returned. Every world exporting a required
        interface now has a config interface (nullary where there is nothing to configure: deny, readonly,
        memfs, null, capture, loopback); `.none` stubs are unchanged. Flagged for the planner: the spec's
        surface form shows `configure` as a bare world export — either the spec adopts the config-interface
        encoding or the handle-returning form needs a different mechanism.
12. **Async operations (SPEC commit c11ca7a, branch `area/02-async-operations`):** every operation that
    returned `future<T>` is now `async func(...) -> T` — disk read/write (param renamed `d` → `dev` to match
    the spec sketch), all fs path/file/immutable-handle ops, all net TCP/UDP ops, text `read-line`, time
    `sleep`, and eo9:exec/task `runnable`/`wait`/`kill` (`runnable: async func(t)` with no result); `resume`
    and `spawn` are unchanged, as are already-sync ops (`len`, `now`, `get-bytes`, `exec-size`, `write`, …).
    No `future<T>`/`stream<T>` value types remain in any eo9 package. Verified with the pinned toolchain:
    `async func` with `borrow<…>` parameters and resource results validates, and wit-bindgen 0.57.1
    generates async Rust imports (`pub async fn op(…)`) and guest-implementable async trait methods for
    exports — which is what unblocks wasm guest providers for the blocking ops (plan/09's constraint).
    One binding-level consequence: async imports take `string`/`list` arguments by value rather than by
    reference (one-line call-site changes in eosh and readwrite). Mechanical cross-area updates made under
    planner authorization: eo9-guest (`main!` gained an `async fn main` arm; block_on doc), readwrite
    example (async `main`, no more block_on), eosh component (one owned-String call site). The runtime and
    integration-test crates embed their own WIT copies and were left untouched — area 04 reconciles the
    host side (async-lifted task.wait/kill/runnable and provider ops) when it syncs its copy.
13. **`configure` as a component-algebra operation (SPEC commit bd74868, branch `area/02-configure-op`):**
    added `configure: func(p: component, args: list<named-arg>) -> result<component, configure-error>` to
    `eo9:exec/component-algebra` — binds a provider's exported config interface with constant, WAVE-typed
    argument values and returns an already-configured provider that re-exports only the API (the config
    interface is no longer visible); invalid or ill-typed arguments fail at compose time. `configure-error`
    cases: `not-a-provider`, `no-config-interface`, `invalid-args(string)`, `internal(string)`. The shared
    argument vocabulary (`arg-spec`, `named-arg`) lives in a record-only `eo9:exec/args` interface (the
    same types-only convention as `eo9:X/types` and `images`), `use`d by both `component-algebra` and
    `task` — so a world importing `task` picks up only the authority-free `args` vocabulary, not
    component-algebra; wit-bindgen keeps `arg-spec`/`named-arg` visible as `use` aliases inside both
    interfaces, so existing guest code compiles unchanged. Host-side implementation of the new function
    falls to areas 03/04; eosh's generated bindings already require it.
14. **`eo9:pci` — standardized PCI / PCI Express device API (owner request, branch `area/02-pci`).**
    New package `wit/pci` with the usual shape: types-only `types`, the `pci` interface, `pci-optional`,
    per-world config interfaces, and worlds `none` / `deny` / `filtered`. Shape decisions and reasoning:
    (a) `device-address` (segment/bus/device/function) lives in `types` next to `pci-impl` so
        `filtered-config` can name it without dragging the authority-bearing `pci` interface into the
        world's imports — same motivation as decision 4, extended to a pure-data record.
    (b) Configuration-space and BAR register access is **width-explicit** (`access-width:
        byte|word|dword|qword`, values zero-extended/truncated through u64) rather than owned-buffer:
        register access is width-sensitive (a 4-byte register is not four 1-byte writes), so the width is
        part of the request. There are no bulk-data operations in v0, so `eo9:pci` has **no `eo9:io`
        dependency**; a bulk BAR read/write with the owned-buffer round-trip can be added later if a
        real driver wants it (framebuffers).
    (c) Interrupts: `enable-interrupts(kind, count) -> list<interrupt>` (one resource per vector; intx is
        exactly one; dropping every handle disables delivery) and a repeated `wait(borrow<interrupt>) ->
        u64` that returns the number of deliveries coalesced since the previous wait — the same
        repeated-await convention as `net.accept`/`task.wait`, per decision 12 no `stream` value types.
    (d) DMA: `alloc-dma` hangs off the `device` (not the root) because the mapping — and the IOMMU domain,
        where one exists — is per-device and the returned bus/IOVA address is only meaningful for that
        device. `dma-buffer` exposes `dma-address`/`dma-len` plus `dma-read`/`dma-write` copy accessors
        that mirror `eo9:io` buffer's accessors (including trap-on-out-of-bounds); how a `dma-buffer`
        relates to `eo9:io` buffer (e.g. a NIC driver handing DMA'd packets up as buffers) is an open
        question flagged to the planner.
    (e) Device control is explicit where it is security-relevant: `set-bus-master` and `reset` are
        first-class ops (auditable, deniable by an attenuator) even though they are expressible as raw
        config-space writes; a provider that gates DMA must also filter command-register config writes.
    (f) Worlds: `none` (absence — universal), `deny` (refusal is meaningful for PCI), and `filtered`
        (attenuator: `configure(allow: list<device-address>)`, only listed devices are enumerable/openable,
        everything else answers `denied`) — `filtered` is how "exactly this one device" grants are
        composed, which is the security posture for a capability that implies DMA. Stub implementations:
        `pci.none` shipped under `guest/stubs/pci-none` (area-09 conventions, registered in
        `GUEST_COMPONENTS`); `pci.deny`/`pci.filtered` implementations are deferred to area 09.
    (g) `wit/check.sh` now validates `pci`; no existing package changed. SPEC.md has no PCI section yet —
        a proposed paragraph went to the planner with this decision.

15. **`fs-impl` moved from `eo9:fs/types` into `interface fs`; the `types` interface is gone (owner-approved
    convention change; SPEC "Multi-instance imports and type identity", branch `area/02-fs-impl-in-interface`).**
    (a) Rationale: resource identity is per *exporting instance*, so a root handle declared in a shared
    types-only sibling forces every named import of `fs` to share one mint — which made two-slot consumers
    (`fs.overlay`) ill-typed against independent leaves. Declaring the resource in the API interface itself
    makes every named import mint its own root-handle type, exactly like the interface-local `file`/
    `immutable-handle` resources already did. (b) `fs-optional` and the `*-config` interfaces now `use
    fs.{fs-impl}`; the `none`/`deny`/`memfs` worlds no longer export a types interface; consumers that only
    name the handle (fs.none, optional consumers) acquire a *types-only* import of `eo9:fs/fs`, which
    `describe` classifies as authority-free (structurally: no functions) and the runtime satisfies
    unconditionally with a resource-only `eo9:fs/fs` linker instance. (c) The CLI/embed `requires_fs`
    pre-checks now skip authority-free imports (closing the old GAPS nit about types-only fs imports
    demanding a grant); eosh's `envinfo` still classifies by the `/types`-suffix heuristic — no in-tree
    program is affected (none import `fs-optional` yet), noted for a later pass. (d) text/time/entropy/disk/
    net/pci keep their types-only siblings for now; migrate them opportunistically when they gain a
    multi-instance consumer, using this decision as the template.

16. **`configure` is synchronous (owner ruling 2026-05-27, the bug-1 fix).** Every `*-config` interface's
    `configure` is now a plain `func`, not `async func`. Rationale: `configure` binds compile-time constants
    and must not block or perform I/O, so async bought nothing — and it made nested-configured compositions
    untypable (a sync caller cannot wait on a parked async `configure`; plan/03 D17). Making it sync removes
    the gamble: the binder sync-lowers the call and a configured provider's `configure` may synchronously
    reenter another's. Changed: the `configure` signature in entropy/time/fs/disk/net/pci/text/perf
    `*-config` interfaces (the `eo9:exec/component-algebra.configure` *operation* was already sync and is
    unrelated); the 10 stubs that export a `configure` dropped `async fn`; the binder's configure-call codegen
    (plan/03 D18); the `det_env_guest` WAT fixture's three config imports. SPEC's `main`/`configure` paragraph
    now declares `configure` synchronous and "a wart we keep small on purpose." A provider that ever needs to
    do something dynamic does it lazily in its API, never in `configure`.
