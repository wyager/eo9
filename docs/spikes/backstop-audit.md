# Backstop accountability audit

Implements the SPEC doctrine ("liveness is event-driven, never timer-driven … a periodic
backstop that exists so progress happens is a confession. Backstops may remain as
detectors during bring-up, but every backstop firing that discovers actionable work no
event delivered is a high-priority bug, and the end state carries none").

This audit inventories every periodic/interval timer in the kernel and the usermode
runtime, classifies each, and arms every progress-backstop as a *detector*: when a
backstop wake discovers work that no event delivered, it counts the find and prints a
rate-limited `liveness:` diagnostic. The backstops themselves are deliberately left in
place (owner ruling: "leave that in for now") — removal is the end-state follow-up, and
the detectors are how we earn it.

## Classification rule

- **DEADLINE-TIMER** — the event *is* time: a sleep deadline, a restart delay, a typed
  bound expiry, a watchdog that declares failure. Legitimate; stays.
- **PROGRESS-BACKSTOP** — exists so work that *should* have been event-woken gets picked
  up anyway. A confession; armed as a detector below.

## Inventory

| # | Site | Value | Class | Notes |
|---|------|-------|-------|-------|
| K1 | `wasm/mod.rs` `IDLE_WAKE_INTERVAL_NS` | 10 ms | **PROGRESS-BACKSTOP** | Idle-`wfi` cap while a child runs. Its own doc confesses: "so a compute-bound child whose fuel yield is somehow not detected as runnable still advances". The wake edges that should make it unnecessary: fuel-yield synchronous wakes (keep the loop hot), GIC/PLIC/UART/timer interrupts (wake the `wfi` directly, including masked-but-pending ones), and `wake_idle`'s every-hot-pass re-poll. Known leaner: the IntxWait take→register window analysis (plan/12 entry 69) cites this cap as the worst-case latency bound for a raced delivery. |
| K2 | `wasm/mod.rs` `IDLE_BACKSTOP_NS` | 1 s | **PROGRESS-BACKSTOP** | Idle-`wfi` cap at the bare prompt (nothing running). Input wakes via the UART RX interrupt, deadlines via the armed timer; the 1 s cadence exists "as a liveness backstop" and to give the UART scavenger (K3) a heartbeat. |
| K3 | `arch/*/uart.rs` `scavenge_rx` | per idle wake; 1 s nudge | **PROGRESS-BACKSTOP** (two rescues) | (a) drains FIFO bytes the interrupt path missed — every byte it moves outside the IRQ handler is stranded input, a detector find; (b) after 1 s of input silence, one dummy data-register read revives QEMU's wedged character feed — an *external-cause* rescuer (the wedge is QEMU's feed state machine under host load, not a kernel wake edge), counted but not treated as a kernel bug. |
| K4 | `wasm/mod.rs` `BLOCK_ON_WATCHDOG_NS` | 30 s | DEADLINE (typed failure) | Declares a wedged boot-time operation failed; it never makes progress happen. |
| K5 | `request_timer_wake` users: `SleepUntil`, `IntxWait` bound, svc restart deadlines | per deadline | DEADLINE | The event is time, armed precisely; `idle_wait` consumes them per pass. |
| K6 | `wasm/mod.rs` `MIN_WAKE_NS` | 100 µs | implementation floor | Avoids arming a timer at/before "now"; not a liveness mechanism. |
| K7 | `virtio_blk.rs` poll bound | bounded spins | DEADLINE (typed failure) | "A hang backstop, not a timeout": it bounds a wait and fails loudly; it does not rescue progress. Same class as the driver stubs' typed poll limits (out of this lane's scope). |
| K8 | `arch/{riscv64,x86_64}/mod.rs` boot `arm_wake(10ms)` | one-shot | bring-up detail | The first timer arm before the executor takes over; not periodic. |
| U1 | `crates/eo9/run.rs` + `providers.rs` `park_until_progress` backstop | 10 ms | **PROGRESS-BACKSTOP** | `park_timeout` cap when services exist. The restart-deadline bound inside the same `min()` is deadline-class; the unconditional 10 ms is the confession — its own doc: "a wake source this function does not know about costs at most `backstop` of latency". |
| U2 | `providers.rs` `wait_until_runnable` | none | event-driven | Plain `park()` with no timeout, woken only by the doorbell edge protocol (loom-checked). The good citizen: proof the end state is reachable. |
| U3 | `eo9-runtime/svc.rs` restart `until` instants | per restart | DEADLINE | Bounded into U1's park timeout; legitimate. |
| — | browser blob executor | — | out of scope | Event-driven by the JS event loop (JSPI); no periodic timers of its own. Noted for completeness. |

## Detector design

