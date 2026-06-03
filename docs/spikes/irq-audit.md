# Interrupt-edge audit: sequencing invariants, site by site

Commissioned after the paste-freeze fix (plan/12 entry 70) exposed a *sequencing* bug class:
code where the order of (read state / clear latch / mask / unmask / EOI / claim / complete /
consume count) is load-bearing, the compiler cannot see it, and a violation has consequences
ranging from one spurious wake-up to a permanently deaf console. This audit walks every
interrupt-edge site in the kernel (all three architectures, the wasm-layer INTx/timer paths)
plus the guest-side virtio ISR acknowledgements, states each site's invariant precisely,
checks the current order, and asks: if the invariant were silently violated by a future
refactor, what happens, and does anything rescue it?

Verdict vocabulary:

* **SOUND** — order is correct and either the hardware semantics are forgiving of
  reordering, or a structural pattern (masked-`wfi`, sticky counters, bounded fallbacks)
  makes the order non-load-bearing.
* **SOUND-BUT-FRAGILE** — order is correct *today*, the order is load-bearing, nothing but a
  comment enforces it, and the failure mode of a reorder is severe (deafness, a stranded
  source) with no or weak rescue.
* **BUG** — order is wrong today. (None found.)

The companion piece, `irq-ownership.md`, evaluates whether Rust ownership/typestate can turn
the FRAGILE entries' comments into compile errors, with a working prototype on the PLIC.

## A. aarch64

