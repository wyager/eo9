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
9. **wit-bindgen bump for `fs.overlay`: investigated, not possible yet (no qualifying release exists).**
   The `fs.overlay` world needs a *named import of a foreign interface* (`import upper: eo9:fs/fs@0.1.0`,
   twice for `upper`/`lower` — see plan/09 D11 on `area/09-fs-overlay`), and the planned unblock was to bump
   the guest `wit-bindgen` pin to a release whose bundled `wit-parser` can parse it. Verified findings
   (2026-05-27):
   * The pinned wit-bindgen 0.57.1 bundles the **0.247** wasm-tools family; its text grammar has no
     `ExternKind::NamedPath` (`import <id>: <pkg:iface>` falls into the `UsePath` arm and fails with
     "expected `/`, found `:`"), and its binary decoder rejects the new named-import-name encoding
     ("invalid leading byte (0x2) for import name"), so feeding it a 1.250-encoded binary WIT package does
     not work either (probed with a scratch crate against the elaborated `overlay` world).
   * The feature landed in the **0.249** wasm-tools family (`ExternKind::NamedPath` is present in
     wit-parser 0.249/0.250; absent in 0.247/0.248).
   * **wit-bindgen 0.57.1 is the newest published release** (crates.io has nothing newer as of 2026-05-27;
     the Artifactory mirror agrees), so there is no release to bump to; wit-bindgen's git `main` pins
     wit-parser **0.249** and would work, but that is an unreleased git pin, not a release.
   Options, in preference order: (a) wait for the next wit-bindgen release (≥ 0.58 will bundle ≥ 0.249) and
   then run the originally-planned bump + full ABI re-validation against the wasmtime-45 host (integration
   suites, CLI transcripts, QEMU smoke — the generated async/callback ABI must keep matching wasmtime 45);
   (b) if `fs.overlay` becomes urgent before that, take a planner decision to pin wit-bindgen to a git rev
   of `main` (dependency-policy change + the same full re-validation); (c) keep `fs.overlay` parked (its
   draft stays excluded on `area/09-fs-overlay`). No pins, sources, or generated components were changed by
   this investigation; the guest workspace still builds with 0.57.1 exactly as before.
10. **wit-bindgen: temporary git pin to upstream main (owner-approved option (b) of decision 9).** The guest
    workspace now pins `wit-bindgen` to upstream main rev `ea49687c8db0c6abb5de9fa3ea3c7c298587c8f3`
    (2026-05-22, "fix: async lifted exports with direct results"), which bundles the **0.249** wasm-tools
    family (wit-parser/wasmparser/wit-component/wasm-metadata 0.249.0) — the first family whose grammar has
    `ExternKind::NamedPath`, i.e. named imports of a foreign interface, which the `fs.overlay` world needs.
    The root Cargo.toml pin-table line mirrors the git pin (no host crate consumes wit-bindgen). **Switch
    back to a crates.io version pin with the first published release whose bundled wit-parser is ≥ 0.249.**
    Validation against the wasmtime-45 host (the one real risk):
    * Zero guest source changes were needed — the whole guest workspace (SDK, 18 stubs, examples, eosh,
      coreutils) compiles unchanged and all 35 components componentize and validate (`--features cm-async`)
      under wasm-tools 1.250.
    * Full `cargo xtask ci` green; explicitly re-ran `deterministic_env` (5/5), `invoker_configured_env`
      (4/4, async-lifted `configure` through the binder), and the CLI transcript suite (31/31, incl. shell,
      env, coreutils, rng). Manual spot checks: `hello`, `readwrite`/`cat` under `--fs-root`, the
      missing-fs refusal, `entropy.seeded --seed 43 $ rng`, `only … $ hello`, and
      `time.frozen --now-seconds … $ hello` (configured frozen clock observed by the program).
    * Bare metal: `build-kernel aarch64` re-AOTs the regenerated components; QEMU `demo` reproduces the
      full sequence (sync seed, hello `success(greeted)`, async sleepy ≈51 ms, entropy.seeded SplitMix64
      values unchanged, on-target Cranelift codegen), and an interactive smoke ran `hello` plus a fused
      `time.frozen --… $ hello` composition compiled on-target — all with the new-bindgen components.
    * `fs.overlay` unblock confirmed: against the extracted draft (branch `area/09-fs-overlay`), `generate!`
      now succeeds for the two named same-interface imports. The generated layout is `crate::upper` and
      `crate::lower` (modules named after the slots), with the shared default-named imports under
      `crate::eo9::…` and exports under `crate::exports::eo9::fs::…`; exported trait methods take
      `FsImplBorrow<'_>` rather than `&FsImpl`. The draft's hand-written forwarding code (written blind)
      needs a mechanical pass against that real layout — area 09's follow-up; nothing was changed on its
      branch.

## Decisions (panic diagnostics)

