# The usermode registry liveness margin

**Question** (plan/10 D21 finding #2): why does a completion-blocked service appear to
advance only in the drive loop's parked 10 ms windows rather than on pump slices — and is
this the mechanism behind the fleet's "CLI transient failures under load"?

## Characterization (instrumented: per-pump service state + drive-loop counters)

The premise is half right, and the half that is right is a *margin*, not a starvation bug.

Measured facts (release build, warm store, the old `svc_shell` session shape, 20+ runs per
configuration; instrumentation: a per-pump trace of every service's
`(runnable, parked, doorbell)` plus drive-loop iteration/park counters):

1. **`pump` runs on every drive-loop iteration, parked or not.** 28 iterations → 28 pumps,
   in every run. Services advance on pumps: `restart.always` restart counts track pump
   counts ~1:1 once the service exists (20 restarts in 28 pumps; chaos-perturbed runs:
   4–10 restarts in 10–17 pumps; never zero).
2. **No service can be completion-blocked in usermode today.** The registry's provider
   surface is eager end to end: `ServiceText::write` pushes to the log ring synchronously,
   `read_line` answers end-of-input immediately, and services hold no other completion
   sources. The trace confirms it: across every sweep, no Running service was ever
   observed `parked` at a pump. ("Completion-blocked services" become real the day
   detach-with-env grants time/fs/net — the margin below is the hole they would fall
   into.)
3. **Pump opportunities are rationed by the foreground's *return events*, not by wall
   time.** The drive loop iterates (and therefore pumps) only when the foreground's
   `resume` returns: a genuine `Blocked` (a host op the pool did not finish inside the
   poll's sync-wake window) or `OutOfFuel` (> ~1M fuel of guest work per donation).
   Decisive control: replacing three trivial foreground lines with two 5000-round
   crunchers added **zero** `OutOfFuel` returns (`out_of_fuel=0` — the rounds ride
   through fuel-yield sync wakes inside a handful of donations) and ~0 extra pumps,
   while one `net.l4.loopback $ sockcheck` line (real loopback round-trips) adds many
   genuine `Blocked` returns. This is exactly the resolve-cache agent's bisect result —
   "fuel-heavy pacing did not help; blocking-I/O pacing fixed it deterministically" —
   explained without any starvation: both pacings add fuel, only one adds *return
   events*.
4. **The 10 ms park is a backstop, not the service CPU source.** Parks occurred only
   while no service was runnable (4–6 per session, almost all before the first detach).
   A compute-runnable service suppresses parking entirely (`services_runnable →
   continue`), so busy services pump at full speed.

So the flaky test was racing a structural margin: in the fast world (session resolve
cache + warm compile caches) a trivial line produces at most one return event, the
session's total pump budget collapses from hundreds to a handful, and whether the
asserting line arrives before the services' last needed pump becomes wall-clock-thin.
Pacing the test with blocking I/O (the resolve-cache agent's fix) is the correct idiom
and stays.

## The structural gap (the fix)

The park path had two genuine holes, both on the `Blocked`+idle edge:

* **The embedder's park woke only on the foreground's doorbell.** A service event during
  the park — a restart delay expiring, or (future) a completion landing for a parked
  service — waited out the remainder of the 10 ms backstop. Bounded today; unbounded
  in *latency accumulation* for any future service that does real I/O (every completion
  would eat up to 10 ms).
* **The park timeout ignored restart deadlines.** A `restart.backoff` deadline due 3 ms
  into a park was picked up ~7 ms late.

Fix (`run.rs` + `providers.rs` + `svc.rs`): the park is now
`park_until_progress(task, registry, backstop)` — it registers the parking thread's waker
with the foreground's doorbell **and every parked Running service's doorbell** (the same
observe→register→re-observe protocol as `Task::runnable`, via the loom-checked
`Doorbell::poll_edge`), bounds the timeout by the earliest pending restart deadline, and
keeps the 10 ms backstop unconditionally. Behavior change for today's eager services:
restart deadlines are honored to the millisecond instead of ±10 ms. Behavior change for
tomorrow's parking services: completions wake the loop immediately instead of polling at
100 Hz. No hot-path change: the new code runs only on the already-parked edge.

## The ledger-#2 verdict (honest)

The "CLI transient failures under load" class is **not confirmed** to be this mechanism.
The margin can only fail assertions that race service progress against a session's end —
`svc_shell`'s two tests are the only suite with that shape, and the broader cli flakes
(compile races, store contention) involve no services at all. The connection stays
plausible only for svc-shaped tests; the ledger entry's target-dir contention explanation
stands for the rest.

## Kernel-side audit (read-only)

The kernel does not share the margin's shape: its drive loop's idle path wakes on the
shared idle-waker list, and kernel service completions (timer wakes, INTx deliveries)
ring those wakers directly (the 5fa53e8 drain-all list plus the pciwait sticky counters),
so a parked kernel loop is woken by service completions already. The kernel svcdemo's
restart pacing runs through `request_timer_wake`, which already bounds the halt by the
deadline. No kernel change needed; noted in plan/12 by reference.