| # | Site | Invariant (precise) | Current order | Rescue if violated | Verdict |
|---|------|---------------------|---------------|--------------------|---------|
| A1 | PL011 RX handler `drain_rx` (`arch/aarch64/uart.rs`) | The `UARTICR` write-1-to-clear must precede the FIFO drain. QEMU's model latches INT_RX only on the FIFO's empty→occupied transition; clear-after-drain races a byte landing between the final `RXFE` check and the clear — the clear wipes the fresh latch, the byte strands, no transition can ever re-occur, console deaf (the paste-freeze bug). | ack-then-drain ✓; the drain loop re-checks `RXFE` per iteration so mid-drain arrivals are consumed. | The idle scavenger (`scavenge_rx`) moves stranded FIFO bytes into the ring on every idle wake, so even a reintroduced reorder degrades to "input arrives with up to ~1 s latency", not deafness. | **SOUND-BUT-FRAGILE** (severe history, comment-enforced order; mitigated by the scavenger backstop) |
| A2 | PL011 `enable_rx_interrupt` init | TRM ordering: `LCR_H` (FEN) before the `CR` enables; `UARTICR` clear-all before `UARTIMSC` unmask (a stale pre-boot latch must not fire into an unconfigured handler). | ✓ | A stale latch firing early is drained harmlessly by the handler (it tolerates an empty FIFO). | SOUND (init-once, forgiving) |
| A3 | `scavenge_rx` producer exclusivity | All FIFO/ring producer accesses and the dummy-`UARTDR` kick decision must execute with IRQs masked (`DAIF.I`), making this thread the sole ring producer; the kick may only fire when FIFO *and* ring are observed empty under that mask. | ✓ mask → drain → check → maybe-kick → unmask, single bracket. | None needed while bracket holds; a violated bracket = SPSC ring corruption (two producers). | SOUND (the mask bracket *is* the enforcement; short and local) |
| A4 | GIC dispatch `kirq` (`exceptions.rs`) — spurious | IAR values 1020–1023 must **not** be EOI'd (GICv2: EOIR for a spurious ID is unpredictable). | ✓ early-return before EOI. | None; consequence is UNPREDICTABLE per the GIC spec. | SOUND (single guarded return) |
| A5 | `kirq` — service-before-EOI for level sources | Each level-sensitive source must deassert (timer: `CNTP_CTL.ENABLE` cleared; UART: ICR cleared + FIFO drained) or be masked at the distributor (INTx: `ICENABLER`) **before** `EOIR`, else the still-asserted line re-fires immediately: interrupt livelock. | ✓ all three arms service, then fall through to one EOI. | None — a livelock starves the boot core (watchdogs don't run from a storm). | **SOUND-BUT-FRAGILE** |
| A6 | `kirq` — IAR/EOIR pairing | Every non-spurious `IAR` read must be paired with exactly one `EOIR` write of the same value. A missing EOI leaves the interrupt active in the CPU interface: the GIC never again forwards same-or-lower-priority interrupts — timer and UART both dead, machine deaf, no rescue. An early `return` inserted between acknowledge and EOI (e.g. while adding a new source arm) is exactly this. | ✓ single fall-through exit. | **None.** | **SOUND-BUT-FRAGILE** (the linear-token shape: an obligation created by `acknowledge()` that must be consumed exactly once) |
| A7 | Generic timer arm/disable pairing | Handler `disable()`s the level PPI before EOI (A5); the executor re-arms via `arm_wake` before each `wfi`. The arm-vs-fire race is closed by masked-`wfi` (`wait_for_interrupt`: mask → arm → `wfi` → unmask) — a masked-but-pending IRQ is architecturally a `wfi` wake event, so a delivery in the pre-halt window still wakes. | ✓ | Pattern is self-rescuing (the backstop cap bounds any missed wake). | SOUND |
| A8 | INTx mask-on-fire (`kirq` arm) + `IntxWait` (`wasm/pci_provider.rs`) | Handler: mask at the distributor before EOI (A5), then count the delivery (sticky `AtomicU64`). Wait poll: **unmask → take → deadline-check → re-arm timer wake → register waker → Pending**. Take-after-unmask means a delivery landing in the unmask window is consumed by the same poll. The channel is the *sticky counter*, not the waker: a delivery between take and register is counted, and a re-poll is structurally guaranteed (busy passes call `wake_idle()` every iteration; parked passes wake within the capped halt) — worst case one backstop interval of latency. `Drop`: mask **then** drain (drain-then-mask would let a delivery land post-drain and strand a stale count for the next wait). | ✓ (reviewed at merge `e1c342c`) | Documented bounded residual: an IRQ pending-at-CPU during `Drop`, handled after the drain, leaves a stale count whose worst consequence is one spurious wait return — absorbed by the drivers' used-ring re-check + re-wait protocol. Cannot misattribute data (attribution is ring contents, plan/09 D34). | SOUND (with the recorded residual) |
| A9 | `idle_wait` ordering (`wasm/mod.rs`) | swap deadline → masked halt (arm inside the mask) → `scavenge_rx` → `wake_idle`. Scavenge-before-wake is load-bearing: bytes rescued by the scavenger must be visible to the read-line future re-polled by the same wake pass, or input waits a full extra backstop. | ✓ | A reorder costs latency (≤1 backstop), not correctness. | SOUND |

## B. riscv64

| # | Site | Invariant | Current order | Rescue | Verdict |
|---|------|-----------|---------------|--------|---------|
| B1 | PLIC claim/complete loop (`traps.rs` `IRQ_S_EXTERNAL` arm) | Every `claim()` returning a non-zero source must be passed back to `complete()` exactly once, after servicing. The PLIC gateway withholds *all further deliveries of that source* until completion: a missing complete strands the source **permanently** — UART0 dead, console deaf, no rescue. A `continue`, `break`, or `?` inserted between claim and complete is exactly this. (Completing a source not held is ignored by the PLIC, so double-complete is harmless; the dangerous direction is the missing one.) | ✓ claim → service → complete, straight-line loop body, terminates on claim()==0. | **None.** | **SOUND-BUT-FRAGILE** — the hardware hands the handler a linear token (the claim ID) and the obligation lives in code structure only. The prototype (irq-ownership.md) makes this a compile-time obligation. |
| B2 | 16550 RX drain (`uart.rs` `drain_rx`) | Drain until `LSR.DR` clear. No write-1-to-clear latch exists to wipe (RDA is reflected level-wise through IIR), so drain order vs. anything is forgiving: a byte arriving after the final check re-asserts the line; the source is claim-held, so the gateway re-forwards after `complete` — a fresh trap, nothing lost. | ✓ | Level semantics + claim-held gateway are self-rescuing; scavenger additionally backstops. | SOUND |
| B3 | 16550 init (`enable_rx_interrupt`) | `FCR` enable+clear before `IER` unmask; the FIFO clear runs after OpenSBI's boot output, preserving the "wait for the prompt before sending" boot-byte convention. | ✓ | Spurious-tolerant handler. | SOUND |
| B4 | riscv64 scavenger | Same bracket as A3, via `sstatus.SIE`. | ✓ | Same as A3. | SOUND |
| B5 | SBI timer quiet (`IRQ_S_TIMER` arm) | `set_timer(u64::MAX)` clears `sip.STIP` (SBI semantics) before `sret`, else the still-pending bit re-traps immediately. Executor re-arms before the next halt. | ✓ | None (re-trap storm) — but the arm is one straight line. | SOUND (minimal surface) |
| B6 | `wait_for_interrupt` | mask (`csrci sstatus,SIE`) → arm → `wfi` → unmask; masked-pending wakes `wfi` per the privileged spec. | ✓ | Self-rescuing pattern. | SOUND |

## C. x86_64

| # | Site | Invariant | Current order | Rescue | Verdict |
|---|------|-----------|---------------|--------|---------|
| C1 | 8259 spurious IRQ 7/15 (`traps.rs`) | A spurious IRQ 7/15 (not in-service per ISR readback) must not be EOI'd; a spurious **slave** IRQ 15 must still EOI the master's cascade line (IRQ 2). | ✓ textbook: ISR readback gate, cascade-only EOI for spurious 15. | Misordering risks a lost or phantom EOI → a wedged in-service bit. | SOUND (the subtle case is handled and commented) |
| C2 | Service-before-EOI (`ktrap`) | Timer: mask (`set_masked`) before EOI so a stale one-shot can't re-deliver; UART: drain before EOI. The 8259 lines are edge-triggered with IRR latching — an edge arriving while in-service is latched and re-delivered after EOI — so a *late* byte is never lost at the PIC; the classic 16550 deafness (line stays high, no new edge ever) requires an *incomplete drain*, and the drain loop runs to `LSR.DR` clear with the scavenger as backstop. | ✓ | IRR latching + scavenger. | SOUND |
| C3 | PIT one-shot arm (`arm_wake`) | Program the count (`pit_oneshot`) **before** unmasking the PIC line, else a stale terminal count from the previous one-shot can fire into the new wait. | ✓ program → unmask. | A stale fire is one early wake (the executor re-checks deadlines); annoying, not fatal. | SOUND |
| C4 | `sti; hlt` idiom (`wait_for_interrupt`) | `cli` → arm → `sti; hlt` adjacent: the STI interrupt shadow defers delivery past the next instruction, so no interrupt can land *between* `sti` and `hlt` — the x86 equivalent of the masked-`wfi` lost-wake closure. Separating the pair (any instruction between) reopens the classic lost-wake hole. | ✓ single `asm!("sti","hlt")` block. | None if separated — but they are welded in one asm block, which *is* structural enforcement. | SOUND (enforced by the asm block boundary) |
| C5 | PCI INTx | Not wired (`WIRED=false`); the provider answers `unsupported`, drivers use their polled paths; `mask`/`unmask` are no-ops. | ✓ | n/a | SOUND by absence |

## D. Guest-side virtio ISR acknowledgement (shares the class: read-to-clear ordering)

| # | Site | Invariant | Current order | Rescue | Verdict |
|---|------|-----------|---------------|--------|---------|
| D1 | disk.virtio `wait_for_completion` / `acknowledge_isr` | Consume the used-ring completion (`used_advanced`) **before** the read-to-clear ISR ack, and ack on *every* exit of the completion path (wait-served, already-completed, polled). Sound only at queue depth 1: with one request in flight, a pending assertion can only belong to the completion just consumed, so the read-to-clear cannot swallow another request's interrupt. The depth-1 precondition is documented at the ack site; a future depth>1 driver needs per-request attribution before this ordering is safe. | ✓ (the unconditional-ack fix, merge `4c210d5`) | ISR ack is best-effort by design: a missed/failed ack costs one spurious wait return, absorbed by the used-ring re-check; persistent failure degrades to the polled fallback (typed, bounded). Never data misattribution (plan/09 D34 drains). | SOUND (rescued; the depth-1 precondition is the fragile edge and is pinned in comments) |
| D2 | gpu.virtio (same pattern, the origin of the fix) | Same as D1. | ✓ | Same. | SOUND |
| D3 | net.virtio `AVAIL_F_NO_INTERRUPT` | The suppression flag must be published at queue init, before any buffer post, on both queues — a later write races the device's avail-ring reads. Suppression is a *hint* (virtio §2.6.7): a device may interrupt anyway; net never enables interrupts so its line stays masked at the controller and a hint-ignoring assert is invisible to net itself. | ✓ set once at init. | Residual on hint-ignoring hardware sharing a swizzled line: a sibling's wait sees spurious deliveries from net's unacked assert and degrades to its polled fallback (bounded, typed) — never deafness or misattribution. QEMU honors the hint (verified: all-three-functions-one-bus battery, zero fallbacks). | SOUND (with the recorded shared-line residual) |

## Summary

* **BUG: 0.** Nothing live found. This surface was just intensively worked (entries 67–70 and
  the driver conversions), and it shows.
* **SOUND-BUT-FRAGILE: 4 clusters** — A1 (PL011 ack-then-drain; severe history, scavenger
  mitigates), A5 (service-before-EOI), A6 (GIC IAR→EOIR pairing; **no rescue**), B1 (PLIC
  claim→complete pairing; **no rescue**). A6 and B1 share the linear-token shape: hardware
  hands the handler an obligation that must be consumed exactly once, and only code layout
  enforces it.
* **SOUND: everything else**, in three structural families that are worth naming because they
  are what *makes* them sound: (1) masked-halt brackets (`wfi`/`sti;hlt` with arm inside the
  mask) close lost-wake races architecturally; (2) sticky counters + guaranteed re-polls make
  waker registration order non-load-bearing (`IntxWait`); (3) bounded fallbacks (scavenger,
  polled completion, used-ring re-checks) convert residual races into latency instead of
  deafness or corruption. New interrupt code should reach for one of these three families
  first; the typestate question (irq-ownership.md) is about the four sites where none of them
  applies cleanly.

Accepted residuals already on the books and re-confirmed here: the `IntxWait` Drop sliver
(one spurious wait return, absorbed), the net.virtio hint-ignoring-hardware shared-line
degradation (polled fallback), and the scavenger's one-keystroke-per-silent-second trade.
