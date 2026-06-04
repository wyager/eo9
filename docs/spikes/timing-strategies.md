# Triggering timing bugs on purpose: retiring the CPU-hog convention

**Status:** spike (area 13/11). **Question (owner's framing):** "It seems sus that we even
need `yes > /dev/null` hogs to find timing bugs when we should have ultimate control over
scheduling — why was that helpful, and what are principled ways to avoid it?"

## 1. Why hogs ever worked, and why the premise of the question is right

Every timing bug found so far lived **below Eo9's deterministic scheduler**, in host layers
we do not schedule:

| Bug | Where the race lived | What widened it |
|---|---|---|
| Lost wakeup (plan/11, fixed `bd67f89`) | `eo9:exec/task.wait`'s host fn vs. a blocking-pool thread's completion — a few-instruction check→register window in `eo9-runtime` | OS preemption of the main thread mid-window |
| Paste freeze (plan/12 e.70, fixed `737af27`) | the kernel's drain-then-ack vs. QEMU's chardev feed — a few-instruction drain→clear window, guest-side, raced by a host-side device model | host load descheduling the TCG vCPU mid-handler |
| Session-lock race (plan/11, fixed `db2f756`) | multi-process create/lock/rename on the host filesystem | scheduler interleaving of separate processes |

The guest world is **raceless by construction** — fuel-metered yields plus deterministic
providers fix the interleaving, and indeed no guest-level race has ever been found. The
owner's instinct is correct: where we control scheduling, there is nothing to shake loose.
The bugs are all in the *embedding*: host functions racing pool threads, the kernel loop
racing QEMU's device models, processes racing each other through the filesystem.

Hogs "work" on those layers by doing two things, both blindly:

1. **Stretching windows** — a thread descheduled *inside* a critical few-instruction window
   keeps the window open for milliseconds instead of nanoseconds.
2. **Jittering arrivals** — cross-thread completions land at more varied times relative to
   the consumer's checks.

That is all they do. The costs: no targeting (the entropy goes everywhere, mostly into code
that is already correct), no reproducibility (a hang under hogs cannot be replayed), they
saturate the machine running the build (load 155 during one review), and the hog processes
themselves leak (the workaround ledger records 104 orphans and two distinct cleanup
pathologies). The lost-wakeup investigation is the proof that hogs were the blunt
instrument, not the method: unamplified-with-hogs reproduced **0/240**; a *targeted 500 µs
sleep inside the suspected window* reproduced **40/40**. The hypothesis did the work; the
hogs mostly generated confidence that something was there.

## 2. The strategy survey, against our actual stack

Our stack: macOS host (no rr), a single-threaded cooperative executor + a small blocking
pool (`eo9-providers-unix/pool.rs`) in usermode, QEMU TCG for metal. Verdicts:

| Strategy | Verdict | Notes |
|---|---|---|
| CPU hogs (`yes` × N) | **RETIRE** | Blind, irreproducible, leaks, saturates the build machine. Nothing below needs them. |
| Targeted delay amplifier (the proven 500 µs pattern) | **KEEP — codified §3** | The single most effective tool we have used. Requires a hypothesis; that is its virtue. |
| Seeded chaos points in the sync primitives | **ADOPT — prototyped §4** | The principled generalization of the amplifier: every check→register / ring / park boundary gets an injection point; a seeded PRNG decides; failures print the seed. Feature-gated, compiled out of production. |
| loom (exhaustive interleaving model) | **ADOPT, narrowly** — *landed: plan/13 entry 21; `RUSTFLAGS="--cfg loom" cargo test -p eo9-runtime --lib loom_`* | Cannot model the whole runtime (wasmtime inside). But our *primitives* are tiny: `Doorbell` (one atomic + one mutexed list, ~40 lines) is exactly loom-shaped. A loom model of ring/register/recheck plus a caller that discards or honors the recheck would have found the lost wakeup **exhaustively** — and proven the fix. Effort: a dev-dependency + a `#[cfg(loom)]` alias layer over `AtomicBool`/`Mutex` in `task.rs`, only worth it for files under `eo9-runtime` that implement synchronization. |
| shuttle (randomized PCT scheduling) | Alternative to loom for larger units | Same code-shaping requirement, probabilistic instead of exhaustive; use if a loom model ever state-explodes. Not needed while the primitive surface stays this small. |
| ThreadSanitizer | **MARGINAL** | Catches *data races* (unsynchronized memory access). All three bugs were logic races over correctly-synchronized state — TSan-clean by construction. Also requires nightly `-Zbuild-std` rebuilds. Not worth standing adoption; fine as a one-off sweep if we ever add lock-free code. |
| lldb scripted breakpoint-delays | Niche, documented §3.1 | The zero-code-change targeted amplifier: a breakpoint with a scripted sleep + auto-continue stretches any window in a *built* binary. Useful when editing the source is awkward (old commits, vendored code). Slower than an in-source amplifier; same methodology. |
| rr chaos mode | Unavailable here (Linux-only) | Worth remembering for CCR/Linux contexts: record once under chaos scheduling, replay the exact failing schedule forever. The gold standard we cannot have on macOS. |
| QEMU `-icount`/record-replay (metal side) | **ADOPT for harnesses, small** | `-icount shift=N,sleep=off` makes guest instruction timing deterministic and (in single-threaded TCG) interleaves device models deterministically with the vCPU. The paste-freeze race (guest drain loop vs. chardev feed) becomes a *deterministic function of input-injection timing* — sweep the injection offset and the window is enumerated instead of dice-rolled. Would not by itself have flagged the bug (you still need the burst input), but turns "freezes sometimes under load" into "freezes at offsets 31..34, always". One xtask flag + harness adoption. |
| Deterministic-simulation testing of the embedding (DST) | **LONG-TERM DIRECTION §5** | The endgame: make host completion *delivery* a controlled input, the way `ParkBed` already controls time. Eo9's own thesis applied to its embedding. |

A prior decision (plan/11, lost-wakeup entry) rejected "prod code carrying interleaving
hooks" for a deterministic regression test. The chaos layer respects the spirit of that
ruling: the hooks exist only under a non-default cargo feature, compile to empty inline
functions otherwise (verified: zero chaos symbols or strings in the feature-off binary,
§4.1), and live only in the
synchronization primitives — not scattered through provider logic.

## 3. The codified targeted-amplifier method

What the lost-wakeup fix agent did, written down as the standing escalation step:

1. **Hypothesize the window** from the symptom's structure (who was parked, what edge could
   have been missed, which two threads touch that state). A wild-specimen backtrace
   (`sample <pid>`) is worth more than a thousand stress iterations.
