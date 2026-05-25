# 09 — Standard stub providers (`guest/stubs/*`)

## Scope
The hand-written stub/virtual providers from the spec's "Standard stubs" lists — small wasm components, one
crate each, composable with `$`/`&`/`with`.

## Spec references
"The capability algebra" (none/deny/stubs table and rules), per-API "Standard stubs" lines, "Environments
and the `&` operator" (the deterministic-environment example), Security (time.fuzzy).

## Deliverables (priority order)
1. `*.none` for every API (exports the `-optional` flavor answering `none`) — tiny, mechanical to write by
   hand, needed by `only`'s story and the loader rule.
2. Deterministic set: `fs.memfs`, `time.frozen`, `time.monotonic-stub`, `entropy.seeded`, `disk.mem` —
   together these make the deterministic environment of integration milestone I2.
3. Attenuators/refusers: `net.deny`, `net.loopback`, `fs.readonly` (imports fs, re-exports it read-only —
   first real middleware provider), `text.null`, `time.fuzzy` (jittered/quantized).
4. Later (needs Message API): `text.capture`.
- Each stub: targets its stub world from `wit/` (plan 02), takes `configure` args where the spec implies
  config (e.g. `entropy.seeded --seed`, `fs.memfs` size), ships with a compose-and-run test against an
  example program.

## Dependencies
02, 07 (provider-authoring support). Consumed by 10, 13, and the I2 milestone.

## Milestones
Match the priority order above; (1)+(2) unblock I2.

## Decisions

1. **Layout and build flow.** One small crate per stub under `guest/stubs/<api>-<stub>`, package name
   `eo9-stub-<api>-<stub>`, listed in `GUEST_COMPONENTS` so `xtask build-guest` componentizes and validates
   it like the examples. Each crate is `no_std` and runs `wit_bindgen::generate!` directly against the
   repo-level `wit/<api>` package (`path: "../../../wit/<api>"`), so the stub worlds are consumed from the
   interface source of truth with no per-crate WIT copies; `eo9-guest` is depended on for the guest runtime
   profile (allocator + panic handler) and the provider helpers (see plan/07, Decisions).
2. **Shipped (v0).** The `.none` stub for every API — `disk.none`, `entropy.none`, `fs.none`, `net.none`,
   `perf.none`, `text.none`, `time.none` — plus `entropy.seeded` and `perf.null`. (`perf.null` is not in the
   priority list but is synchronous and trivial, so it shipped alongside.) Verified with
   `wasm-tools component wit`: every shipped stub imports **nothing** and exports exactly its stub world's
   interfaces (`eo9:X/types` + `eo9:X/X-optional` for the `.none`s; types + API + config interface for
   `entropy.seeded` and `perf.null`).
3. **State and handle convention.** A provider's exported resource types are tokens; the state they refer to
   lives in a `static` (`eo9_guest::provider::ProviderState`), bound by `configure`. `configure` returns a
   fresh handle to that state and `default()` mints another handle to the *same* state — the spec's
   "`default()` hands out exactly the handle `configure` produced" is read as capability identity (same
   state/authority), since an `own` handle cannot be handed out twice. Using a provider before `configure`
   traps (the contract violation is the embedder's, not the program's).
4. **`entropy.seeded` PRNG.** SplitMix64 over the configured seed (hand-written, no dependencies);
   documented as reproducible-but-not-cryptographic.
5. **Deferred: every stub whose interface has `future`-returning operations** — `fs.memfs`, `disk.mem`,
   `time.frozen`, `time.monotonic-stub`, `net.deny`, `net.loopback`, `fs.readonly`, `text.null`,
   `time.fuzzy` (and `text.capture`, which additionally waits on the Message API). Reason (escalated to the
   planner): with the pinned toolchain a wasm guest provider cannot implement a plain
   `func(...) -> future<T>` export. wasm-tools 1.250 enforces "the `async` canonical option requires an
   async function type", so only `async func` exports (e.g. `configure`) may be async-lifted; a
   synchronously-lifted export has no live Component Model task left after it returns, and futures are
   rendezvous, so there is nothing to deliver the value the stub would need to write (wit-bindgen requires a
   current task to park the pending write, and dropping a writable end unwritten traps). Host-side providers
   (area 08) are unaffected — the constraint is specific to providers compiled to wasm.
   **Proposal:** declare the API operations as `async func(...) -> T` instead of `func(...) -> future<T>`
   (callers keep the same concurrency via async-lowered calls/subtasks, and guest providers become ordinary
   async functions — no future plumbing at all); the deferred stubs are then mostly mechanical. Decision
   belongs to the planner/area 02.
6. **Async `configure` works as specified.** The config-interface exports async-lift, componentize, and
   validate (`--features cm-async`); actually invoking them needs the host runtime's CM-async support
   (area 04), same as the examples that await futures.
