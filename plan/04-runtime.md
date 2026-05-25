# 04 — Runtime (`crates/eo9-runtime`)

## Scope
The privileged half of execution on the usermode path: Wasmtime embedding, the Compile and Task API host
implementations, the host side of the Component Model async ABI (completion queues + doorbells), fuel
accounting, WAVE argument/outcome handling. This is where "the TCB" lives in usermode.

## Spec references
"Execution APIs" (all bullets), "Performance" (fusion, scheduling TODO), "Security" (TCB), WASM runtime async
note, "The module store and compilation cache" (consumer of plan 06).

## Deliverables
- `eo9-runtime` crate:
  - Pinned Wasmtime `Engine` configuration: components enabled, CM async enabled, **fuel metering on**,
    deterministic options (NaN canonicalization etc. — document what's available).
  - `compile(component, opts) -> Image`: Wasmtime precompilation; `opts` = debug info, opt level, target
    (cross-compilation kept as a dev/bootstrap convenience; the kernel carries its own compiler, so structure
    the compile path so its core is reusable under no_std + alloc and coordinate with plan 12 early on which
    wasmtime/cranelift crates build for such targets). Deterministic output is a goal;
    verify and document what Wasmtime guarantees, escalate gaps (the store cache depends on this).
  - Task host: `spawn(image, args, limits)` (memory ceiling via Wasmtime's limiter), `resume(task, fuel)`
    (donate-and-run, returns out-of-fuel | blocked | done), `runnable`/`wait`/`kill` as futures,
    fuel conservation accounting (a task can only donate fuel it was donated).
  - Host-side async: per-task completion queue + edge-triggered doorbell; provider host calls (plan 08)
    complete into it; parked guest waitable-sets wake from it. This is internal machinery, not API.
  - Linker assembly: wire a task's imports from (a) fused composition (already in the component),
    (b) root host providers (plan 08), erroring on anything unsatisfied (loader rule from the spec).
  - WAVE (via `wasm-wave`): parse `args` against `main`'s signature, render `program-outcome`; map traps and
    kills into the outcome rendering.
  - Kill/linearity contract: outstanding host ops complete or abort in the provider; results to a dead task
    are dropped; buffers never dangle.
- Expose the `eo9:exec` interfaces to guests (component-algebra via plan 03, compile/task via the above) so
  eosh and supervisors are ordinary guest programs.

## Key risk (spike first)
Wasmtime's CM-async support is experimental. Milestone 1 is a spike: a guest component awaits a
`future<T>` returned by a host function, completed from another thread, under fuel metering, and a
`resume`-style fuel-bounded call works. Report exactly what the pinned version can and cannot do; if the
async ABI path is too immature, propose the smallest fallback (e.g. host-side polling shim behind the same
WIT) to the planner rather than inventing a parallel mechanism.

## Dependencies
01, 02, 03 (algebra), 05 (scheduler integration for run queues on the usermode path). Consumed by 08, 11, 12
(the AOT artifacts), 13.

## Milestones
1. Async + fuel spike (above); `run` a hello component with host-provided text/time.
2. Full task surface (spawn/resume/runnable/wait/kill + limits) with tests; WAVE args/outcomes.
3. Compile cache hook-up with plan 06; no_std+alloc compile-core coordination with plan 12.

## Decisions

### D1. Spike findings: what wasmtime 45 component-model-async can and cannot do

Everything below was established empirically against the pinned `wasmtime 45.0.0`
(default features + `wave`; engine config in `crates/eo9-runtime/src/engine.rs`) and is
exercised by `crates/eo9-runtime/tests/spike_cm_async.rs`.

**Works:**
- **Host-created `future<T>`.** A host function can return a WIT `future<T>` by building a
  `FutureReader` from a `FutureProducer` (any `Future` works); the producer is polled from
  the store's event loop and a completion from another thread wakes the embedder's waker.
  A guest awaits it with sync-lowered call → `future.read async` → `waitable-set.wait`,
  and the value arrives correctly. No async runtime is needed anywhere: the embedder can
  drive `Store::run_concurrent` with a hand-rolled waker (our per-task doorbell).
- **Async (stackful) lifts.** `canon lift … async` without a callback works (needs the
  `CM_ASYNC_STACKFUL` feature, enabled in our engine config), with `task.return` delivering
  the result. Blocking builtins are allowed from such exports.
- **Fuel metering** (deterministic counting, `consume_fuel` + per-store yield interval),
  instantiation, resource imports, typed host functions, memory limits via
  `ResourceLimiter`, and dynamic (`Val`-based) calls via `Func::call_concurrent` all work.

**Does not work / constraints:**
- **Sync-lifted exports cannot block.** Any potentially-blocking canonical builtin
  (`waitable-set.wait`, blocking reads, sync-lowered calls to async callees) traps with
  `CannotBlockSyncTask`. In this wasm-tools/wasmtime generation async-ness is part of the
  *component-level function type*, so a binary whose `main` awaits anything must export
  `main` as an **async function**. → escalation E1 below (WIT `main` should be `async func`).
- **Fuel yields are not resumable suspension points for the embedder.** A fuel yield
  suspends the executing fiber *in place*, held by the in-flight `run_concurrent` poll: it
  is not parked in the store. Dropping that future disposes the fiber (guest dies with
  "future dropped"), and while it exists the store is mutably borrowed, so fuel cannot be
  added or read between donations. Blocking on waitables, by contrast, parks the fiber in
  the store. Consequence: the literal "set fuel to the donation, run, stop at exhaustion,
  re-donate later" shape is not implementable on wasmtime 45 without upstream changes.
- **Imported instance types must use wit-component's encoding** (resource `eq` re-exports,
  named type exports); hand-rolled import types with anonymous local named types are
  rejected ("instance not valid to be used as import"). Only affects hand-written WAT
  guests; wit-bindgen output (area 07) is already in the right shape.

### D2. Resume semantics: the fuel-quantum shim (flag for the planner)

Because of the fuel-yield limitation above, `eo9-runtime` implements `resume(task, fuel)`
behind the unchanged `eo9:exec/task` surface as follows (see `src/task.rs` module docs):
one **long-lived drive future per task** owns the `Store` for the task's life; the store is
given an effectively-infinite fuel pool and a **fixed yield quantum** (`FUEL_QUANTUM`,
currently 10 000) at spawn; `resume` converts the donation into quanta and polls the drive,
counting each synchronously-woken `Pending` as one quantum consumed; it stops polling when
the donated quanta are spent (out-of-fuel — genuinely suspended and resumable), returns
blocked when a poll stalls without a synchronous wake, and done when `main` completes.
Consequences, all deliberate and revisitable:
- fuel accounting is **quantum-granular**: sub-quantum remainders are carried, a resume
  that ends blocked/done under-charges by at most one quantum, and a cooperative guest
  yield (not currently expressible through eo9 WIT) would be charged like a fuel yield;
- the quantum is fixed per task at spawn, not per donation;
- `Store::get_fuel` introspection is unavailable while the task is alive.
This is the smallest shim that preserves the spec's donate-and-run model; the clean fix is
upstream (make fuel exhaustion park the task in the store like a waitable wait, or allow
fuel adjustment on a suspended store) and should be raised with the planner before I3.

### D3. Engine configuration (the pinned TCB knobs)

Components + CM async (+ stackful, + more-async-builtins) on; **fuel on**, epochs off;
NaN canonicalization and deterministic relaxed-SIMD on; shared-memory threads off; fixed
Cranelift opt level (Speed); parallel compilation off; wasm backtraces left on for trap
diagnostics. Codegen determinism is *configured for* but not yet *verified* bit-for-bit;
verifying `Component::serialize` stability across runs/machines is still open with area 06.

### D4. Dependencies and the WAVE implementation

`eo9-runtime` uses workspace `wasmtime` with its default features plus the non-default
`wave` feature, and no other new dependencies. WAVE parsing/rendering goes through
`wasmtime::component::wasm_wave` (wasm-wave 0.248 pinned inside wasmtime) rather than the
root-pinned wasm-wave 0.250, because the `WasmValue` impls for `component::Val`/`Type`
live inside wasmtime — same accepted internal-skew as the rest of wasmtime's 0.248 family.
Follow-up for area 01: making the root pin entry `default-features = false` would let this
crate drop the unused default features (gc, profiling, cache, …) from the TCB build.

### D5. Provider trait surface (for area 08)

`providers.rs` defines the host-side seam: `TextProvider` (write, read-line),
`TimeProvider` (now, monotonic-now, resolution, sleep), `EntropyProvider` (get-bytes,
get-u64), mirroring `wit/text`, `wit/time`, `wit/entropy`. Sync WIT functions are sync
trait methods; WIT `future<T>`-returning functions return `BoxOp<T>` (a plain boxed
`core::future::Future`, no executor or runtime types): the runtime polls it from the
task's event loop and the waker it passes **is** the task's doorbell — complete the
operation anywhere and wake that waker. Providers are per-task values handed to `spawn`;
absent providers mean the corresponding interface is simply not linked (the loader rule),
so capability absence stays a composition concern (`text.none`), not a host stub.
In-memory providers (`CaptureText`, `FrozenTime`, `SeededEntropy`) live in this crate for
tests; `eo9-providers-unix` replaces them on the usermode path at integration.

### D6. Task API mapping and kill/linearity

`Task::spawn` = instantiate against root providers (unsatisfied import ⇒ spawn error),
WAVE-parse/type-check `args` against `main`'s signature, queue the call; nothing runs until
the first `resume`. `runnable` is exposed as `is_runnable()` plus a doorbell-backed future;
`wait` is `outcome()` for now (a future-returning form comes with the guest-facing exec
interface). `kill` drops the task's store: in-flight provider ops are dropped (the
provider's `Drop` aborts or completes the underlying work) and results to the dead task go
nowhere. Host-side `Outcome` has `Trapped`/`Killed` arms beyond the WIT `program-outcome`
(escalation E2). Instantiation cost is not charged against resume donations (it is bounded
and paid at spawn). Memory ceilings are enforced at `memory.grow` via a `ResourceLimiter`.

### D7. Deferred (not in this milestone)

Exposing `eo9:exec` (compile/task/component-algebra) *to guests* so eosh/supervisors can be
ordinary programs; image serialization and the compile-cache hook-up (plan 06); the
no_std+alloc compile-core split (plan 12); `wait`/`kill` as guest-visible futures; the
multi-core "one scheduler at a time" rule; a guest-driven test of `read-line`'s future
path (the host wiring exists; `sleep` covers the mechanism).

### D8. Hardening follow-up (branch `area/04-security-hardening`)

Three resource-exhaustion fixes from security review, tests in `tests/hardening.rs`:
`eo9:entropy/entropy.get-bytes` requests are capped at 64 KiB per call before any host
allocation (oversized requests trap the task; the WIT has no error case to report them —
fold into escalation E2 if one is wanted); `spawn-limits` gained `max-table-elements`
(host-side only for now) and a memory-limited task gets a derived table ceiling
(`max-memory / 8`) so `table.grow` is never unbounded alongside a memory cap; and the
instantiate phase of `spawn` runs on a small fixed fuel budget (4 quanta, no yield
interval), so start-time code in a component fails the spawn instead of burning unbounded
CPU before any fuel was donated.

### D9. Milestone-2 follow-up (branch `area/04-runtime-m2`)

**Image serialization for the compile cache.** `Image::serialize` and (unsafe)
`Image::deserialize` expose wasmtime's precompiled-component bytes so the usermode cache
can skip codegen on a hit; `engine::compatibility_hash` gives area 11 the engine half of
the cache key (wasmtime version + target + compile-relevant config — for Eo9, the
`EngineOptions`). Deserialization verifies engine/version compatibility but is *not* an
integrity check: the bytes are native code, so they must only come from the trusted store
with their content hash verified — hence the `unsafe` contract, to be wrapped by area 11.
Round-trip and rejection tests live in `tests/image_cache.rs`.

**fs / io-buffer linking: on hold.** Work on wiring `eo9:io/buffers` and `eo9:fs/fs` into
the runtime (provider trait + linker, so `eo9-example-readwrite` runs end to end) was
paused on the planner's instruction while the WIT shape of the API operations
(`func(...) -> future<T>` vs `async func(...) -> T`) is under owner review; only an
uncommitted sketch of the host-side `FsProvider` trait existed and was dropped. One
runtime finding feeds that review: under wasmtime 45 a *sync-lifted* `main` cannot block
on waitable-sets (`CannotBlockSyncTask`, spike finding D1), and `wit-bindgen 0.57`'s
`block_on` waits exactly that way — so changing the operations to `async func` does not by
itself make wit-bindgen guests runnable; `main` (or the guest SDK's wait strategy) must
become async-capable too (escalation E1).

### Escalations for the planner

- **E1 (wit/, area 02 + 07):** for a binary to await anything, its `main` must be an
  `async` function at the component level under wasmtime 45. Proposal: declare `main` (and
  probably provider `configure`) as `async func` in the WIT sketches/worlds, and have the
  guest SDK lift accordingly.
- **E2 (wit/, area 02):** `program-outcome` has no arm for abnormal termination (trap,
  kill, out-of-fuel death). `wait`/`kill` return `future<program-outcome>`, which today
  cannot express "killed". Proposal: add a third arm (e.g. `aborted(abort-reason)`).
- **E3 (upstream / planner):** the resume shim of D2 and whether to pursue a wasmtime
  change (or carry a patch) for store-parked fuel yields before milestone I3.
- **E4 (area 01):** root pin entry for wasmtime could become `default-features = false`
  so the runtime can opt out of unused default features in the TCB build.
