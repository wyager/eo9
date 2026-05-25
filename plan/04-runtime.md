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
(record here)
