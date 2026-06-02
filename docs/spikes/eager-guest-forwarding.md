# Spike: the eager-guest-calls-guest suspension

**Problem** (plan/09 D31, study 09, GAPS "suspended-subtask"): a middleware provider with a
sync Rust core (smoltcp, eofs-core, virtqueue state machines) drives its imports with a
single-poll "eager" pattern — async-lower the call, `poll` the generated future once with a
noop waker, and require `Poll::Ready`. Against a **host** callee this always works; against
a **guest** callee that itself performs an import call, the single poll observes
`STARTED` instead of `RETURNED` and the middleware returns its typed io error
("the … provider suspended"). Two flagship attenuation patterns are blocked:
`pci.filtered $ disk.virtio $ …` (study 09) and `net.l4.over-l2 $ <switch port>`
(plan/09 D31).

## Ground truth 1: the component *type* carries asyncness; the *lift* does not have to match it

- The component-model **function type** carries an `async` bit, and import/export type
  matching requires the bits to be equal:
  `wasmparser-0.248.0/src/validator/component_types.rs:1090` (the `async_` field) and
  `:3279` (`if a.async_ != b.async_ { bail!("expected {a_desc} function, found {b_desc}…") }`).
  So a middleware that re-exports `eo9:pci/pci` must keep the exported functions
  *async-typed* — the wiring/compose layer is unaffected by anything below.
- The **canonical lift options are validated independently of the type's async bit**.
  `wasmparser-0.248.0/src/validator/component.rs:1291` (`lift_function`) computes the
  expected core signature from `(abi, options.concurrency)` via
  `component_types.rs:1191` (`ComponentFuncType::lower`) — which matches on
  `(Abi::Lift, Concurrency::Sync)` / `(Abi::Lift, Concurrency::Async{..})` and **never
  consults `self.async_`**. There is no rule "an async-typed function must be lifted
  async".

**Conclusion: an `async func` in WIT can be lifted synchronously.** The constraint
plan/03 D17 recorded ("a forwarder's export asyncness must match the WIT declared
asyncness") was about matching the *type* (sync WIT functions cannot be async-typed
exports), not about the lift options of async-typed functions.

## Ground truth 2: the whole toolchain already has the knobs

- **wit-component** chooses the lift ABI per export from the **core export's name
  prefix**, not from the WIT asyncness: plain `eo9:net/l4@0.1.0#recv` → sync lift
  (`AbiVariant::GuestExport`), `[async-lift]…` + `[callback][async-lift]…` → async lift
  (`kernel/vendor/wit-component/src/validation.rs:1248-1254`). Imports likewise:
  `[async-lower]name` vs plain `name` (`validation.rs:2110`).
- **wit-bindgen** (git ea49687, the pinned rev) exposes exactly this as the `async`
  option: `crates/core/src/async_.rs` — `-export:eo9:net/l4#recv` forces a **sync lift
  for a WIT-async export**, `-import:…` forces a **sync lower for a WIT-async import**;
  unlisted functions default to the WIT declaration. Per-function granularity, today.
- **wasmtime 45 runtime**: a sync-lower of an async-typed function is the
  block-until-done path (the existing `sleeper_wat` fixture exercises it); a sync-lifted
  callee is run to completion inside its queued call and posts **only**
  `Status::Returned` — the `Status::Started` event that async-lifted callees post when
  their start adapter lowers parameters
  (`kernel/vendor/wasmtime/src/runtime/component/concurrent.rs:2698`) does not exist on
  the sync path (`concurrent.rs:2467-2545`, the `else` branch of `queue_call`).

## The failure mechanism (verified by the fixture matrix)

For a queued call to a guest callee, the caller parks until the callee's *first status
event* (`concurrent.rs:2898`, the `start_call` wait loop) and — for an async-lowered
caller — **breaks at the first event it sees**. `Status::Started` is posted by the
caller-side machinery the moment the callee's parameters are lowered
(`concurrent.rs:2698`), *before the callee's body runs*, for **every** queued call,
regardless of the callee's lift flavor. Events live in a single last-write-wins slot
(`concurrent.rs:4824`), so what the eager caller's single poll observes is decided by a
race between its own resumption and the callee's completion:

- a callee whose activation **never yields** completes inside its own work item, and
  `Returned` overwrites `Started` before the caller resumes — the caller's single poll
  sees `RETURNED`. This is why `net.l2.deny $ net.l4.over-l2` and `disk.mem $ fs.eofs`
  work today: their callee bodies make no import calls.
