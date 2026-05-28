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

### D10. Milestone-3 follow-up (branch `area/04-runtime-m3`): async reconciliation, fs/io linking, loader rule

**Async-operation host implementations.** With the async-operations migration, blocking
operations are `async func`s end to end: freshly built guest components import them as
async function types and lift `main` with the async callback ABI. The host side now
implements every such operation as a wasmtime *concurrent* host function
(`func_wrap_concurrent`): the returned future awaits the provider's `BoxOp` directly and
the waker that reaches the provider is the task's doorbell. This replaced the
milestone-1-era host-created `future<T>` wiring (`FutureReader` + producer), which no
longer matches what the toolchain emits. One trap worth recording: a stale
`guest/target/components` artifact from before the migration still carried the old
`func(...) -> future<T>` encoding and initially sent this work down the wrong path —
always rebuild guest components (`cargo xtask build-guest`) after a WIT change, since
wit-bindgen's generated bindings do not reliably retrigger cargo rebuilds on `wit/` edits.

**fs / io-buffers linking.** `FsProvider` (open, open-exec, list-directory, stat,
create-directory, remove, owned-buffer read/write/exec-read, exec-size, close hooks) joins
the provider traits, with `MemFs` as the in-memory test provider. `eo9:io/buffers` is
backed by a per-task buffer table in the runtime (buffers are host memory, so they carry
their own caps: 16 MiB per buffer, 64 MiB per task, enforced before allocation); the
owned-buffer round-trip takes the bytes out of the table for the life of an operation and
restores them on completion, success or error. `file`/`immutable-handle` resource drops are
forwarded to the provider. Tests: `tests/fs_api.rs` (buffer table; sync-lowered async fs
operations from an async-lifted WAT guest) and `tests/readwrite.rs` (below).
**Disk is not included**: it is the same pattern (one more provider trait + ~an hour of
wiring) but nothing consumes it yet; it can follow with the unix providers.

**Loader rule for optional imports.** The always-registered `eo9:X/X-optional` flavors
answer `default() -> some(handle)` when the capability was granted and `none` otherwise,
so a program importing an optional capability it was not granted spawns fine and observes
absence (observationally `X.none`); required imports still fail at spawn. Types-only
interfaces and `eo9:io/buffers` are always available (they carry no authority). Test:
`optional_import_is_auto_sealed_when_not_granted`.

**Task API reconciliation.** Host-side `Task::wait()` (future resolving with the outcome)
joins `runnable()`/`kill()` to mirror the now-async `eo9:exec/task` operations; `kill`
remains synchronous on the host side since it resolves immediately.

**readwrite (definition of done): runs end to end.** The merged `eo9-example-readwrite`
component — async `main` awaiting async fs operations, owned buffers round-tripping
through `eo9:io/buffers` — runs to completion against `MemFs` in `tests/readwrite.rs`,
covering both its success vocabulary (`round-tripped(n)`, bytes verified in the provider)
and its failure vocabulary (fs error surfaced by the program). This also closes
escalation E1 for practical purposes: wit-bindgen now emits genuinely async-lifted
exports, which is exactly what wasmtime 45 requires for a guest that awaits.
`tests/readwrite.rs` builds the component via the guest workspace if it is missing
(normally `cargo xtask build-guest` has already produced it).

**Cross-crate touch-ups (disclosed).** Adding the `fs` field to `Providers` required a
one-line `fs: None` addition at the two existing construction sites outside this area
(`crates/eo9/src/providers.rs`, `tests/eo9-integration/tests/determinism.rs`), and area
13's kill/linearity sleeper fixture was mechanically re-synced to the async `sleep`
operation (its old future-returning import no longer links). Areas 11/13 own both files
and may adjust further.

### D11. Exec provider (branch `area/04-exec-provider`)

**Granted surface.** A new optional `exec` provider (None by default) links
`eo9:exec/{component-algebra, images, compile, task}` for the task that holds it.
Component-algebra operations delegate to `eo9-component` (load/save/describe/compose/
extend/restrict/rename; `configure` is registered but
answers `configure-error::internal` until area 03's implementation lands — small
follow-up); `compile` delegates to `Image::compile` on the same
pinned engine. Per-task handle tables carry caps: 32 components / 64 MiB total component
bytes / 16 images / 8 children, enforced before allocation.

**Child capability policy.** `ExecProvider::new(engine, ChildPolicy)` takes an explicit,
embedder-supplied policy; a child gets exactly what its composed image carries plus the
root providers the policy factory returns (default: none). Nothing is inherited from the
parent's own host authority.

