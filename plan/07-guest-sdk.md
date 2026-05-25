# 07 — Guest SDK & example programs (`guest/eo9-guest`, `guest/examples`)

## Scope
Everything needed to write Eo9 programs and providers in Rust comfortably: generated bindings for the
`eo9:*` WIT packages, an async story on top of the Component Model ABI, outcome/argument helpers, and the
example programs used by every other area's tests.

## Spec references
"WASM runtime" (worlds, arguments vs imports, outcome types), "Eo9 API design" (futures), "Execution APIs"
(one concurrency vocabulary), capability accessor convention.

## Deliverables
- `eo9-guest` crate:
  - `wit-bindgen`-generated bindings for the eo9 packages (re-exported per API module).
  - Async shim: make `future<T>` awaitable from guest Rust (use wit-bindgen's CM-async support if the pinned
    toolchain has it; otherwise the thinnest possible wrapper — escalate findings, keep aligned with plan 04's
    spike).
  - Helpers: buffer round-trip wrappers, `default()` accessor wrappers, a `main!`-style macro that maps a
    Rust function onto the world's `main` export and its success/failure variants.
  - Guest profile decision: default to `no_std + alloc` so components import **only** `eo9:*` interfaces (no
    hidden WASI capability imports). Provide the allocator + panic handler. If this proves too painful,
    propose the fallback (std + a WASI shim provider) to the planner — it has capability-model implications.
- `guest/examples`:
  - `hello` (text + time), `outcomes` (exercises success/failure variants and bad-arguments),
    `readwrite` (fs), `cruncher` (pure compute + fs, for `only` demos), `sleepy` (time futures),
    `many-reads` (hundreds of concurrent disk reads — plan 04/13 concurrency tests), `netcat-lite` (net).
  - Each example documents the world it targets; examples dual as conformance fixtures.
- Build flow per plan 01 (cargo-component or wit-bindgen+wasm-tools), wired into xtask.

## Dependencies
01, 02. Consumed by 09, 10, 13, and every integration milestone.

## Milestones
1. `hello` builds as a component and runs under plan 04's milestone-1 runtime (integration milestone I1).
2. Async shim + `many-reads`; fs/net examples.
3. Provider-authoring support (export an API interface + `configure`) — needed by plan 09.

## Decisions
(record here)
