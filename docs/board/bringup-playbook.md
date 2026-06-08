# Hardware bring-up playbook

What it takes to get Eo9 from "board in hand" to "first program output" on a new
SoC. Distilled from the Orange Pi 5 Plus (RK3588) bring-up — every example below is a
real register, command, or incident from that effort, but each lesson is framed as
what the *next* board port needs. Deeper companion docs:

* [orange-pi-5-plus.md](orange-pi-5-plus.md) — the concrete RK3588 board profile + status.
* [rk3588-pcie.md](rk3588-pcie.md) — non-ECAM PCIe config access (the DesignWare shim).
* [gfx-simplefb.md](gfx-simplefb.md) — pixels via the firmware framebuffer, no display driver
  (and [hdmi-simplefb-plan.md](hdmi-simplefb-plan.md) for how it actually went on this board).
* [usb-ohci-plan.md](usb-ohci-plan.md) / [usb-boot-demo-plan.md](usb-boot-demo-plan.md) —
  the USB input and USB-boot lanes.
* `boards/opi5-serial-loader/` — the serial loader stub and host tools (the transport).
* [../spikes/hvf-cntv.md](../spikes/hvf-cntv.md) — the MMIO/ISV lesson referenced in §1.4.

## 1. Prerequisites — know these before touching the hardware

### 1.1 Boot chain anatomy

Every Arm SBC boots through a chain; know each stage's job and what it hands the next:

```
BootROM (mask ROM, in-SoC)         picks a boot medium (RK3588: SPI-NOR → eMMC → SD),
                                   loads the first-stage loader into SRAM
  → TPL/SPL ("idbloader")          DRAM init, loads the next stage into DRAM
    → TF-A BL31 (EL3 resident)     secure monitor; stays alive, serves PSCI via SMC
      → U-Boot proper (EL2)        drivers, environment, the interactive prompt
        → us                       the Eo9 kernel image
```

What matters downstream:

* **Which exception level you receive.** On the Orange Pi 5 Plus, TF-A hands U-Boot
  EL2h (visible in the TF-A boot log: `SPSR = 0x3c9`), so the kernel image needs an
  EL2→EL1 trampoline: zero `CNTVOFF_EL2` (so the virtual counter equals the physical
  one), set `HCR_EL2.RW` (EL1 is aarch64), park the FP/CP15 traps, seed
  `SCTLR_EL1 = 0x30D00800`, `eret` to EL1h. An emulator that enters your ELF at EL1
  never executes any of this — the trampoline is board-profile code.
* **MMU/cache state at handoff.** Under U-Boot's `go`, the MMU and caches are ON,
  running U-Boot's translation regime; under `booti` they are off. Your entry code must
  be correct in *both* regimes (see §3).
* **Secondary cores** are usually parked by TF-A; bring-up is single-core. Record the
  boot core's MPIDR on day one even if you don't use it.
