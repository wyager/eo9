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