2. **Inject a temporary, uncommitted delay** (`std::thread::sleep(Duration::from_micros(500))`)
   *inside* the hypothesized window — not near it, in it.
3. **A/B**: unfixed+amplified must reproduce near-100%; fixed+amplified must drop to zero.
   The lost wakeup went 0/240 (hogs) → 40/40 (amplifier) → 0/160 (fix, same amplifier).
4. **Remove the amplifier; verify absent from source and binary** before merging anything.
5. Record the experiment in the plan file — the amplifier line itself is the regression
   test's specification, even when (per the plan/11 ruling) it does not ship.

### 3.1 The lldb variant (zero code changes)

For a binary you cannot or do not want to rebuild (old commits, vendored layers):

```
lldb -- ./eo9 -c "cat /notes.txt"
(lldb) br set -f link.rs -l 1641            # the window's first line
(lldb) br command add 1
  script import time; time.sleep(0.0005)
  continue
(lldb) run
```

Same methodology, ~100× slower per iteration (debugger stop/resume), no source edit.
Documented for the toolbox; the in-source amplifier is preferred when the tree is editable.

## 4. The prototype: seeded chaos points in the synchronization primitives

See `crates/eo9-runtime/src/chaos.rs` and the `chaos` cargo feature (off by default,
forwarded by `crates/eo9`). Design:

- **Where** (the principled part — instrument the *primitives*, not the bug sites):
  `Doorbell::ring` entry (jitters cross-thread completion delivery), `Doorbell::register`
  entry (stretches every check→register window in the tree), `Task::runnable` poll entry
  (stretches caller-side check→call gaps), and the embedder's pre-park boundary in
  `crates/eo9` (`wait_until_runnable*`). Four sites cover every doorbell user — including
  the lost-wakeup site — without naming any of them.
