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

## The failure mechanism (hypothesis to be verified by the fixture)

For an async-lowered call to an **async-lifted** guest callee, the caller parks until the
callee's *first status event* (`concurrent.rs:2898`, the `start_call` wait loop) and — for
an async caller — **breaks at the first event it sees**. `Status::Started` is posted the
moment the callee's start adapter lowers the parameters, *before the callee's body runs*.
Events live in a single last-write-wins slot (`concurrent.rs:4824`), so:

- a callee whose body runs to completion **without yielding** overwrites `Started` with
  `Returned` inside the same work item; the caller resumes and reads `Returned` —
  this is why `net.l2.deny $ net.l4.over-l2` and `disk.mem $ fs.eofs` work today;
- a callee whose body performs **its own import call** yields mid-body (guest-to-guest
  calls suspend the worker fiber; host calls that aren't immediately ready park the
  task), the event loop resumes the waiting caller while the slot still says `Started`,
  and the eager caller's single poll fails — this is the wall, and it reproduces at
  exactly the observed boundary: trivial-bodied callees work, callees with nested calls
  don't.

A **sync-lifted** callee never posts `Started` at all, so its caller — async-lowered or
sync-lowered, eager or awaiting — wakes only on `Returned`, at any nesting depth.

## Direction chosen: (a) sync lifts and sync lowers for eager middlewares

An eager middleware's implementation contract is already "I complete in one activation
and never park". Making its **exports sync-lifted** and its **imports sync-lowered**
declares that truthfully at the canonical-ABI level:

- exports: callers (eager or awaiting) get eager completion — the wall disappears;
- imports: a sync-lower of an eager chain never actually blocks (the chain grounds out
  at an eager host leaf or an eager sync-lifted guest), and the bridge code in the
  middleware becomes a plain function call — the `eager()` poll-once helper and its
  typed failure path disappear with it;
- the WIT surface, the component types, and every existing composition are unchanged.

Candidate (b) — teaching the runtime to drive an async callee's nested chain to
completion before reporting a status — is vendored-runtime surgery in the most delicate
code we ship and is unnecessary if (a) holds. Candidate (c) — rewriting middlewares as
genuinely-async state machines — fights the sync cores (smoltcp's and eofs-core's traits)
and re-fixes every middleware forever.

## Verification

`tests/eo9-integration/tests/eager_guest.rs` builds the minimal chain from hand-written
fixtures (an eager single-poll consumer → a forwarding relay → a guest leaf, over the
real `eo9:time` package) in all four lift/lower combinations and pins:

1. the wall, reproduced (async-lift relay with a nested call → the consumer's single
   poll observes a non-`RETURNED` status);
2. the fix (sync-lift relay → the consumer's single poll observes `RETURNED`);
3. sync-lowered imports block-until-done against both lift flavors;
4. a genuinely-parking callee still yields the typed-failure shape, never a hang.

(Results recorded below after the fixture lands.)

## Results

See the test file; summarized in plan/03 D25 / plan/07 D14 once verified.
