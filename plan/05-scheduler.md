# 05 — Scheduler (`crates/eo9-sched`, no_std)

## Scope
The one scheduler used by both the usermode binary and the bare-metal kernel (SPEC.md Implementation
Details). Pure policy + bookkeeping over abstract tasks; no Wasmtime types, no OS types.

## Spec references
"Execution APIs" (schedulers are ordinary programs; resume/fuel; readiness bullets), Performance scheduling
TODO, Implementation Details.

## Deliverables
- `eo9-sched` crate, `#![no_std]` + `alloc`:
  - Task table: states (runnable / blocked / done), per-task fuel ledger with **conserved** donation
    (a scheduler node can only hand out fuel it received), parent/child structure.
  - Completion-queue + doorbell primitives (SPSC/MPSC queue, edge-triggered flag) as reusable types — the
    runtime (04) and kernel (12) both use these to implement the async host side.
  - Run-queue policies: `deterministic` (stable order, e.g. lowest-id-first — used by tests and the
    deterministic environment story) and `fair` (round-robin to start). Policy is a small trait.
  - Platform trait: `idle()` (wait for an external event), `now()` optional, `wake(core)` later for SMP.
    Usermode implements it with thread parking; the kernel with WFI/interrupts.
  - Single-core first. SMP (per-core run queues, the "one resumer per task" rule) is a later milestone —
    leave the invariant documented and asserted.
- Host-side unit tests: given a scripted set of tasks/completions, the deterministic policy produces an
  identical execution trace every run (property test).

## Dependencies
01 only (types). Consumed by 04 and 12. Keep the crate dependency-free apart from `alloc` (+ `heapless` or
similar only if justified — ask).

## Milestones
1. Task table + fuel ledger + deterministic policy + trace tests.
2. Completion queue/doorbell types adopted by plan 04.
3. Fair policy; SMP design note (not implementation) agreed with planner.

## Decisions
(record here)