- **What**: at each point a per-thread SplitMix64 stream (derived from one global seed +
  the thread's creation index) draws: mostly nothing, sometimes `yield_now()`, sometimes a
  10 µs–3 ms sleep. Probabilities tunable via `EO9_CHAOS_SLEEP_PCT`.
- **Seeding**: `EO9_CHAOS_SEED=<u64>` or auto-generated; the seed is printed to stderr at
  first use (`eo9 chaos: seed=…`). Same seed ⇒ same per-thread delay decision streams.
  **Honest fidelity note:** with real OS threads this is *statistical* replay (the delay
  decisions repeat; the OS may still interleave differently around them), not rr-style
  exact replay. In practice the same seed re-hits the same hang with high probability
  because the injected delays dominate the natural jitter by orders of magnitude.
- **Cost when off**: the feature gates the module body and every call site compiles to
  nothing (empty `#[inline(always)]` stubs); verified by byte-identical release `.rlib`
  sections / no symbol references (see the acid-test log in §4.1).

### 4.1 Acid test — does it find the lost wakeup without hogs?

Method: locally revert the 3-line fix at `link.rs` (restoring the discarded-`Ready`), build
with `--features chaos`, run the committed harness (`tests/chaos-harness/run.sh`: N
iterations of `eo9 -c "cat /notes.txt"`, per-run watchdog, seed = base+iteration, **zero
hogs, otherwise idle machine**), then restore the fix and sweep long.

Results (measured on this machine, otherwise idle, **zero hogs**; full logs in the
plan/13 entry):

- **Pre-fix + chaos:** first hang at iteration **2, 2, 1, 3, 2** across five base seeds
  (1000..5000); sustained hit rate 24/60 + 6/15 + 3/15 + 3/15 + 5/15 ≈ **34 % of
  iterations hang**. Every sampled hang's backtrace matches the wild specimens
  (`run::drive_to_completion` → `providers::wait_until_runnable` → `thread::park`,
  blocking pool idle).
- **Replay fidelity:** re-running one hanging seed (1002) re-hung **10/10** — the
  statistical-replay caveat above turned out conservative for this bug (the injected
  register-window sleep dominates).
- **Pre-fix, chaos feature off, no hogs (control):** **0 hangs / 300 iterations** — the chaos layer is
  what finds it, not the harness or ambient load.
- **Fixed + chaos:** **0 hangs / 400 iterations** (two seeds x 200) — and the full CLI suite is green
  with the feature on.
- **Feature off = compiled out:** the feature-off binary contains zero chaos
  symbols/strings (the seed banner string is absent); call sites are empty inlined stubs.
  Byte-identity across builds is not claimed (incremental codegen layout varies); the
  behavioral gate is the unchanged suite results.
- **An unplanned sixth reproduction:** the harness's own unguarded *priming* invocation
  hung on its first run — the acid test caught its own scaffolding (the priming run now
  has the same watchdog as iterations). Timing bugs do not respect test/prod boundaries.

The 500 µs targeted amplifier remains the *fastest* reproducer (40/40) — chaos needs a few
iterations because it spreads its budget over four sites. That is the expected trade:
chaos is for **finding** (no hypothesis needed, runs in CI), the amplifier is for
**confirming** (hypothesis in hand, near-deterministic).

## 5. The endgame: DST for the embedding

`ParkBed` already proves the pattern at the API level: time is a provider, the test owns
completion. The generalization is to make **host completion delivery** itself a test-owned
schedule:

- A `DeliverySchedule` trait between providers and the doorbell: production = "ring
  immediately from the completing thread" (today's behavior); test = completions enqueue
  into the schedule, and the test (or a seeded explorer) decides delivery order and
  placement relative to the consumer's polls.
- The blocking pool already funnels every completion through one choke point
  (`Completer`); routing that through a schedule object is mechanical (~200 lines), and
  the single-threaded executor means delivery placement fully determines the interleaving
  — *exact* replay, not statistical, because the threads stop mattering.
- With that in place, the lost-wakeup class becomes an ordinary enumerable test: "deliver
  the completion at every poll boundary of the consumer and assert progress" — a dozen
  deterministic cases instead of a stochastic sweep.
- Effort estimate: the trait + pool routing ~1 session; converting the integration
  harness's spawn paths ~1 more; the enumerating test driver ~1 more. Worth scheduling
  after the current wave; the chaos layer covers the gap meanwhile.

## 6. The escalation ladder (the standing recommendation)

For any suspected timing bug, in order:

1. **Capture a wild specimen** if one exists (`sample`, backtrace) before killing it.
2. **Chaos sweep** (`--features chaos`, the harness, no hogs): cheap, hypothesis-free,
   seed-replayable. If it reproduces, you have a seed and a backtrace.
3. **Hypothesize the window** from the backtrace; **targeted amplifier** (§3) for a
   near-deterministic reproducer; A/B the fix against it.
4. **Fix**; amplifier removed and verified absent; chaos sweep again as regression
   (now expected clean).
5. **If the primitive itself changed**: add/extend the loom model for it (§2) — exhaustive
   verification of the new ordering, once, at PR time.
6. Metal-side timing bugs: same ladder, with `-icount` + input-offset sweeps as the
   amplifier analog (the window becomes enumerable), and the QEMU model source as the
   "other thread" to reason about.

Hogs appear nowhere on the ladder. The one residual use of *load* is detecting wholly
unsuspected contention (the lost wakeup was originally flushed out by a CI run's ambient
load, not by anyone looking for it) — and the chaos sweep in CI replaces exactly that role,
reproducibly.
