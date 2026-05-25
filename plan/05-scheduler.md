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

1. **Crate shape.** `eo9-sched` is `#![cfg_attr(not(test), no_std)]` + `alloc` with zero dependencies
   (matching the kernel placeholder's pattern: std is linked only by the host test harness). The bare-metal
   build is verified with `cargo check -p eo9-sched --target aarch64-unknown-none` (the target is installed
   via the kernel workspace's toolchain file, same pinned nightly). xtask does not yet run that check —
   recommend area 01 add it to `build`/`lint` so regressions are caught by `xtask ci`; until then it is a
   documented manual check (crate docs, "no_std" section).
2. **Scheduler is pick/report, not resume.** The crate never executes anything. The embedder's cycle is
   `pick()` (marks the task Running and hands the caller the single-resumer role) → donate fuel → resume on
   the real engine → `report(task, spent, outcome)`. `ResumeOutcome` carries no program outcome — that value
   stays with the embedder; the scheduler only needs out-of-fuel / blocked / done.
3. **A fourth task state, `Running`.** The brief lists runnable/blocked/done; a `Running` state was added so
   the single-resumer-per-task invariant is visible and checkable: a picked task leaves the run queue and
   cannot be picked again until reported, and `pick()` panics if called while a task is in flight
   (single-core: at most one). Under SMP the same invariant becomes "at most one core resumes a given task".
4. **Fuel model.** A standalone, reusable `FuelLedger<A>` (generic over account id) enforces conservation:
   fuel enters only by `import`, leaves only by `burn`/`export`, moves only by balance-checked `transfer`;
   `imported == burned + exported + Σ balances` after every operation, failed operations change nothing.
   The `Scheduler` embeds one (accounts = its pool + one per task) and exposes `refuel`/`donate`/`reclaim`/
   `export`/`fuel_of`/`fuel_audit`. `refuel` is the node's own incoming donation (the caller asserts it
   really received that fuel); it also caps the node's *outstanding* fuel at `u64::MAX`, which is what makes
   internal movements (donate, reclaim, retiring a finished task's leftover into the pool) overflow-free.
   A finished or killed task's unspent fuel returns to the pool automatically.
5. **Policies.** `Policy` is a tiny trait (enqueue / dequeue / remove / len) over `TaskId`.
   `DeterministicPolicy` = lowest runnable id first (a pure function of the runnable set, so traces replay
   identically; can starve high ids by design). `FairPolicy` = FIFO round-robin. Task ids are allocated
   monotonically and never reused.
6. **Completion primitives.** `CompletionQueue<T>` (FIFO, `push` reports the empty→non-empty edge) +
   `Doorbell` (atomic edge-triggered flag, `&self` ring/take) as separate reusable types, per the spec's
   io_uring-shaped readiness description. The queue is deliberately not a concurrent structure: the embedder
   brackets pushes with its own exclusion (mutex in usermode, critical section in the kernel); a lock-free
   MPSC variant can replace it behind the same shape when SMP lands. Scheduler integration point is
   `Scheduler::ready(task)` (spurious wakes are no-ops).
7. **Platform trait.** `idle()` (required) and `now() -> Option<u64>` (optional, defaults to `None`).
   The SMP hook `wake(core)` is documented on the trait but deliberately not declared yet — it waits for the
   milestone-3 SMP design note (per-core run queues, multi-core single-resumer rule).
8. **Kill/reap bookkeeping.** `kill` retires a task that is not mid-resume (report first), is idempotent on
   done tasks, and does not cascade to children — cascading is the embedder's policy. `reap` removes a done
   task only once its children are reaped, so the parent/child structure never dangles.
9. **Tests.** Property tests use a small in-tree splitmix64 generator instead of an external property-testing
   crate (keeps the crate and dev-deps at zero dependencies): scripted random workloads replay to identical
   traces under the deterministic policy, and fuel conservation holds under arbitrary operation sequences at
   both the ledger and scheduler level, with failed operations changing nothing.
