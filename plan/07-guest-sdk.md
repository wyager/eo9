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

1. **Guest profile: `no_std + alloc`, confirmed workable.** Every guest crate is `#![no_std]` +
   `extern crate alloc`; the SDK's `rt` module provides the global allocator and a panic handler that
   lowers to the wasm `unreachable` instruction (the host sees a trap; no capability is needed to report a
   panic). The allocator is `dlmalloc` (feature `global`) — the same allocator rustc's own `std` uses on
   wasm targets — added to the guest workspace pins (a dependency beyond the approved foundation list;
   **approved by the planner**). `wit-bindgen` is taken with `default-features = false` and features
   `macros, realloc, bitflags, macro-string, async` (dropping its `std` feature; `async` is dependency-free
   and no_std-clean).
2. **One shared set of bindings, re-exported per API.** `eo9-guest` runs `wit_bindgen::generate!` once
   against an internal `eo9-guest:sdk/sdk` world that imports every eo9 API, and re-exports the generated
   modules under `eo9_guest::api::{io,text,time,entropy,perf,disk,fs,net}`. Program crates generate only
   their own world (outcome variants, `main`) and map the API interfaces onto those shared modules with
   wit-bindgen's `with` option — done for them by `eo9_guest::bindings!({ world: "...", apis: [...] })`.
   wit-component drops unused imports when componentizing, so the SDK-world metadata never widens a
   program's import list: each component imports exactly the interfaces whose functions it calls.
3. **WIT layout for guest crates.** Each guest crate owns a `wit/` directory containing its world, with the
   repo-level `wit/<api>` packages it imports symlinked under `wit/deps/` — the same convention the
   `wit/` packages themselves use. (Passing several package directories as multiple `generate!` `path`
   entries does not work: each directory's own `deps/` re-adds shared transitive packages, e.g. `eo9:io`,
   and wit-parser rejects the duplicate.) Example worlds live in the example crates under the
   `eo9-examples:` namespace; `eo9:` stays reserved for the standard packages owned by area 02.
4. **Macros.** `bindings!` must be invoked at the crate root and its `apis` list must match the world's
   imports exactly (wit-bindgen errors on both missing and unused remappings, keeping the capability list
   auditable in the source). `main!` implements the world's `Guest::main` from a plain `fn main` with the
   world's typed success/failure variants. Program crates depend on `eo9-guest` and `wit-bindgen` under
   those names. Helpers (`text`, `time`, `entropy`, `buffer`) are stateless one-shot wrappers that fetch the
   root handle via `default()` per call; programs doing repeated I/O hold the handle themselves.
5. **Async story (wit-bindgen 0.57.1) — works in the guest; running it needs host-side CM-async.** Imports
   returning `future<T>` are generated as synchronous functions returning `FutureReader<T>`; awaiting them
   inside `eo9_guest::block_on(async { ... })` uses the Component Model waitable-set built-ins and works
   under no_std. `wasm-tools component new` (1.250) componentizes such modules fine and the result imports
   only the expected eo9 interfaces; validation needs the cm-async feature, so (with planner authorization)
   xtask's build-guest validate step now passes `--features cm-async` and the `readwrite` (fs) example ships
   in `GUEST_COMPONENTS`. Actually *executing* future-bearing components still depends on CM-async support
   in the host runtime (area 04); `time.sleep` / many-reads-style concurrency examples wait on that.
6. **Deferred.** Bindings/helpers for the `-optional` interface flavors; `sleepy`, `many-reads`, and
   `netcat-lite` examples (pending the area-04 async host support above); any `println!`-style formatting
   macros. (Provider-authoring support — milestone 3 — has partially landed; see decision 7.)
7. **Provider-authoring support (milestone 3, first slice — added by area 09).** `eo9_guest::provider`
   provides `ProviderState<T>`, the `static`-friendly cell for a provider's shared state (bound by
   `configure`, read by `default()` and every operation; exported resources are just tokens referring to
   it). Provider crates do **not** use `bindings!`/the shared API modules: exported interfaces must be
   generated locally, so each stub crate calls `wit_bindgen::generate!` directly against its stub world in
   the repo-level `wit/<api>` package. Helpers for operations that return `future<T>` are deliberately
   absent for now: such exports cannot be implemented by a wasm guest provider with the pinned toolchain
   (only `async func` exports may be async-lifted) — see plan/09-providers-stubs.md Decisions for the
   constraint and the escalation.
8. **Mechanical update by area 02 (async operations, branch `area/02-async-operations`):** blocking API ops
   are now `async func` in wit/, so the generated imports are async Rust functions (string/list args by
   value). `main!` gained an `async fn main` arm for worlds with `main: async func`; the readwrite example
   moved to an async `main` and dropped `block_on`; hello/outcomes/cruncher are untouched. The decision-7
   constraint on future-returning exports is now moot for the standard APIs (no ops return `future<T>`
   anymore), which unblocks the deferred async stubs/examples once area 04's host side catches up.