**How children are driven.** wasmtime forbids recursive `run_concurrent`, so a child
cannot execute inside the parent's host calls. Children live in a `ChildSet` shared
between the exec provider (inside the parent's store) and the parent `Task`; the parent's
embedder-facing `resume` gives each runnable, unfinished child one fuel quantum per
iteration, charged against the parent's own donation — so children run on parent fuel with
no embedder changes, and killing/dropping the parent drops its children. The guest-facing
`wait`/`runnable` host functions only observe child state (waking the parent while a child
still needs CPU, or parking on the child's doorbell when it is blocked); guest-facing
`resume` is not supported yet (it reports a finished child's outcome, otherwise traps with
a clear message) — escalation E5. Dropping a `task` handle kills the child.

**Evidence.** `tests/exec_api.rs`: an executor guest granted exec receives a child binary
and an adapter provider as `list<u8>` arguments, loads both, composes the adapter onto the
child, compiles, spawns with no extra providers, waits, and returns the child's rendered
outcome ("42") as its own success — all under the ordinary fuel-sliced resume loop; a
second test shows exec is not linked unless granted. Adding the `exec` field to
`Providers` needed the same disclosed one-line `exec: None` touch-ups in `crates/eo9` and
`tests/eo9-integration` as the fs field before it.

### D12. Exec polish (branch `area/04-exec-polish`)

`configure` in the granted exec surface now delegates to `eo9_component::configure`
(unknown/missing/invalid-argument cases map to `invalid-args`, the rest 1:1). `kill` keeps
the child entry as a finished task (`Task::kill_in_place`), so `wait` after `kill`
resolves to `abnormal(killed)` instead of trapping. The per-task handle-count and
component-byte caps have direct unit tests.

**Open issue (planner decision needed): configured compositions trap at instantiation.**
Configuring the real `entropy.seeded` stub via the algebra, composing it onto an
unconfigured entropy consumer, compiling and spawning fails during instantiation with
`wasm trap: uninitialized element` (an indirect call through a never-initialized table
slot in the synthesized binder) — the configured provider never runs under wasmtime 45.
Captured verbatim by `algebra_configured_composition_currently_traps_at_instantiation`
in `tests/exec_api.rs`, which should be flipped to assert the deterministic seeded stream
once resolved. Per instruction, no workaround was attempted; candidate directions are an
area-03 binder fix or bind-on-first-use.

### D13. The variadic-tail convention in the WAVE arg binder (branch `area/04-positional-args`)

Owner request: `cat a.txt b.txt` instead of `cat --path a.txt`. The convention, applied
uniformly by the front-ends and the runtime:

- **Positional values** (bare tokens after the program name) fill `main`'s parameters in
  declaration order; named `--flag value` pairs still bind by name and win their parameter.
  This was already eosh's application rule; `eo9 run` / the implicit-run CLI form now accept
  bare tokens too (`cli::ProgramArg`, `run::bind_args`), with `--` forcing everything after
  it positional.
- **A final `list<string>` parameter is the variadic tail**: positional values left over once
  the other parameters are filled are collected into it, and when nothing supplies it at all
  it defaults to the empty list. The empty-list default lives in the runtime's
  `wave::parse_args` (last parameter + `list<string>` only), so every embedder gets it; the
  CLI and eosh also emit the collected list themselves. A *named* flag for a `list<string>`
  parameter whose value is not already WAVE list syntax is coerced to a one-element list
  (`cat --paths a.txt` ≡ `cat a.txt`); supplying both the flag and positionals is rejected as
  ambiguous.
- Type direction is unchanged: a positional landing on a `string` parameter is quoted
  literally, anything else is passed through as WAVE text (so `cruncher 9 200000` works and a
  mis-typed positional gets the runtime's "not a valid `u64`" error naming the parameter).
- Scope: usermode (`eo9 run`, eosh via the host exec provider) — the kernel's and the web
  blob's own arg codecs do not have the empty-list default yet; on those surfaces the
  variadic tail still has to be supplied (follow-up if it matters in practice).

Coreutil signature changes that ride on this are plan/17 D6.

### Escalations for the planner

- **E1 (resolved by the async-operations migration):** binaries that await must have an
  async-lifted `main` under wasmtime 45. The WIT now declares `main: async func`, wit-bindgen
  emits the async callback lift for it, and `eo9-example-readwrite` runs end to end (D10).
  No further action needed; kept here for the record.
- **E2 (wit/, area 02):** `program-outcome` has no arm for abnormal termination (trap,
  kill, out-of-fuel death). `wait`/`kill` return `future<program-outcome>`, which today
  cannot express "killed". Proposal: add a third arm (e.g. `aborted(abort-reason)`).
- **E3 (upstream / planner):** the resume shim of D2 and whether to pursue a wasmtime
  change (or carry a patch) for store-parked fuel yields before milestone I3.
- **E5 (guest-facing `resume`):** donating fuel from one guest to another cannot be
  implemented by executing the child inside the parent's host call (recursive event loops
  are forbidden); it needs either an upstream wasmtime facility or a redesign where the
  embedder loop brokers per-child donations. Until then `eo9:exec/task.resume` traps with
  a clear message and schedulers must use `wait`.
- **E4 (area 01):** root pin entry for wasmtime could become `default-features = false`
  so the runtime can opt out of unused default features in the TCB build.
