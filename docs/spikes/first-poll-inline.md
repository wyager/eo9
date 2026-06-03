# Design note: first-poll-inline for guest callees

**Status**: prototype built and A/B-verified (area/04-first-poll); default-on
evaluation complete (area/04-firstpoll-eval) — see "Default-on evaluation" at the end
of this note for the standing gate (`cargo xtask firstpoll-ab`), the usermode and metal
numbers, the feature-on three-arch battery, and the GO recommendation awaiting the
owner's call. The vendored wasmtime gains the off-by-default
`component-model-async-first-poll` feature (kernel/vendor/README.md);
`tests/firstpoll-ab` is the standalone A/B workspace (both arms build the *same*
vendored copy, the feature the only variable). Default builds everywhere keep the
queued path.

**Context** (docs/spikes/eager-guest-forwarding.md, SPEC "Boundaries are honestly
async"): under the async-first doctrine every boundary that can wait is declared and
bound async, which makes **every** guest-to-guest call on a converted chain a *queued*
call in wasmtime 45 — even when the callee would complete without waiting. The runtime
direction recorded in the SPEC is to make the fast case fast: complete non-waiting async
calls inline, exactly as host calls are first-polled today. This note pins where that
change lives, what it must check, and how it is validated.

## Where the queue decision lives (vendored wasmtime 45)

All references are `kernel/vendor/wasmtime/src/runtime/component/concurrent.rs`.

1. **`prepare_call` (:2570)** creates the callee's `GuestTask`. Its `lower_params`
   closure posts `Status::Started` the moment the parameters are lowered (:2698) —
   caller-side machinery, before the callee body runs an instruction.
2. **`queue_call` (:2333)** wraps the actual callee invocation (`make_call`) in a work
   item and **always defers it**: `push_high_priority(WorkItem::GuestCall(…,
   GuestCallKind::StartImplicit(fun)))` (:2557-2566). This push is *the* queue decision:
   there is no inline path for guest callees. For callback-ABI (async-lifted) callees,
   `fun` runs the initial activation and hands the returned code to
   `handle_callback_code` (:2463).
3. **`start_call` (:2816)** then parks the caller in a wait loop (:2898-2940) until the
   callee's first status event arrives through the per-waitable event slot
   (last-write-wins, :4824). An async-lowered caller breaks at the first event it sees —
   `Started` if the callee hasn't completed inside its work item yet, `Returned` if it
   has.
4. **`handle_callback_code` (:2213)** interprets the activation's returned code —
   `EXIT`/`YIELD`/`WAIT` (:193-198). Under the callback ABI **suspension is a return
   value**, not a captured stack: an activation that wants to wait *returns* `WAIT(set)`.

Notably, upstream already contemplates this optimization in the comment at
:2884-2896 ("we _could_ call the callee directly using the current fiber … probably only
worth doing if there's a measurable performance benefit"), and names the one semantic
hazard: a directly-called callee that makes a *blocking* sync-lowered import would block
the caller, which the CM spec forbids.

## The proposed change

In `queue_call`, when the callee uses the **callback ABI** (`callback.is_some()`) and
entry is legal (gate below), run `fun` **immediately on the caller's stack** instead of
pushing it, then dispatch on the code it returns:

- **`EXIT`** — the callee completed (or cancelled) inside its initial activation:
  `task_complete` has already posted `Returned`/`ReturnCancelled` to the event slot, so
  the caller's `start_call` wait loop finds the terminal status without an event-loop
  round trip. The fast case is now one direct call plus the existing bookkeeping.
- **`WAIT(set)` / `YIELD`** — the callee genuinely suspended (or politely yielded):
  `handle_callback_code` has already registered it exactly as the queued path would
  have. Nothing to undo — the "fallback to queueing" is simply *not* having saved
  anything: the inline attempt only moved the initial activation earlier. The caller
  proceeds to park in `start_call` as today.

This is clean under the callback ABI precisely because suspension is a return value: the
initial activation either finishes or returns a code; there is no callee stack to
capture mid-call (the kernel runs fiberless; usermode fibers are never needed for this
path). Stackful (non-callback) async lifts and sync-lifted callees keep the current
paths unchanged.

## The legality gate (checked before inline entry)

Inline entry is a *conditional* fast path; when any check fails, push the work item
exactly as today:

1. **Reentrance** — the callee instance must be enterable now (`enter_instance` rules):
   inlining must not bypass the instance-reentrance lock that the event loop would have
   enforced at dequeue time.
2. **Backpressure** — the callee instance's backpressure state
   (`backpressure-inc`/`-dec`, `check_blocking_for`) must permit starting a new task; a
   backpressured instance keeps the queued path so the existing fairness holds.
3. **Stack depth** — the inline call deepens the caller's native stack by one activation
   per chain hop. Eo9 runs with explicit stack checks (signals-based traps are off on
   metal), so the gate budgets depth: inline only while remaining stack exceeds a fixed
   reserve; deeper chains fall back to the queue. The hardening matrix's depth-N chains
   are the regression net.
4. **Caller flavor** — the caller must be in a context where running guest code is legal
   right now (it just executed the lower, so it is); cancellation already delivered to
   the *caller* (`wake_on_cancel` pending) falls back to the queue so cancel ordering is
   unchanged.

## Determinism policy

**Always-inline-when-legal.** The gate's checks are all functions of store state, not of
timing, so the inline/queue decision is deterministic for a deterministic program — a
chain either always inlines at a given hop or never does. No heuristics, no counters, no
adaptive thresholds: those would make completion *order* (observable through fan-out,
`waitable-set` delivery, and the determinism suite) depend on history. Fan-out
serialization note: with inlining, a consumer issuing K calls to *eager* callees
completes each call at issue (status `Returned`, no handle) instead of interleaving
their activations through the queue — a *more* sequential, still-deterministic order.

## Expected interaction with the hardening matrix

These tests must stay green **unchanged** under the prototype (same outcome values, not
just same pass/fail):

- `async_chains.rs` (deep parked chains): leaf still parks → the inline attempt at each
  hop returns `WAIT`; semantics identical, one queue round-trip saved per hop. The eager
  leaf variant flips from "queued, completes via event slot race" to "completes at
  issue" — same observed status (`RETURNED`), same values, by design.
- `async_kill.rs` (kill / cancellation): kill drops the store regardless of inlining;
  the cancellation cascade tests pin that `CANCELLED` delivery, `task.cancel`
  acknowledgement, and `RETURN_CANCELLED` propagation are unaffected (the inline attempt
  never runs *after* a subtask exists — cancel always targets a parked, queued-or-inline
  started task in the same states as today).
- `async_fanout.rs` (fan-out): completion-order encodings are the sharpest detector —
  any nondeterminism or lost completion introduced by inlining shows up as a changed
  `order` digit string. The eager call's digit position must not change (it already
  completes at issue today via the wait-loop race; inlining makes that the guaranteed
  path rather than the raced one).
- `async_trap.rs` (trap-while-parked): a trapping inline callee surfaces the trap on the
  caller's stack instead of through the work item — same store poisoning, same outcome;
  the test asserts the trap reason and cleanup only.
- `async_bind.rs` (bind interplay): bind runs at spawn, orthogonal to inlining; pinned
  so the prototype can't accidentally reorder bind relative to first entry.
- `eager_guest.rs` (the seven-row matrix): the three `STARTED — the wall` rows become
  `RETURNED` under inlining when the nested call no longer yields — these tests *will*
  change and are the intended positive signal. They get updated (with the spike doc)
  only when the prototype lands; until then they pin today's behavior.

## Rollout plan

1. Vendored feature `first-poll-inline`, **off by default**; the only code change sites
   are `queue_call` (the conditional direct call) and a small legality-gate helper.
2. A/B in CI: the full matrix plus the `eager_guest` suite run twice in
   `tests/eo9-integration` (feature off = today's pins; feature on = the three wall rows
   asserted `RETURNED`, everything else byte-identical).
3. Measure: parked-chain completion latency and queue round-trips per call (count work
   items) at depths 1-4, fan-out K=3 — the "measurable performance benefit" upstream
   asks for, recorded in this note when collected.
4. Only after A/B is stable: flip the default in the Eo9 engine options, keep the
   feature flag for one release as the escape hatch, and propose upstream with the
   numbers (the upstream comment invites exactly this).

## Prototype results (area/04-first-poll)

**Where it landed.** Feature `component-model-async-first-poll` in the vendored
wasmtime, exactly two change sites plus a helper: `queue_call` returns the activation
closure instead of pushing it when the gate passes (callback-ABI callee, `Caller::Guest`
only — the host->guest `queue_call0` path always queues — instance enterable per the
same `do_not_enter`/`backpressure` checks `is_ready` makes, nested depth <
`MAX_INLINE_DEPTH` = 64), and `start_call` runs it inline (`run_inline_activation`)
before its wait loop. `EXIT` → the terminal status is consumed directly, no event-loop
round trip, no suspension; `WAIT`/`YIELD` → fall through (async-lowered callers take
their handle without suspending at all; sync-lowered callers park as today, with the
pending `Started` consumed first so the wait cannot miss a wakeup). One subtlety the
design sketch glossed: the inline run happens *before* the caller joins the subtask
waitable to its sync-call set, so events land in the (last-write-wins) slot without
waking anything — consuming the slot afterwards is both the result delivery and the
lost-wakeup guard.

**A/B harness**: `tests/firstpoll-ab`, a standalone workspace (embed-spike pattern) that
patches the whole vendored wasmtime family for *both* arms, so the feature is the only
variable. It includes the original suites by `#[path]` — pins verified verbatim, not
re-transcribed.

- The 21-test async-hardening matrix (`async_{chains,kill,fanout,trap,bind}`): green
  with identical outcomes in both arms, including completion-order encodings,
  cancellation cascades, and the two contract traps.
- `eager_guest`: the seven pins hold with the feature off; with it on, exactly the
  three rows whose STARTED came from a queued call to a *callback-ABI* callee flip to
  RETURNED (the wall row to `2002`, the sync-lifted-relay row to `2002`, the
  sync-lower-to-async-lifted row to `2007`), and the async-lower-to-*sync-lifted* row is
  pinned still STARTED — inlining is scoped to callback callees by construction.
- The real-chain suites (eofs, pci_filtered, net_l4_over_l2, vnic_switch,
  interposition, compound_config, algebra_properties, soundness_corpus): 33/33 in both
  arms.
- Kernel: an aarch64 build with the feature on (kernel feature `first-poll-inline`,
  appended manually to the build_kernel feature list) boots under QEMU, composes
  `net.l4.loopback $ sockcheck` on-target, runs it (`ok: echoed(52)`), and powers off.

**Numbers** (release, eager forwarding chain `time_leaf_async $ relay^N $
consumer(sync-lower)`, one async-lowered callback-ABI boundary per hop, all completing
eagerly; medians of four interleaved runs, 200 iterations each, spawn+run per
iteration; machine moderately loaded, treat as ~±15%):

| shape | queued (off) | inline (on) | delta |
|---|---|---|---|
| eager chain depth 1 | ~132 µs/run | ~82 µs/run | −38% |
| eager chain depth 2 | ~137 µs/run | ~90 µs/run | −34% |
| eager chain depth 4 | ~156 µs/run | ~112 µs/run | −28% |
| parked chain depth 3 (park+complete cycle) | ~167 µs | ~135 µs | −19% |

Every paired run at every depth was faster with the feature on; the parked-chain win is
the descent (the initial activations down to the host park run inline; the completion
cascade is event-loop-driven in both arms, as designed). Run-to-run noise still
dominates per-hop accounting — instantiation is most of each iteration — so these
numbers support "measurable benefit", not a per-boundary cost model.

**Known semantic deviation** (inherited from the upstream comment's caveat): a
directly-called callee that *blocks mid-frame* — a blocking sync-lowered import inside
its initial activation — now blocks its caller's fiber for the duration, where the
queued path blocked only a worker fiber and let an async-lowered caller proceed after
`Started`. Eo9 guests are callback-ABI throughout and express waits as `WAIT` codes, so
no shipped composition does this (the matrix and chain suites confirm), but the spec
change upstream names is required before default-on or an upstream proposal.

**Remaining for default-on** (rollout step 4): decide where the embedder opts in (an
Eo9 engine option vs. the cargo feature), wire the host workspace A/B into the local
gate (`cargo test` twice in tests/firstpoll-ab; an xtask subcommand would be the
ergonomic spelling), quantify per-hop costs on a quiet machine with work-item counters,
and a metal (not QEMU) measurement once a board is in hand.

## Default-on evaluation (area/04-firstpoll-eval, 2026-06-02)

**The standing gate now exists**: `cargo xtask firstpoll-ab` refreshes the guest
components, runs the whole A/B workspace in both arms (the 21-test hardening matrix and
the real-chain suites must be identical; the eager-guest pins are arm-specific by
construction), and then runs `--rounds N` (default 5) interleaved A/B timing rounds
reported as per-shape medians with min..max spread and the host load context.
`--gate-only` is the fast regression spelling. Any future change to the vendored async
machinery goes through this command. For kernel measurement builds,
`EO9_KERNEL_FEATURES_EXTRA=first-poll-inline` appends the feature to the standard
kernel feature lists (all three arches); nothing in the repo sets it, and a rebuild
with it unset was verified byte-identical to the pristine feature-off image.

**Usermode numbers** — quiet machine (load average ~5.6, 5 interleaved rounds; spreads
are tight and the arms do not overlap on any shape):

| shape | off: median (min..max) | on: median (min..max) | delta |
|---|---|---|---|
| eager chain depth 1 | 97.7µs (95.1..99.6) | 64.3µs (60.1..65.0) | −34% |
| eager chain depth 2 | 101.1µs (99.7..104.1) | 67.7µs (63.8..69.2) | −33% |
| eager chain depth 4 | 117.8µs (114.5..121.3) | 79.3µs (78.4..82.6) | −33% |
| parked chain depth 3 | 118.4µs (114.5..129.2) | 99.9µs (96.2..101.6) | −16% |

A heavily loaded run earlier the same day (load average 34-41, agents building
concurrently) showed the same direction with wide spreads (−6..−42% eager, parked
within noise) — the inline arm's median won every shape in every round under both
conditions. The parked-chain win is the descent inlining (the initial activations down
to the host park), as designed; the completion cascade is event-loop-driven in both
arms.

**Metal op-phase A/B** (QEMU TCG aarch64 — still emulation, not silicon; the real-board
datapoint waits for hardware). Method: boot each arm's kernel image, run the
composition five times in one boot; repetition 1 pays the on-target compile, the
session compile cache makes repetitions 2+ pure operation phase, timed from the typed
newline to the outcome marker on the serial stream (25 ms/char paced input, echo
verified — the check-gpu console conventions). Two boots per arm, interleaved:

| chain | off: median (spread) | on: median (spread) |
|---|---|---|
| `disk.virtio $ fs.eofs $ readwrite` (7 timed reps/arm) | 1.13s (1.10..1.16) | 1.13s (1.10..1.17) |
| `net.virtio $ net.l4.over-l2 $ l4check` (7 timed reps/arm) | 2.18s (2.18..2.20) | 2.21s (2.20..4.43) |

Storage is parity: the arms swapped order between rounds (off faster in round 2, on
faster in round 1), so boot-to-boot variance dominates any per-boundary effect. Net's
quiet-window medians are 2.18 vs 2.21 (+1%, within the day-to-day spread of a chain
whose op phase is real DNS over slirp plus interrupt-paced pumping); the on-arm's
4.43s outlier coincided with a logged host load spike to ~24. Conclusion unchanged
from the conversion-time A/B: when callees complete promptly, honest awaits — queued
or inlined — cost nothing visible at metal op scale; the inline win is µs-per-boundary
and lives below interrupt pacing and device latency.

**Feature-on metal battery** (semantic confidence; all on the feature-on kernels):

- aarch64 storage: baseline `disk.virtio $ fs.eofs $ readwrite` with INTx-paced
  completion (`ok: round-tripped(10)`, the interrupt-wait diagnostic present); the
  flagship filtered chain `pci.admit-address --allow "[{segment: 0, bus: 0, device: 3,
  function: 0}]" $ pci.filtered $ disk.virtio $ fs.eofs $ ls /` → `keep.txt`,
  `ok: listed(1)`, INTx served through the filter; next boot, the same filtered chain
  with `cat /keep.txt` served the contents — power-cycle persistence intact.
- aarch64 net: `l4check` resolved real DNS every rep; the switch demo
  `net.virtio $ (rename port-a link-a $ rename port-b link-b $ net.l2.switch) $
  vnicheck --mode arp` verified both per-port MACs against the gateway.
- aarch64 gfx: `EO9_KERNEL_FEATURES_EXTRA=first-poll-inline cargo xtask check-gpu` —
  both screendumps pixel-for-pixel exact.
- aarch64 services: the `svcdemo` boot — `svc list` (worker running, banner finished),
  `svc log banner`, `svc stop worker`, `exit` ends the boot once nothing runs.
- riscv64 and x86_64: feature-on images boot through init to the console; `hello` and
  `cruncher --seed 9 --rounds 200000` (`ok: digest(14341732361190694547)`) — the
  on-target codegen and run paths under the inlined first poll on all three arches.

**What remains true of the deviation**: a directly-called callee that blocks mid-frame
(a blocking sync-lowered import inside its initial activation) blocks its caller's
fiber. No Eo9 guest can express this — every Eo9 guest is callback-ABI and waits by
returning `WAIT` — and the gate pins that scope (the sync-lifted eager row stays
queued). The deviation is therefore unobservable in any shipped Eo9 composition, but it
is still a deviation from the written CM spec, which matters exactly when proposing the
optimization upstream. The upstream comment in `start_call` invites the change with
numbers; the spec conversation should accompany that proposal, not gate Eo9's own
default.

**Recommendation: GO for default-on in Eo9's own builds** — concretely, append
`first-poll-inline` to the three standard kernel feature lists in xtask (and the
vendored-stack web embedding when its lane picks it up), keeping the cargo feature as
the escape hatch for one release. Reasoning: semantics are pinned identical everywhere
we can observe them (the matrix, the chains, the eager pins, and the full metal battery
across three architectures, including the pixel-exact display path and the service
registry); performance is strictly better in usermode (−6..−42% on eager chains, parked
chains unaffected) and parity on metal where device pacing dominates; the determinism
policy (always-inline-when-legal, store-state-only gate) was designed for default-on
from the start; and the one semantic deviation is unreachable from Eo9 guests by
construction. NOT covered by this evaluation, deliberately: usermode `eo9` (registry
wasmtime — parity arrives only via the upstream proposal) and real-silicon timing
(Orange Pi, when it lands). Flipping the default is the owner's call; the change is
one line per feature list plus updating this note.