* **PSCI is the power interface** — `SYSTEM_RESET`/`SYSTEM_OFF` via SMC (not HVC; the
  conduit is in the DTB's `/psci` node, `method = "smc"` on real TF-A).

### 1.2 The constants to gather

A v1 board profile is compiled-in constants. The full list, with the value from this
bring-up as the worked example:

| Constant | RK3588 / Orange Pi 5 Plus value | Authority |
|---|---|---|
| DRAM base / size | bank base `0x0020_0000`, 8 GiB LPDDR5 | `bdinfo` |
| Link/load address | `0x0020_0000` (run-in-place) | yours, derived from DRAM base |
| Console UART base + type | DW-APB UART2 at `0xfeb5_0000`, stride 4, width 4 | DTB `serial@…`, then a live write |
| UART clock / baud | 24 MHz, 1.5 Mbaud (divisor 1) — *don't reprogram it* | DTB + vendor convention |
| GIC version + bases | GICv3: GICD `0xfe60_0000`, GICR `0xfe68_0000` (frame 0 = boot core) | DTB + PIDR2 read |
| Device MMIO window | `0xfd00_0000`–`0xfe9f_ffff` peripherals (one Device GiB covers it) | SoC TRM + DTB |
| Entry EL | EL2 (SPSR `0x3c9` in the TF-A log) | boot log |
| PSCI conduit | SMC | DTB `/psci` |
| Timer frequency | `CNTFRQ_EL0` = 24 MHz | read it, don't assume |
| Control FDT address | `0xeb9f6c38` | `bdinfo` (see verification note below) |
| Watchdog | DW-WDT at `0xfeaf_0000`, 24 MHz `tclk` | SoC dtsi + TRM |

### 1.3 How to verify each from the vendor U-Boot prompt

The DTB is the authority; pre-arrival docs are the map. Mark every unconfirmed value
**[verify-on-board]** and burn the tags down at the prompt on day one:

* **`bdinfo`** — DRAM banks, relocation addresses, and the control FDT pointer
  (`fdt_blob`). Caution from this bring-up: the `fdtcontroladdr` *environment variable*
  was unset on the vendor U-Boot — `bdinfo`'s value is the one to trust.
* **`fdt addr <fdt_blob>; fdt print /chosen`** (and `/serial@…`, `/psci`, each
  `pcie@…`) — walk the live tree for every address, clock, and interrupt you plan to
  hardcode. `fdt print` of the UART node gives you `reg`, `reg-shift`, `reg-io-width`,
  and the clock — exactly the parameters a 16550-family driver needs.
* **Identification-register reads with `md`** — the strongest verification is reading
  a peripheral's ID register live. The GIC version probe: `md 0xfe60ffe8 1` reads
  GICD `PIDR2`; the `0x3b` we got decodes ArchRev = GICv3. Same trick for the
  redistributor frame (`0xfe68ffe8`). If an `md` to a documented address hangs the
  prompt, the address (or an ungated clock) is wrong — better to learn that
  interactively than inside your kernel.
* **Console UART**: it's verified by the very prompt you're typing at — note the baud
  (1.5 Mbaud is the Rockchip convention) and *never reprogram the line*. U-Boot left
  LCR/DLL/FCR correct; your driver should touch only LSR/THR/RBR on day one.
* **Capability inventory**: `help` — know which load paths exist before designing the
  transport. This vendor U-Boot (2017.09) had `mm`, `go`, `crc32`, `fatload`, `booti`,
  `ums`, `rockusb` — but **no** `loadx`/`loady`, which ruled out xmodem and shaped the
  custom loader in §2.

### 1.4 Pre-arrival work that pays off

Everything not board-blocked should be done and verified under emulation first: the
flat-image header + objcopy path, the link-address change, the UART driver
parameterization, the `ConfigAccess` refactor for non-ECAM PCIe (rk3588-pcie.md), the
FDT parser (gfx-simplefb.md). One subtlety worth importing from the virtualization
work: device MMIO must go through single general-purpose-register accessors
(`ldrb/ldrh/ldr`, no SIMD/FP, no writeback) — `read_volatile` constrains *that* an
access happens, not *which register class* performs it, and a compiler-chosen
`ldr s0, [x8]` is invisible under software emulation but fatal under a hypervisor
(see hvf-cntv.md). Real silicon at EL1-without-a-hypervisor doesn't care, but the same
accessors keep one code path correct everywhere.

## 2. The transport doctrine — serial first

### 2.1 Why serial

The debug UART is the one channel that is *always present, already configured, and
free of host-side policy*. Every alternative failed or threatened to on this bring-up:

* **SD/USB-stick sneakernet** died on host policy — the development host refused to
  mount external media (enumeration worked, mounting didn't). One image per walk to
  the bench is also no dev loop at all.
* **USB gadget modes** (`ums`, `rockusb`, fastboot) interact with bench electrics
  (§5) — plugging the OTG port into the host's hub killed the serial console.
* **TFTP** needs the NIC up, which on this board means PCIe, which is a whole
  post-bring-up project (rk3588-pcie.md).

A 13 MiB image at 1.5 Mbaud is ~87 s. That is a perfectly serviceable unattended dev
loop, and it works on day zero.

### 2.2 The loader-stub pattern

You cannot stream a fresh image at every iteration *through the U-Boot prompt* (no
`loadx`, and prompt-paced `mm` typing of a 13 MiB image would take hours). So
bootstrap once, then go raw:

1. **Bootstrap a tiny stub over the prompt.** The stub is 1,060 bytes — 265 words —
  typed into RAM via prompt-paced `mm.l` at `0x0400_0000` (~10 s), then cross-checked
  with U-Boot's own `crc32` command before ever jumping. Budget the stub ≤ 4 KiB:
  small enough that prompt-paced entry stays in seconds and the whole thing is
  auditable as a disassembly listing.
2. **The stub speaks a raw framed protocol** on the same UART, forever:
  `"EO9L"` magic + 24-byte little-endian header (`load_addr: u64, length: u64,
  x0_value: u64`) + payload + CRC-32 (IEEE). It answers `k` per 64 KiB received
  (progress), then `K` (CRC ok → jump with `x0 = x0_value`) or `E` (mismatch → back
  to idle). A ~3 s mid-transfer stall answers `T` and re-arms. **A failed transfer
  never needs hands** — that property is the whole point.
3. **Ack design**: at 1.5 Mbaud a byte takes ~6.7 µs and the stub's per-byte service
  loop is ~1 µs, so the 32-byte RX FIFO cannot overflow — no byte-level flow control
  needed. The host *sender*, however, should use the 64 KiB acks as windowed flow
  control (stream freely until > 512 KiB ahead of the last ack, alarm after 10 s of
  no acks): the first naive sender alternated chunk-write with a blocking 50 ms ack
  poll and turned an 87 s transfer into 182 s. Let the wire set the pace.
4. **Defensive bounds**: reject `length` of 0 or > 1 GiB and any payload overlapping
  the stub's own 64 KiB home — a corrupt header must not become a self-overwrite.

### 2.3 Treat vendor boot commands as hostile

Case study: the stub carries a correct arm64 `Image` header, and `booti 0x04000000`
should have launched it. Instead the vendor U-Boot **data-aborted inside itself**
(ESR `0x96000010`, PC `0x292f60` relocated — its Android/FIT image heuristics
choking on a minimal image), and *its own recovery reset then failed*, wedging the
board until a physical power cycle.

The doctrine that fell out:

* **`go <addr>` is the trusted launch primitive** — it does nothing but jump. The
  cost: the MMU/caches stay on (a cache-maintenance obligation, §3) and `x0` is not
  the DTB (pass the real FDT address in your own protocol instead — the stub's
  `x0_value` field exists precisely because of this incident).
* Vendor `booti`/`bootm` paths run large heuristic codebases you didn't audit, on
  forks years behind mainline. Use them only as an *experiment with a recovery plan*,
  never as the load-bearing path.
* Keep the standard image header anyway — it costs 64 bytes and works under mainline
  U-Boot and any future sane bootloader.

### 2.4 When the alternatives are worth it

After the serial loop is solid and the bench electrics are understood: a direct (no
shared hub) OTG cable enables `rockusb`/fastboot-class transfers and — more
importantly — the SoC's mask-ROM USB recovery mode, which is the free unbrick path.
TFTP becomes attractive only once the NIC works under *your* kernel, at which point
you have already booted many times without it.

## 3. Cache coherency at handoffs — the hard-won chapter

Three incidents, one rule. Read this section before writing any loader or entry code.

Background, in one paragraph: `dc cvau` cleans a D-cache line to the **Point of
Unification** — far enough that *instruction fetch in the same cache regime* sees the
data. The **Point of Coherency** (PoC) is further: effectively DRAM, what an agent
with caches *off* observes. A line cleaned to PoU can still be dirty above the PoC.
`dc civac` cleans+invalidates to PoC. Cache maintenance ops take a *virtual* address
but act on the line holding the *physical* address behind the current translation.

### Incident (a): the loader's PoU sweep — a silent wild jump

The stub receives the payload under `go`, i.e. through U-Boot's live EL2 D-cache, and
originally swept with `dc cvau` before jumping. That is the textbook "make freshly
written code executable" sequence — and it is *wrong here*, because the payload
kernel immediately drops to EL1 with `SCTLR_EL1.{M,C,I} = 0`: every fetch and data
access now reads DRAM at the PoC, where any line cleaned only to PoU may still be
stale. Symptom: nothing. No abort, no garbage output — a silent wild jump into stale
bytes. The fix is `dc civac` + `dsb sy` (under `booti`-style cache-off loading every
op degrades to a cheap clean-line no-op, so the PoC sweep is safe everywhere).

Corollary: **the kernel must not trust any loader's sweep.** The entry code does its
own `dc civac` over the whole footprint — image *plus `.bss` and the boot stack* —
then `ic iallu`, before the EL2 drop. Sweeping `.bss`/stack matters independently:
stale *dirty* lines left over from the firmware's earlier use of low DRAM can write
back at any moment **over** your cache-off stores (your early translation tables live
in `.bss`). Clean+invalidate evicts them for good.

### Incident (b): the stale-bootargs FDT — cacheable-write/Device-read aliasing

Proven live on the board: U-Boot edits its control FDT in place (`fdt set /chosen
bootargs …`) through its own *cacheable* mapping, and those edits were still sitting
in dirty D-cache lines when it jumped. The kernel maps that physical range as
*Device* (non-cacheable) memory, so its reads went straight to DRAM — and saw the
pre-edit tree: a `/chosen` with no `bootargs` at all. The interim bench workaround
(forcing cache pressure with `crc32 0x10000000 0x1000000` at the prompt before `go`)
confirmed the diagnosis and is exactly the kind of fragile magic a playbook exists to
eliminate.

The durable fix leans on the VA-acts-on-PA property: sweep `dc civac` through *your
own Device VAs* and you evict exactly the lines the firmware dirtied under its
different, cacheable mapping of the same physical bytes. Ordering subtlety: the FDT
*header itself* may be stale, so `totalsize` cannot be trusted until its own line is
swept — sweep the first 8 bytes, read magic+totalsize, bounds-check against a sane
cap (1 MiB; the measured control FDT is ~170 KiB), and only then sweep the full range
and copy. The copy must be **byte-volatile**: the compiler may merge or vectorize
ordinary slice reads, and an unaligned multi-byte access on Device-nGnRnE memory is
an alignment fault.

### The rule

> **Every handoff between MMU/cache regimes sweeps the shared bytes to PoC, on the
> consumer's side too.** Producer sweeps after writing (`dc civac`, `dsb sy`, plus
> `ic iallu`+`isb` if the bytes are code); consumer sweeps before reading anything it
> didn't write under its own current regime. Cache ops by VA act on the PA, so sweep
> through whatever mapping you have — a Device-VA sweep evicts cacheable-mapping
> dirt. Belt *and* suspenders is correct here precisely because the failure mode is
> silent.

## 4. Loop-safety doctrine — nothing boots without it

The bring-up dev loop runs unattended. Every kernel outcome — success, panic, hang —
must land the board back at the bootloader prompt with a legible trace. Build these
*before* the first jump, not after the first mystery hang:

1. **Exit = reset.** End-of-run calls PSCI `SYSTEM_RESET` (`0x8400_0009`) on the
   board (not `SYSTEM_OFF` — a powered-off board needs hands). Before the SMC, drain
   the UART transmit path (bounded spin on LSR `TEMT`) so the outcome line survives
   the reset.
2. **Panic = marker + drain + reset.** The panic report carries a grep-stable marker
   line (`EO9-PANIC`) so host-side tooling can classify the boot mechanically, then
   flows into the same drained reset.
3. **Hardware watchdog, armed early.** RK3588 numbers: DW-WDT at `0xfeaf_0000`,
   `TORR TOP=13` ≈ 22.4 s at the 24 MHz watchdog clock, response mode 0 (direct SoC
   reset), pat by writing `0x76` to `CRR`. Pat it from the scheduling chokepoints —
   every drive-loop pass and every idle wake — so neither a hot nor a parked kernel
   starves it; any hang returns to the prompt in ≤ ~22 s. Two doctrine points:
   * Arming is best-effort with **loud verification** — read back the enable bit
     *and observe the counter moving*; if firmware left the clock gated, print
     `wdt: arm FAILED` and boot on rather than blind-poking clock-gate registers.
   * It is a dead-man's switch, not a progress backstop — it never makes anything
     advance, it only bounds how long a hang can hold the bench.
4. **Boot-bisection beacons.** Single raw characters banged straight at the UART THR
   (poll LSR THRE, store — pre-MMU, pre-stack, two scratch registers) at every early
   stage: `A` image entry, `B`/`b` after the EL2 drop / EL1-direct entry, `C` kmain
   reached, `H` banner imminent, `E` MMU on, `F` heap up, `G` watchdog armed, `D` FDT
   parse returned. When a boot dies silently, the last surviving letter pinpoints the
   dead stage with zero debugger access — this is exactly how the first silent hang
   on this board was bisected. The beacons stay in the production board image: they
   cost nothing and the prefix reads as a boot signature.
5. **Periodic heartbeat.** `hb <uptime-ms>` every ~5 s from the watchdog-pat
   chokepoint, so a live-but-quiet kernel is distinguishable from a hung one within
   seconds on any serial capture.
6. **Bounded host-side tails with stall alarms.** The console tail exits after N
   seconds of silence (default 20) instead of holding the port forever; the sender
   alarms at 10 s without an ack. And **always tee the capture to a timestamped
   log** — the first jump on this board produced output that was lost forever
   because a killed sender's stdout was block-buffered. Line-buffer *and* tee.

## 5. Bench electrics — the analog failure modes

Software people lose whole days to these. All three bit this bring-up:

* **UART back-powering.** A common FTDI-class adapter drives TX idle-high at logic
  level; with the board "off", that idle-high line phantom-powers part of the SoC
  through the RX pin's protection diodes. Result: a board that never properly
  power-cycles, brown-out weirdness, and resets that don't reset. Workaround: assert
  a **break condition** (hold TX low) from the host while the board is supposed to be
  off, so the line stops sourcing current. End-state fix: a buffered/isolated adapter
  so the console can stay connected across hard power cycles without feeding the
  board.
* **OTG / hub ground interactions.** Plugging the board's USB-C OTG port into the
  *same hub* as the serial adapter killed the console outright (back-power/ground
  path through the shared hub). Rule: the OTG cable goes directly to the host or
  stays unplugged; never share a hub between the console adapter and any
  board-connected USB.
* **Power-supply pickiness.** This board wants 5 V/4 A on its power USB-C; many USB-PD
  chargers won't negotiate that profile and the board sits completely dead — no LEDs,
  no BootROM, indistinguishable from a bricked unit. A swapped charger cost a bench
  day. Keep the known-good supply labeled; when the board is "dead", check supply and
  LEDs before suspecting anything you did.
* **The eventual nice-to-have: remote power control.** Every wedge this bring-up
  produced was cured by a power cycle, so a network-controlled relay (or, nicer, a
  small MCU pressing the PMIC power button, which gives a clean PMIC-sequenced
  start) buys more unattended autonomy than a JTAG probe would. Debug doctrine:
  serial beacons + watchdog + power control cover the actual failure modes; JTAG is
  rarely the bottleneck.

## 6. The checklist — board in hand to first program output

Each step has a verification gate; do not advance past a failed gate.

1. **Before the board arrives**: flat-image header + objcopy path proven under
   emulation; link address parameterized; UART driver parameterized
   (stride/width/no-divisor-touch); a known-good serial adapter *for the board's baud
   rate* (1.5 Mbaud needs FT232-class; cheap 115200-only adapters garble).
   *Gate: the same image boots under the emulator via its flat-image path.*
2. **Power + console**: known-good supply, console adapter only (no OTG, no shared
   hub). *Gate: the bootloader banner at the expected baud, a responsive prompt.*
3. **Constants sweep at the prompt**: `bdinfo`; `fdt addr <fdt_blob>` + `fdt print`
   of `/chosen`, the UART node, `/psci`, GIC, watchdog, `pcie@…`; `md` of GICD/GICR
   `PIDR2`; `help` inventory. Record everything; burn every [verify-on-board] tag.
   *Gate: zero unverified constants in the board profile.*
4. **Transport up**: bootstrap the loader stub (`mm`-paced + `crc32` cross-check),
   launch with `go` only. *Gate: a deliberate bad-CRC probe answers `E` — the stub is
   alive and re-arms itself.*
5. **Loop-safety in the image before the first jump**: exit=SYSTEM_RESET with TX
   drain, panic marker, early-armed watchdog with loud verification, beacons,
   heartbeat; host capture line-buffered + teed, bounded tails.
   *Gate (emulation): the board-profile image still passes the standard battery with
   all of it compiled in.*
6. **First jump — minimal image** (smallest store, one known program): send via the
   stub with the real FDT address as `x0`. *Gate: the full beacon prefix and the
   banner; on silence, bisect by last letter (`A` missing = jump/image/UART; stuck at
   `B` = the EL2 drop; …) — and reread §3 first, both wild-jump classes live there.*
7. **Watchdog validation**: deliberately starve it. *Gate: SoC self-resets to the
   prompt in about the configured timeout.*
8. **End-of-run validation**: let the program complete. *Gate: outcome line, then
   `SYSTEM_RESET (back to U-Boot)`, then the prompt returns by itself — the
   unattended loop is closed.*
9. **Full image**: the complete store, interactive shell over serial. *Gate: the
   shell accepts keystrokes (polled input latency is acceptable day-one; wiring the
   UART interrupt is a follow-up, not a blocker).*
10. **Only now, the peripherals**, each its own session with its own doc: PCIe
    enumeration (rk3588-pcie.md), framebuffer (gfx-simplefb.md), and onward per the
    board doc's hardware-goals roadmap.

## 7. After first light — incident classes from the running system

First light moves the failure modes up the stack: the board boots reliably, and the
incidents start coming from the kernel, the network, and the bench process itself.
Five classes from this bring-up, each with the workaround that held the bench and the
real fix it points at.

### 7.1 Console input truncates at the RX FIFO depth

Any console line longer than 64 bytes truncated at exactly byte 64 — deterministic
(two identical commands mangled at the same column; a 64-char command lost only its
newline, byte 65). The DW-APB UART RX FIFO is 64 bytes deep, and the board profile's
RX interrupt path never drains it: input reaches the shell only when the kernel's
idle backstop scavenges the FIFO — the backstop's own `stranded input` line named the
mechanism (accountability lines earn their keep). Between scavenges the FIFO
overflows silently. QEMU never showed it: the emulated console path has no FIFO
reality. Bench workaround: type in sub-FIFO chunks with pauses long enough for a
scavenge (eosh_cmd.py: 40-byte chunks, 6 s pauses, a redundant trailing newline).
Real fix (GAPS'd, kernel lane): drain the FIFO from the RX interrupt (or an adequate
poll cadence) so the backstop goes back to being a detector, not the input path. The
lesson: **an emulator's console is not your UART** — FIFO depths and drain paths
exist only on silicon, so "type a long line" is a real bring-up test.

### 7.2 Long synchronous kernel work starves the drive loop — and the watchdog

A 486 KiB composed-component compile ran synchronously inside one host call; for the
whole compile no drive-loop pass ran — no heartbeat, no watchdog pat — and at
codegen+18.2 s the 22.4 s DW-WDT reset the board mid-compile, reproduced 2-for-2.
The fix (merged): on-target codegen runs on a fiber and yields every ~5 ms of compile
work; between slices the kernel pumps children/services, pats the watchdog, and
prints a throttled `codegen: still compiling` line. The pat stays honest per the §4
doctrine — it fires only after a slice of real progress plus a scheduling pass, so a
compile wedged inside one function still resets the board. The general rule: **any
synchronous kernel work longer than the watchdog period is a hardware reset waiting
for a big enough input** — slice it or budget it. Watch the same shape guest-side: a
guest hot-spinning on synchronous host calls is invisible to cooperative scheduling
for its entire bound (the stranded-runnable backstop's first real hit, GAPS'd).

### 7.3 Never cycle the board while a foreground server owns the prompt

A power cycle was started while `telnetd` was serving on the eosh foreground. The
serial `poweroff` was swallowed (a foreground child owns the console; eosh is not
reading), Ctrl-C did not kill the server, and the scripted U-Boot command sequence
poured into the kernel console — the transfer stalled at 0%. Compounding it: a
`poweroff` typed in a *telnet* session no-ops silently (the session stack lacks the
power capability and refuses without saying so — GAPS'd as a UX bug). Proven
recovery: **burn the session budget** — telnetd exits after serving `--sessions N`,
so quick scripted `exit` sessions (`nc`) walk it to completion, the serial prompt
returns, and serial `poweroff` works; wait a few seconds after the last session (an
immediate poweroff raced the teardown and was swallowed too). The rule: before any
cycle, check what owns the console — a foreground server means the bench's normal
channel is gone.

### 7.4 Verify protocol-level identity claims before trusting any filtering theory

Days of "TX is blocked by the switch" theorizing dissolved when a probe finally
printed its own sender fields: every ARP the board had ever sent carried the driver's
hardcoded emulator-era source IP (10.0.2.15) — off-subnet, so the network rightly
ignored it, and the bench evidence ("the gateway never replies, the Mac never learns
the board") was exactly what *perfect* TX would have produced. The lesson
generalizes: **before reasoning about why the other end doesn't answer, print the
identity fields your frames actually carry** (source MAC/IP, VLAN, advertised
capabilities) — one diagnostic line kills whole theory families. The probe grew
`--source` and a counts line on every exit so each run advances a round.

### 7.5 Managed-LAN reality: source validation against DHCP-snooping bindings

The office LAN validates traffic source IPs against its DHCP-snooping bindings: an
address the switch has no lease binding for is silently filtered — no ICMP, no log,
dead air indistinguishable from a driver bug (which is why §7.4's discipline must
come first; the two effects were entangled in the same "TX blocked" mystery).
Consequence: **DHCP addressing is canonical on a managed network** — static
addressing is a lab convenience the switch may simply refuse, and it can appear to
work in some flows while silently failing in others, depending on what the policy
validates. The stack's `--address dhcp` exists for this reason; the printed
`dhcp acquired` line is the operator's source of truth for the board's address. The
broader rule: the network is a managed system with its own policy agents — bring-up
debugging must hold "the switch filters us by policy" and "our frames are wrong" as
separately falsifiable theories, and the falsifiers are cheap (tcpdump on the same
VLAN, a second host's ARP table, an unprivileged `nc` beacon listener).
