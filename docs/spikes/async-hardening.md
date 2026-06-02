# Spike: the async suspension/cancellation hardening matrix

**Context** (plan/13, plan/04; SPEC "Boundaries are honestly async"): the async-first
conversion stands the flagship chains on runtime paths GAPS flagged as unexercised —
"suspended-subtask path not yet exercised end-to-end; cancellation of an in-flight
forwarded call traps". This spike exercised them systematically with hand-written
canonical-ABI fixtures that *genuinely await* (async-lowered calls joined to waitable
sets, `WAIT` callback codes, `task.return` from the callback — the machinery every
wit-bindgen async build uses), against a controllable host clock (`park.rs`'s
`ParkBed`), so every park, completion, cancellation, and kill is test-driven and
deterministic.

Fixtures: `tests/eo9-integration/src/fixtures.rs` (the `eo9-tests:hard` /
`eo9-tests:bindhard` section); suites: `tests/eo9-integration/tests/async_chains.rs`,
`async_kill.rs`, `async_fanout.rs`, `async_trap.rs`, `async_bind.rs`.

## Headline: the GAPS cancellation caveat is stale — both mechanisms are clean

- **Host kill of a forwarded park** (`Task::kill` while consumer → N relays → parker →
  host sleep is parked): `Outcome::Killed`, the in-flight provider operation is dropped
  with the store, nothing panics — at depths 0 through 3. A completion arriving after
  the kill is a quiet no-op (the doorbell waker outlives the dead task).
- **Guest cancellation of an in-flight forwarded call** (`subtask.cancel`, sync flavor):
  the `CANCELLED` event is delivered to the callee's callback; a well-behaved callee
  cancels its own downstream (the cascade), acknowledges with `task.cancel`, and the
  canceller's blocking cancel resolves to `RETURN_CANCELLED` (4) — through forwarding
  layers, with the host-level sleep aborted and released. No trap.

The caveat dated from the binder-era forwarding path. What *does* trap is exactly the
canonical ABI's contract, now pinned:

- cancelling an **eagerly-completed** call traps `unknown handle index 0` — a call that
  returns at issue never mints a subtask handle (`Status::pack` packs no waitable with
  `RETURNED`), so there is nothing to name;
- cancelling after the completion event was **already consumed** traps
  ``"`subtask.cancel` called after terminal status delivered"``
  (`Trap::SubtaskCancelAfterTerminal`, vendored concurrent.rs:3772).

Conversion guidance: generated/hand-written async callers must cancel only subtasks
they still hold un-resolved handles for — both misuses are loud traps, not corruption.

## The liveness hole the SPEC rule exists for, demonstrated

A forwarding layer that **ignores** its `CANCELLED` event leaves the canceller's sync
`subtask.cancel` blocked forever: a quiet park, not a trap, fuel donations never help.
(`an_unacknowledged_cancellation_parks_the_canceller_forever`.) Host kill remains the
backstop and still releases everything. This is the concrete shape behind SPEC's "an
await across a trust boundary without a deadline or kill-scope is a liveness bug" — a
middleware that forwards async calls **must** handle `CANCELLED` (cancel downstream,
`task.cancel`), and the wit-bindgen conversion pass should verify generated bindings do.

## Deep suspension chains (section 1)

Awaiting consumer → N awaiting relays (N = 0..3) → parking leaf over the host clock:

- the whole chain parks on **one** host operation (no per-layer operations, no spin);
- the result survives every layer (`157 + 10·N` observed exactly);
- completion propagates promptly through all layers once the host completes;
- repeated runs are byte-identical (the executor's work queue is deterministic — the
  "Returned overwrites Started" race of the eager spike is a race in slot terms only);
- an eager leaf under the same awaiting machinery completes without ever parking and
  starts no host operation (the `RETURNED` arms all the way up).

## Fan-out (section 3)

One consumer, three concurrent calls against one parker instance (two park, one eager),
awaited jointly on one waitable set; per-task callee state via context-local slot 0:

- no completion lost; results stay associated with their calls (sum exact);
- completion order tracks the host completion schedule exactly and is deterministic
  (order digits byte-identical across runs, mirror schedule flips them);
- cancel-one-of-K: the cancelled call resolves `RETURN_CANCELLED`, releases its host
  operation, and the remaining calls complete normally.

## Trap-while-parked (section 4)

A sibling subtask trapping (`unreachable`) while another is parked on a live host
operation: the trap surfaces promptly as the program's outcome with the callee's own
reason (a trap poisons the composed program — SPEC kill semantics), and the parked
sibling's operation is released with the store. No hang, no leak.

## Bind interplay (section 5)

- a baked configuration is bound at **spawn**: a program that parks on its first
  instruction still observes the configured value after the park;
- a refused configuration surfaces as `SpawnError::ConfigurationRefused` with the
  provider's own text, and the would-park program is never entered (no host operation
  started) — "configure never traps" holds in async chains.

## Executor parity (section 6)

The aarch64 metal executor exercises the same machinery end-to-end (2026-06-02, this
worktree's kernel build):

- **Park/resume on the generic timer**: the demo's `sleepy` canary — an async-lifted
  `run` awaiting `eo9:time/time.sleep` 50 ms against the kernel timer — measured
  54 001 000 ns elapsed across the await (`ok (>= requested)`): a guest task suspends
  on bare metal and the wfi-idling executor resumes it on the timer interrupt.
- **Kill mid-execution**: `cruncher --seed 9 --rounds 2000000000` at the eosh serial
  console, Ctrl-C after ~7 s → `abnormal: killed`, prompt returns immediately; the
  shell and executor stay healthy (`hello` → `ok: greeted` right after).
- **Interrupt at the parked prompt**: Ctrl-C while eosh is parked on the UART read-line
  is absorbed without crash or spurious output; the next command runs normally.
- **Clean shutdown through the supervision chain**: `poweroff` → eosh exits
  `success(poweroff-requested)` → init honors it → PSCI SYSTEM_OFF.

Residual (recorded, not covered): a kill delivered while a *store program* is parked on
a kernel-timer await — no /bin program sleeps today, so the metal-side mid-park kill is
exercised only via the read-line park (UART) and by the usermode suite at the runtime
layer. When a sleeping store program exists, add the Ctrl-C-mid-sleep transcript.

## Toolchain notes (for the conversion pass and future SDK work)

- `[task-cancel]` is imported from the **`[export]<interface>`** module (alongside
  `[task-return]<func>`), not from `$root` — wit-component's validator scopes it to the
  exported function's task.
- Context-local storage (`[context-get-i32-0]`/`[context-set-i32-0]`, `$root`) works as
  the per-task state channel for concurrent activations of one instance; the fan-out
  parker is the in-tree example.
- The sync `subtask.cancel` **blocks** until the callee's terminal status (and may only
  be called from an async task); the `[async-lower]` flavor returns `BLOCKED` instead.
  Middleware cancel arms should expect to park there while the cascade resolves.