- a callee that makes a **queued call of its own** yields to the event loop mid-body;
  the waiting caller resumes while the slot still says `Started`, and the eager poll
  fails. A call is queued when it is **async-lowered (against any callee)** or
  **sync-lowered against an async-lifted guest**. Host calls complete inline
  (`first_poll` / `poll_and_block` poll the host future once on the caller's stack) and
  sync-lowered calls to **sync-lifted guests** are direct fused-adapter calls — neither
  yields.

The empirical matrix (tests/eo9-integration/tests/eager_guest.rs, all seven pinned):

| leaf lift | relay import | relay export | consumer call | observed |
|---|---|---|---|---|
| async | async-lower poll | async | eager poll | **STARTED — the wall** |
| async | async-lower poll | async | sync-lower (block) | completes; relay saw RETURNED |
| async | async-lower poll | sync | eager poll | STARTED (export lift alone is irrelevant) |
| sync | async-lower poll | sync | eager poll | STARTED (async-lower always queues) |
| async | sync-lower | sync | eager poll | STARTED (sync-lower to async-lifted guest queues) |
| sync | sync-lower | sync | eager poll | **RETURNED — the fix** |
| sync | sync-lower | async | eager poll | **RETURNED — the minimal conversion shape** |

## Direction chosen: the all-sync convention for eager components

A component is **eager-callable** iff no export activation ever yields: every import
call must be **sync-lowered**, and every guest it calls must itself be eager-callable
and **sync-lifted** (host providers always qualify — they complete inline). The export
lift of the component itself does not affect its callers (Returned overwrites Started
within the activation), but a sync lift is what makes the component's *own* exports
non-queued for the eager component above it — so middlewares that sit under other eager
components (pci.filtered under disk.virtio, the switch under net.l4.over-l2,
disk.virtio under fs.eofs) need sync lifts too. The WIT types stay `async func`
throughout: the validator only forbids the async *option* on a sync *type*
(`wasmparser-0.250.0/src/validator/component.rs:402-409` `check_asyncness`), never the
reverse, and the toolchain already exercises the allowed direction (the text-sink
fixture sync-lifts `read-line` today).

What this buys, per component class:

- **Pure attenuators / policy middlewares** (pci.filtered, fs.filtered, net.l4.filtered,
  the switch): convert wholesale via wit-bindgen's `async` filter (sync bindings for
  both directions). Their bodies become plain function calls; the `eager()` helper and
  its typed-failure path disappear.
- **Drivers and engine bridges** (disk.virtio, net.virtio, fs.eofs, net.l4.over-l2):
  sync-lower their imports (the sync engine cores stop needing `eager()` entirely);
  sync-lift their exports so eager components above them stay eager.
- **One documented residual**: a sync-lifted task may not block, so a *parking* host
  operation (the INTx `wait`) cannot be forwarded through a sync-lifted middleware —
  exactly the interrupt-under-filter case. The polled fallback already covers it
  (drivers degrade to polling when `wait` fails), and the limitation shrinks to
  "interrupt-mode pacing does not survive interposition", recorded in plan/09.

Candidate (b) — teaching the runtime to drive a callee's nested chain to completion
before reporting status — remains the only path to "eager callers over *genuinely
async* callees", is vendored-runtime surgery, and is not needed for any shipped
composition. Candidate (c) — async rewrites of the middlewares — is superseded: the
all-sync convention removes the async machinery instead of doubling it.

## Verification

`tests/eo9-integration/tests/eager_guest.rs` — seven tests over hand-written
canonical-ABI fixtures (`tests/eo9-integration/src/fixtures.rs`, the `eo9-tests:eager`
section): the wall reproduced, every single-knob variant pinned as insufficient, and
both fix shapes (all-sync, and sync-lowered-imports-with-async-export) completing for an
eager caller. The fixtures also pin, as a side effect, that the encoder + validator +
runtime accept sync lifts and sync lowers of async-typed WIT functions end to end.

## Conversion plan (the follow-up pass, per stub)

1. `pci.filtered`, `fs.filtered`, `net.l4.filtered`, `net.l2.switch`, `pci-admit-*`,
   `fs-policy-*`, `net-policy-*`: `async: ["-all"]` in `generate!` (sync both ways),
   bodies lose their `eager()`/await scaffolding.
2. `disk.virtio`, `net.virtio`: sync-lower imports + sync-lift exports; keep the
   polled-fallback for `wait` (see the residual above); verify `pci.filtered $
   disk.virtio $ fs.eofs $ cat` and the vnic l4-over-switch acceptance on metal.
3. `fs.eofs`, `net.l4.over-l2`: same; delete the `eager()` helpers; the
   "provider suspended" io-error class disappears.
4. `fs.overlay`, `fs.eofs`-adjacent stubs, `gfx.*` when it lands: same convention.
5. Un-ignore the vnic l4-over-switch acceptance test (vnic_l4.rs) and re-run study 09's
   filtered-storage transcript on metal.

The conversion is per-stub bindgen configuration plus mechanical body simplification;
no WIT, no algebra, no vendored-runtime changes.