11. **Trap reasons are cleaned; the panic message needs a per-world post-trap export (2026-05-27).**
    Owner ruling: surface guest panic diagnostics through the existing `abnormal(trapped(reason))` arm, via
    an executor-mediated path (option A) — never a hidden ambient import.
    - *Done now (capability-clean, no WIT):* `crates/eo9-runtime/src/trap.rs` rebuilds the trapped reason
      from wasmtime's already-flowing demangled backtrace — trap kind ("guest panicked — wasm `unreachable`
      …") plus a symbol-only call chain (`… ← core::panicking::panic_fmt ← main`) with code addresses and
      rustc `[hash]` disambiguators stripped, bounded to 16 frames, deterministic across runs/builds. This
      replaces the raw escaped `{err:#}` text the user studies flagged as unreadable.
    - *Still missing — the panic message string and source line.* The `#![no_std]` guest panic handler
      (`guest/eo9-guest/src/rt.rs`) discards `PanicInfo` before the `unreachable` trap, and at the **component
      boundary** the host cannot read them back: a host import would be an ambient capability a guest could
      use for general output (disqualified), and a component does not expose its inner core memory/globals to
      the host, so there is no post-trap memory read either. Mechanism (c) — wasmtime's own trap text — was
      verified to carry the backtrace but **not** the message (the handler threw it away), and `--debug-info`
      adds no source line to the text backtrace.
    - *Proposed WIT (deferred — WIT is contended this wave):* a tiny, capability-clean **export** the SDK
      synthesizes on every world, e.g. `eo9:rt/diagnostics` with `last-trap-reason: func() -> option<string>`.
      The panic handler writes "<message> at <file:line>" into a guest static before trapping; the executor,
      on catching the trap, calls that export (an export grants the guest nothing — it cannot be used for
      proactive output, only read by the executor on the trap path) and folds the result into
      `trapped(reason)`. This is an *export*, not an import, so it does not widen the guest's authority.
      Implement once the configure-sync WIT churn settles.
12. **D11's post-trap export cannot work on wasmtime 45 — blocked, owner ruling needed (2026-05-28).**
    Implementing the proposed `eo9:rt/diagnostics.last-trap-reason` export hit a hard runtime fact: any wasm
    trap sets a **store-level** poison flag (`invoke_wasm_and_catch_traps` → `StoreOpaque::set_trapped`,
    vendored wasmtime `src/runtime/func.rs`), and every component-level call first checks `may_enter`, which
    returns false once the store has trapped (`src/runtime/component/concurrent.rs`), failing with
    `Trap::CannotEnterComponent`. So the executor cannot call *any* export of the task after the panic's
    `unreachable` trap — this is the component model's own "a trapped instance may not be re-entered" rule,
    not a wasmtime quirk, so waiting for upstream is not a plan. Realistic alternatives, each needing an
    owner ruling because each trades against a half of D11:
    (a) a narrowly-scoped **import** `eo9:rt/diagnostics.report-panic(msg)` that only the SDK panic handler
    calls, immediately before trapping; the executor implements it as write-once-into-the-task's-trap-slot
    and surfaces it nowhere except a subsequent `trapped(reason)` — technically an import, but useless as an
    output channel (the host discards it unless the task then traps);
    (b) opportunistically routing the panic message through `eo9:text/write-err` when the world already
    imports text — no new WIT, but the message lands on stderr rather than in the outcome and text-less
    programs stay silent;
    (c) keep the status quo: the cleaned, message-less backtrace from D11's done-now half.
    Nothing implemented for this item this wave (the other two WIT follow-ups — exec `wiring`, eosh
    `program-failure` classes — landed); GAPS keeps the panic-message line open citing this decision.
13. **Panic messages reach `trapped(reason)` via the write-once `report-panic` import (2026-05-28;
    implements option (a) of D12, owner-approved).** The SDK panic handler now formats
    "<message> at <file>:<line>" from `PanicInfo` (re-entrancy-guarded so a panic while reporting skips
    straight to the trap) and calls `eo9:rt/diagnostics.report-panic` before executing `unreachable`. The
    usermode executor keeps a per-task write-once slot (truncated at 1 KiB) and the trap renderer folds it
    into the cleaned reason: `abnormal(trapped("guest panicked: <message> at <file:line> — wasm
    \`unreachable\` …; guest backtrace: …"))`; the kernel mirrors the same slot on `KernelState` and
    prefixes its `trapped` text the same way. Capability posture: this is a diagnostics sink, not an output
    channel — write-once per task, never readable by any guest, surfaced only on the trap path; calling it
    and not trapping has no observable effect, and `only` always admits it (it is part of the runtime
    contract, like the allocator). Every SDK-built component now imports `eo9:rt/diagnostics`; the D11
    "cleaned backtrace" rendering is unchanged underneath. Remaining: the browser blob must register the
    import before the next `/vm` asset rebuild (plan/02 D19), and the kernel's headless `program=` runner
    still prints the raw trap text (its interactive shellexec path carries the message).