A backstop *firing* is: the `wfi`/park woke because the **cap** elapsed (not a requested
deadline, not an interrupt/unpark — those are events), **and** the wake discovered
actionable work. Work discovered after a cap-rated late wake must have existed when we
parked (on the kernel's single core nothing changes guest-visible state during a masked
`wfi` except interrupts, and an interrupt would have woken the `wfi` early; in usermode an
unpark would have ended the park early), so it is stranded by construction.

Kernel (`idle_wait` now returns a `WakeKind`):

- wake classification: `delay` was **cap-rated** iff the cap (K1/K2), not a requested
  deadline, determined it; the wake was **late** iff the full delay elapsed. Early wake =
  event = never a finding.
- on a cap-rated late wake, three stranded-work probes:
  - **input**: `scavenge_rx` moved FIFO bytes (now returns the count). Documented
    false-positive window: a byte landing in the few-µs gap between the timer wake and the
    scavenge's IRQ mask is counted though it was about to interrupt normally — vanishingly
    rare in scripted runs, stated on the diagnostic ("possible").
  - **intx**: any `pci::intx_pending(line) > 0` (a new peek that does not consume). A
    delivery only lands while a wait holds the line unmasked, and the handler's count
    should be consumed by the re-poll the delivery's own interrupt triggers — pending
    count at a cap-rated late wake means the re-poll edge was missed.
  - **runnable**: the drive loops (`shell.rs` init + eosh sessions) check the *next* drive
    pass: `any_runnable` immediately after a `WakeKind::Backstop` means a child/service
    was runnable while the core slept the full cap.
- `block_on` applies the same rule to its single future: `Ready` immediately after a
  cap-rated late wake is a find.

Usermode (`park_until_progress`):

- the park was **cap-rated** iff the 10 ms backstop (not the restart deadline) set the
  timeout; **late** iff the full timeout elapsed (measured; `park_timeout` may also wake
  spuriously early — treated as an event wake, conservatively never a finding).
- on a cap-rated late wake: foreground readiness (`poll_edge`) or `registry.any_runnable()`
  = stranded work.

Diagnostic policy (both sides): a per-kind atomic counter; `liveness:` grep-stable
prefix; print on the first find and every 16th thereafter (`n=` carries the running
count). Cost when silent: a handful of relaxed atomic loads per idle wake — nothing on
the hot path. The detectors stay on in normal builds.

## Gate enforceability

- **Usermode**: enforceable now — the busy-workload integration tests assert their
  captured stderr carries no `liveness:` line (added to the svc_shell suite). Any future
  stranded-work regression fails the suite loudly.
- **Kernel**: the battery scripts grep every transcript for `liveness:`; `cargo xtask ci`
  runs no QEMU, so the kernel gate lives in the battery convention, not ci. Recorded as
  the honest limit; the metal gate rides every reviewer battery the same way the
  canonical-values check does.

## Battery results (2026-06-04, detectors armed throughout)

**Zero stranded-work findings across the entire battery.** Today's backstops are already
silent — they rescued nothing anywhere we could make them speak. They stay armed as
detectors; the first `liveness:` line in any future transcript is a high-priority bug by
doctrine, with the missing wake edge as the fix (never a relaxed assertion).

| Session | Result | `liveness:` lines |
|---|---|---|
| aarch64 / riscv64 / x86_64 demos | canonical values byte-identical | 0 / 0 / 0 |
| storage: round-trip + admit-filtered chain (INTx-served) | green | 0 |
| net: l2check ARP + l4check DNS | green | 0 |
| gpu: interactive draw (checksum exact) + `check-gpu` (pixel-exact) | green | 0 |
| svcdemo + backoff crasher (2 timer-paced restarts, deadline-rated — correctly not flagged) | green | 0 |
| cancelcheck `--attempts 25` (hits=1, data-miss=0) | green | 0 |
| 10 unpaced 53-char paste bursts | 10/10 | 0 |
| HVF: boot + disk round-trip over INTx | green | 0 |
| chaos sweep: 3 seed bases x 60 `-c` iterations | 0 hangs | 0 |
| chaos x services: 20 seeded `--svc` sessions (detach/list/stop churn) | 20/20 | 0 |
| usermode suites: svc_shell 8/8 (x8 runs, gate embedded), full `cargo xtask ci` | green | 0 |

Honest coverage notes: the `-c` chaos sweep cannot exercise the park backstop at all
(`drive_to_completion` uses the timeoutless `wait_until_runnable`); the service-bearing
coverage is the seeded `--svc` sessions, the svc_shell gate, and the kernel svcdemo
session. The IntxWait take→register window — the audit's prime suspect — was exercised by
the INTx-heavy storage/cancelcheck/HVF sessions and never needed the backstop: the
masked-`wfi` pending-interrupt rule plus the every-hot-pass `wake_idle` cover the edge in
practice, exactly as the design argued.
