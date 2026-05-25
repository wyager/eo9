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
(record here)
