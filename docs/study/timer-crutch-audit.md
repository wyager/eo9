# Timer-crutch audit (area/35) — 2026-06-09

Every timer, sleep, poll loop, cadence, retry interval, and backstop in the kernel,
guest components, and usermode runtime, classified against the owner doctrine
(2026-06-09): timers papering over event-handling bugs "are bugs, we must fix them
aggressively"; the executor model is "tasks available to execute immediately, or
wfi()" with "timers = a min-heap of (time T → task X) + a single hardware timer armed
to the heap minimum"; SPEC "liveness is event-driven" + the `liveness_finding`
detectors are the enforcement arm.

Scope note: `area/34-fuel-yield-latency` (in flight) owns the kernel executor core —
`IDLE_WAKE_INTERVAL_NS` cap deletion, fuel-yield runnable detection,
`IDLE_BACKSTOP_NS` restructure, detector extension, and the `wasm/mod.rs` /
`shell.rs` / `shellexec.rs` drive paths. Those items are inventoried below as
**owned by area/34** and not re-litigated here.

## Verdict

The executor core itself already matches the doctrine's shape (`SleepUntil` →
`request_timer_wake` `fetch_min` → one armed generic timer is exactly the mandated
min-heap-to-one-hardware-timer model, with area/34 tightening the caps around it).
The crutches live **above** the executor, in the device drivers: every guest driver
is deliberately polled with interrupts masked ("the polled driver's suppression
discipline"), so three whole input paths ride poll cadences over events the hardware
already offers — **USB HID keystrokes** (OHCI WDH interrupt masked, 2 ms sleep pace),
**network receive** (virtio-net INTx unused by `net.virtio` while `disk.virtio` right
next to it proves the path; idle telnet/oskexec listeners busy-pump the core at 100%),
and the **usermode service loop** (a hard-coded 10 ms ambient park backstop). One
detector — `scavenge_rx` — both detects and silently rescues on most wake kinds,
which the doctrine calls out by name.

---

## Class A — crutches over a missing/broken event path (THE BUGS)

Priority order is by user-visible cost. "Reachable today" = the event can be consumed
with plumbing that already exists; "needs plumbing" = a kernel surface must land first.

| # | Path | Cadence today | Available event | Reachable today? | Cost | Fix shape | Size |
|---|------|---------------|-----------------|------------------|------|-----------|------|
| A1 | USB HID keystroke forwarding: `guest/stubs/usb-kbd/src/lib.rs:43,193-198` (`POLL_PACE_NS` = 2 ms sleep between empty `usb::read` polls); the OHCI core masks ALL controller interrupts at bring-up (`crates/eo9-ohci/src/driver.rs:161,243-244` — "the polled driver's suppression discipline"); `poll_interrupt` (`driver.rs:909-...`) is a short poll of the done queue | 2 ms sleep + one multi-host-call poll round, forever (the always-on `station` kbd service never stops) | **OHCI WritebackDoneHead (INT_WDH)** — the controller already writes the done queue back and the driver already acks WDH (`driver.rs:540-553`); the interrupt edge is generated and thrown away. RHSC (root-hub status change) likewise | **QEMU leg: yes.** `usb.ohci-pci` rides `eo9:pci`, which has `enable-interrupts`/`wait` wired and *proven* by `disk.virtio` (INTx live on aarch64-virt `arch/aarch64/mod.rs:59` and riscv64 `arch/riscv64/mod.rs:46`; x86_64 unwired). **Board leg: no.** `usb.ohci` rides `eo9:platform`, whose `enable-interrupts` answers `unsupported` in v1 (`kernel/.../platform_provider.rs:461-469`; usb-ohci-plan risk 7, GIC SPIs 216/219) | Keystroke latency adds ≤2 ms (pace sits under the 8–10 ms endpoint interval) — modest; the real cost is the **idle machine**: the kbd service wakes the core every 2 ms forever, each empty poll a chain of host calls, so the 1 s idle backstop is never reached and the station config can never idle near 0% | Unmask `INT_WDH` (+`RHSC`) behind a wait surface in the core; `usb::read` = arm TD → await interrupt → drain done queue, with the existing short poll kept as the fallback where interrupts answer `unsupported` (x86_64, v1 platform). `usb.kbd` drops `POLL_PACE_NS` entirely and just awaits `read` | **M** (core + both shells + check-usb/check-station). Board leg **L** (platform interrupt routing lands first) |
| A2 | Guest net receive is a busy-pump: `guest/stubs/net-l4-over-l2/src/lib.rs:769-805` (`wait_until` — zero pacing between pump rounds; each round is `recv_frame` host-call chains that complete inline) and `guest/stubs/net-text/src/lib.rs:395-401` (the accept loop retries 4 s `TimedOut` accepts forever) | continuous — an idle `telnetd`/`oskexec` listener is **permanently runnable**: the kernel drive loop never reaches `idle_wait` (shell.rs keeps the loop hot for a runnable child), the core never sleeps, tens of thousands of host calls/s at the bare "waiting for a connection" state | **virtio-net RX used-ring interrupt (INTx)** — recorded follow-up plan/12 D59, stated verbatim in `guest/stubs/net-virtio/src/lib.rs:29-31` ("`disk.virtio` waits on them — but this driver still polls"). Board: RTL8125 ISR exists but `arch::pci_intx::WIRED = false` on rk3588 (`arch/aarch64/mod.rs:88`, deferral recorded in `rk3588_pcie.rs:93`) | **QEMU leg: yes** (D59 — the IntxWait machinery `pci_provider.rs:982-1045` is proven by disk.virtio). **Board leg: no** (rk3588 PCIe INTx plumbing first) | Idle listener = 100% of a core (QEMU vCPU pegged; on the board it also starves the 1 s idle and is the workload under which the stranded-runnable detector fired its first real hit, GAPS 2026-06-08) | `net.virtio` `recv-frame` gains an interrupt-wait arm (unmask RX, await INTx, ack ISR; poll fallback where unsupported) — then `wait_until`'s pump round genuinely parks per round and the `net.text` accept-retry loop becomes cheap without modification | **M** (D59, net.virtio + check gates). Board leg **L** |
| A3 | Usermode service drive loop ambient backstop: `crates/eo9/src/run.rs:170-175` parks with a hard-coded `Duration::from_millis(10)`; `crates/eo9/src/providers.rs:1239-1276` documents it as covering "a wake source this function does not know about" | 100 wakes/s whenever a session with ≥1 detached service is otherwise fully parked | All known wake sources are **already registered**: foreground doorbell, every parked service's doorbell (`park_ready`), the earliest restart deadline (`next_restart_due`). The backstop hedges *unknown* sources — definitionally a crutch by the doctrine | **Yes** — pure parameter/structure change, no plumbing | Idle `eo9 shell` with a service burns 100 wakes/s instead of ~1; contrast: the foreground-only loop (`wait_until_runnable`, providers.rs:1219-1228) parks **indefinitely** — the event-pure shape already exists in the same file | Mirror area/34's kernel restructure: lengthen to detector-grade (~1 s) or delete the cap and trust the registered wake set + the park-backstop detector. **Sequence after area/34 lands** so both executors keep one doctrine shape | **S** (after area/34) |
| A4 | USB connect detection polls port status: `usb-kbd/src/lib.rs:41,65-74` (50 sweeps × 50 ms), `guest/examples/usbcheck/src/lib.rs:36,102` (100 ms watch pace) | 50–100 ms sweeps until a device appears | **OHCI RHSC** (root-hub status change interrupt) — masked with everything else at bring-up | Same legs as A1 (rides the same wait surface) | One-shot at service start; 50 ms granularity is imperceptible for hotplug — lowest class-A priority | Fold into A1: `usb::port` watch becomes an RHSC-driven wait where available | **S** (incremental on A1) |
| A5 | Kernel storedisk: one synchronous spin per request, `kernel/eo9-kernel/src/virtio_blk.rs:87,462-477` (`POLL_LIMIT` = 50 M spins on the used ring) — the **entire executor blocks** for the request's duration | per disk request | virtio-blk used-buffer notification (INTx — wired on the QEMU profiles that carry storedisk) | Event wired; the blocker is the synchronous in-kernel driver shape (no async path to park on) | Small on QEMU (sub-ms completions); structurally it means a slow device freezes the whole kernel | **Already tracked**: disk-iops audit rung 3 (`docs/study/disk-iops-audit.md` — real ring + interrupt-driven completion). Inventory only; no new GAPS entry | L (rung 3) |

### The class-A fix queue (priority order)

1. **A2 — net receive interrupt wait (QEMU leg / D59)** — M. Biggest steady-state cost
   (idle listeners peg a core today; A1's 2 ms pace is at least bounded). All plumbing
   exists; `disk.virtio` is the template.
2. **A1 — OHCI WDH interrupt for HID (QEMU leg)** — M. The suspected #1: confirmed —
   keystrokes ride a 2 ms poll over a masked, already-acked interrupt. Latency cost is
   small but the always-on station service makes the idle cost permanent.
3. **A3 — usermode 10 ms park backstop** — S, sequenced after area/34.
4. **A4 — RHSC connect watch** — S, rides A1.
5. **Board legs of A1/A2** — L each, gated on platform interrupt routing
   (usb-ohci-plan risk 7) and rk3588 PCIe INTx respectively. These are the follow-up
   plumbing lanes; until they land, the board's polled fallbacks stay honest-and-bounded.
6. **A5** — proceeds inside the disk-iops rung-3 lane, not here.

---

## Class B — honest timed obligations ("time T → task X")

| Item | Where | Verdict |
|------|-------|---------|
| `SleepUntil` + `request_timer_wake` (`fetch_min`) + one armed generic timer | `kernel/.../wasm/providers.rs:484-494,708-724`, `wasm/mod.rs:296-308` | **Matches the mandated model exactly** (the atomic-min slot is the degenerate min-heap; parked futures re-arm per pass). Executor caps around it: owned by area/34 |
| Kernel service restarts: `SRun::WaitingRestart { until_ns }` + `request_timer_wake` on every pass | `wasm/svc.rs:264,756-758,918` | Explicit deadline, wakes precisely — good |
| Usermode restarts: `WaitingRestart { until: Instant }` + `next_restart_due` bounding the park | `crates/eo9-runtime/src/svc.rs:276,644,697`; `crates/eo9/src/providers.rs:1265-1271` | Explicit deadline, cap-rating distinguishes it from the backstop — good |
| `restart.backoff` policy (base × 2^n, 1 h cap, give-up budget) | `guest/stubs/restart-backoff/src/lib.rs:34,50-68` | Pure compute, returns a delay — the "policies are programs" shape |
| Usermode `time.sleep`: dedicated timer thread over a **deadline min-heap** | `crates/eo9-providers-unix/src/time.rs:6,16,64-77` | Literally the doctrine's min-heap — good |
| Hardware watchdog: 22.4 s DW-WDT + pat from every drive pass/idle wake; 5 s heartbeat | `wdt.rs:50-53,116-151` | Dead-man's switch, not a progress backstop — the file carries the doctrine note verbatim. Heartbeat is pat-quantised diagnostics |
| `fibercompile` `SLICE_NS` = 5 ms compile slice + 5 s progress line | `wasm/fibercompile.rs:63,72` | Scheduling quantum + throttled diagnostics, not a liveness device |
| `BLOCK_ON_WATCHDOG_NS` 30 s | `wasm/mod.rs:282` | Wedge alarm (also class D) — errors loudly |
| `INTX_WAIT_BOUND_NS` 2 s | `wasm/pci_provider.rs:78` | The SPEC awaits-are-bounded rule; deadline-rated wake, typed error |
| USB spec waits: 10 ms reset recovery, 2 ms address settle, endpoint interval placement | `crates/eo9-ohci/src/enumerate.rs:27-29`, `driver.rs:836-841` | Honest USB-mandated obligations — but see the ambient-cadence flag below |
| net.l4 deadlines: RECV 4 s / CONNECT 6 s / SEND_FLUSH 1.5 s / DHCP 20 s | `net-l4-over-l2/src/lib.rs:231-252` | Honest wall-clock windows (the round-cap conflation bug is FIXED in this code — reset-on-tick frozen-clock backstop, lines 236-245). The *waiting between checks* is A2's busy-pump, though |
| QEMU bring-up handshakes: virtio reset spin (`virtio_blk.rs:246-254`), OHCI reset/ownership/port-reset bounds (`driver.rs:115-126`), rtl8125 reset/quiesce/PHY bounds (`net-rtl8125/src/lib.rs:159-192`), timer calibration / GIC / RTC spins (`arch/*/timer.rs`, `gic.rs:292`, `rtc.rs:44`) | one-shot, boot/bring-up | Bounded hardware settle handshakes — fine |
| `async_demo` sleepy canary (50 ms) | `wasm/async_demo.rs:31` | Demo/test instrument |

**Implemented as ambient cadence rather than explicit deadline (flag for the planner):**

* **OHCI frame-counted time** — the drivers hold no time capability by design
  (`driver.rs:10-11`), so every honest timed obligation (10 ms reset recovery, 2 ms
  settle) is *counted by spinning HcFmNumber register reads* at up to
  `FRAME_POLL_LIMIT_PER_MS` = 50,000 host calls per counted millisecond
  (`driver.rs:121`): waiting out one reset recovery can burn ~500 k host calls where
  the doctrine's model wants one armed timer. Same family: rtl8125's autoneg link wait
  is `LINK_WAIT_LIMIT` = 2 M PHYstatus host-call polls (~seconds of spin,
  `net-rtl8125/src/lib.rs:176,997`). These are round-counted time proxies — the exact
  shape the GAPS `wait_until` entry warns generalizes. Tension to surface: the
  "drivers hold no time capability" rule vs. "time T → task X wants a timer, not a
  spin". Options: a narrow `frame-wait`/`sleep` surface for drivers, or accept the
  bring-up-only spins and convert just the steady-state ones (A1/A2 do the latter).
* **Kernel svc restart dueness** is re-checked on every drive pass
  (`svc.rs:732-758`) — but each pass also lowers `request_timer_wake`, so the wake is
  deadline-precise; acceptable.

---

## Class C — external-bug workarounds (the QEMU chardev kick class)

| Item | Where | Scoped? | Verdict |
|------|-------|---------|---------|
| **QEMU wedged-chardev kick**: after 1 s of total input silence, one dummy data-register read (QEMU calls `accept_input` unconditionally) revives a wedged character feed | `arch/aarch64/uart.rs:346-385` (`QUIET_BEFORE_KICK_NS`), same pattern `arch/riscv64/uart.rs:110-120`, `arch/x86_64/uart.rs:121-123` | **Documented and timed, but NOT profile-scoped**: the kick also executes on `board-opi5plus` (aarch64), where the QEMU bug cannot exist — with the documented one-keystroke-loss window along for the ride ("harmless stale read on real hardware" mitigates but the doctrine says *scoped to the affected profile*) | Fix: cfg-gate the kick (not the FIFO scavenge) to non-board profiles. **S** |
| **PL011 ack-then-drain ordering** (QEMU model latch behavior) | `arch/aarch64/uart.rs:320-336` | Behavioral, not timed; thoroughly documented; correct on hardware too | Fine as-is |

---

## Class D — detectors (the enforcement arm)

| Detector | Where | Fires loudly? | Rescues? |
|----------|-------|---------------|----------|
| `liveness_finding` — stranded input / stranded intx / stranded runnable | `wasm/mod.rs:437-491`, callers in `shell.rs:154,347`, `block_on` (`mod.rs:512-517`) | **Yes** — `kprintln!("liveness: …")`, 1st + every 16th; the check gates assert no-`liveness:` | Report-only ✓ (the intx probe uses `load`, never consumes — `pci.rs:94-100`) |
| `block_on` wedge watchdog (30 s) | `wasm/mod.rs:280-282,520-525` | Yes — typed error ends the operation | Report-only ✓ |
| Usermode park-backstop finding (services only; fired-waker + restart-due exclusions) | `crates/eo9/src/providers.rs:1278-1325` | Yes — stderr `liveness:` line, 1st + every 16th; suite-asserted. The foreground-arm removal is correct and documented at length (delivered-late ≠ missed edge) | Report-only ✓ |
| Frozen-clock backstop in `wait_until` | `net-l4-over-l2/src/lib.rs:236-245,788-801` | **No** — a frozen-clock giveup surfaces as a plain `timed-out`, indistinguishable from an honest deadline expiry | Bounded-exit, not a reporter. Minor: emit a distinct typed/console marker when `frozen_rounds` trips (it should only ever fire under a frozen test stub). **XS** |
| **`scavenge_rx`** — idle-path FIFO scavenge + chardev kick | `arch/*/uart.rs` (`scavenge_rx`), called from `idle_wait` (`wasm/mod.rs:429`) | **Partially.** The finding only rates when the wake was **backstop-rated** (`mod.rs:437-447`); `scavenge_rx` runs and *moves bytes on every idle wake*, so bytes rescued on an Event- or Deadline-rated wake (the common case whenever any sleep is pending) are rescued **silently** | **FLAGGED — the doctrine's named case.** The rescue half (moving FIFO bytes the IRQ path owed) is a crutch. What breaking the rescue would expose: (a) the board pre-IER bug class — before the `line_init` IER fix this scavenge was *the only input path* on the board (GAPS 64-byte truncation entry); (b) the QEMU wedge becoming permanent deafness instead of a ≤1 s hiccup; (c) any residual IRQ-drain race. Minimum doctrine-compliant fix: count and report scavenged bytes on **all** wake kinds (same counter, same rate limit), keep the move; once transcripts stay quiet across profiles, consider demoting the move to report-only behind the board/QEMU split. **S** |

---

## Class E — host-harness polling (acceptable; for completeness)

All in `xtask/src/main.rs`: 300 s flat step timeouts (`GPU_STEP_TIMEOUT:3714`,
`USB_STEP_TIMEOUT:4317`) — GAPS already records the want for progress-aware
no-progress alarms instead of flat bounds (check-telnet false kill, 2026-06-08);
25 ms serial drain sleeps (`:3870,4010,4418,4672`); 100 ms post-kill settles
(`:3872,4420`); 250 ms QMP keypress pacing deliberately ≥ the guest's endpoint
polling interval (`:4555-4557` — this one *follows* A1: when HID goes
interrupt-driven the pacing comment should be revisited, not the value); 30 s QMP
socket read timeouts (`:4143,4517`). Guest-side gate instruments `hidcheck`
(`POLL_PACE_NS` 2 ms) and `usbcheck` (100 ms watch, settle sleeps) mirror `usb.kbd`
and ride A1/A4's fix.

## Inventory owned by area/34 (not touched here)

`wasm/mod.rs`: `IDLE_WAKE_INTERVAL_NS` 10 ms child-running cap (the fuel-yield
crutch — deletion in flight), `IDLE_BACKSTOP_NS` 1 s (restructure),
`MIN_WAKE_NS`, the `WakeKind` cap/deadline rating, detector extension;
`shell.rs` both drive loops (`:120-180,320-400`); `shellexec.rs` drive paths and
the `:536` child-turn idle backstop. The A3 usermode item deliberately sequences
behind this lane.

## GAPS entries this audit subsumes or updates

* **"Board console input truncates at exactly 64 bytes" (2026-06-08)** — the fix is
  in-tree: the board `line_init` IER write enables the DW-APB RX interrupt and the
  uart module documents it as "the one-IER write that fixed the 64-byte console
  truncation" (`arch/aarch64/uart.rs:289-291`). Entry should be closed by the board
  lane after one bench confirmation (this audit lane cannot touch serial).
* **"wait_until conflates the frozen-clock backstop with wall-clock windows"
  (2026-06-08)** — implemented in `net-l4-over-l2/src/lib.rs:236-245` (reset-on-tick
  frozen-clock counter; waits are wall-clock-bounded). Entry can be closed; the
  *generalized* concern (round caps as time proxies) survives in the class-B
  ambient-cadence flag above (OHCI frame counting, rtl8125 link wait).
* **"Backstop detector first real hit: stranded runnable on the board"
  (2026-06-08)** — stays open; A2 is the most likely workload class (polled driver
  waits keep children runnable; any parked moment that misses a wake edge surfaces
  exactly this way). The A2 board leg's INTx plumbing is the structural fix.
* **plan/12 D59** (net.virtio interrupt receive) — elevated to a doctrine bug as A2;
  new GAPS entry added.
* **disk-iops audit rung 3** — already tracks A5; no new entry.
* **"Check gates can leak QEMU / flat step timeouts"** — class E here; unchanged.
