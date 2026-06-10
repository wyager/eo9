# USB Host (HID keyboard/mouse) on the Orange Pi 5 Plus — implementation plan

Goal: a USB keyboard and mouse on the bench board feeding eosh console input
(hardware goal #4 in orange-pi-5-plus.md). Plan produced 2026-06-08; owner approved
the OHCI-only v1 scope the same day. No code yet — this document is the lane design.

## 0. Hardware facts (verified)

Sources: Linux v6.12 `rk3588-orangepi-5-plus.dts` / `rk3588-base.dtsi`, the original
board-DTS submission (linux-arm-kernel, Oct 2023), the post-v6.12 USB3-enable patch
(wens, Oct 2024), and the live board's vendor control FDT captured at
`.claude/board-bringup/vendor-control-fdt.dtb`.

RK3588 host controllers as wired on this board:

| Controller | Base | GIC SPI | PHY(s) | Physical port |
|---|---|---|---|---|
| usb_host0_xhci (DWC3) | 0xfc000000 | 220 | u2phy0_otg + usbdp_phy0 | USB-C front port — BANNED on this bench (kills serial console) |
| usb_host1_xhci (DWC3) | 0xfc400000 | 221 | u2phy1_otg + usbdp_phy1 | onboard USB3 hub → both USB 3.0 type-A ports + M.2 E-key |
| usb_host0_ehci/ohci | 0xfc800000 / 0xfc840000 | 215 / 216 | u2phy2_host | USB 2.0 type-A port #1 (direct) |
| usb_host1_ehci/ohci | 0xfc880000 / 0xfc8c0000 | 218 / 219 | u2phy3_host | USB 2.0 type-A port #2 (direct) |
| usb_host2_xhci | 0xfcd00000 | 222 | combphy2_psu | unused on this board |

Key v6.12 fact: mainline v6.12 ships this board USB2-only — exactly the four
EHCI/OHCI nodes + u2phy2/u2phy3; the xHCI/USB3 wiring landed post-v6.12 (~v6.13).

Support facts (v6.12 base dtsi, cross-checked against the vendor control FDT, which
carries all four EHCI/OHCI nodes status="okay" with u-boot,dm-pre-reloc):
- Clocks HCLK_HOST{0,1}, HCLK_HOST_ARB{0,1}, ACLK_USB + u2phy 480 MHz; power domain PD_USB.
- `companion = <&ohci>` on each EHCI node — classic EHCI/OHCI companion split.
- u2phy2/u2phy3 in the USB2PHY GRFs at 0xfd5d8000 / 0xfd5dc000 (rockchip,rk3588-usb2phy).
- VBUS for both USB2-A ports: vcc5v0_usb20, regulator-fixed, GPIO3_B7 active-high
  (the analogue of the NIC rail GPIO3_B4 the PCIe lane drives).
- NO dma-coherent on any USB node — the PCIe coherence discipline applies in full.
- 0xfc800000 falls inside the board profile's existing Device GiB — no MMU change.
- OHCI DMA pointers are 32-bit; kernel RAM window is below 4 GiB — fine.

## 1. Why OHCI-only v1

A stock keyboard/mouse is low-/full-speed. On the USB2-A ports, EHCI's CONFIGFLAG=0
at reset routes every port to the companion OHCI (EHCI 1.0 §4.2) — LS/FS devices land
on OHCI, full stop; an EHCI-only or xHCI-only driver would address zero keyboards on
these ports. The USB3-A ports sit behind the onboard hub on xhci1 — that path needs
DWC3 glue + u2phy1 + a hub-class driver + xHCI (3–4× the surface, largest board-only
share, and no v6.12 reference config exists for this board). U-Boot handoff is
rejected as a runtime dependency (nothing enumerated under the serial-loader flow)
but used as bench recon (usb tree; u2phy GRF md dumps around `usb start`).

v1 = OHCI on the two USB2-A ports: HC reset (preserve HcFmInterval), HCCA, one
control ED pair, one interrupt ED per device, done-queue walk. Defensively clear
both EHCIs' CONFIGFLAG (one register write each — not a driver). EHCI (split
transactions), hub class, IRQs, xHCI/USB3: recorded follow-ups.

## 2. Driver placement (the Eo9 model)

Precedent: plan/09 D46 (eo9-rtl8125 core crate + guest wasm driver over eo9:pci).
SPEC line 574 anticipates platform devices. This lane builds the second hardware root:

- New WIT package `eo9:platform@0.1.0` mirroring pci.wit's shapes: enumerate() →
  list<region-info> (names from the board profile: usb-host0-ohci, …); claim(name)
  exclusive with busy semantics; width-explicit read/write through the syndrome-valid
  mmio accessors; alloc-dma/dma-address/dma-read/dma-write with the SAME coherence
  brackets as the PCI provider (factor DmaBuffer + arch::dma_coherence::sync into a
  shared module — do not copy); interrupt ops present but `unsupported` in v1.
  Worlds: none + deny so the composition algebra and refusal tests work like PCI's.
- Kernel provider src/wasm/platform_provider.rs mirroring pci_provider.rs; new boot
  grant token `platform`, never linked by default (MMIO+DMA without IOMMU is
  full-memory authority — same SPEC posture as pci).
- SoC plumbing stays kernel-side in arch/aarch64/rk3588_usb.rs (cited constants like
  rk3588_pcie.rs): PD_USB check, clock gates, u2phy2/3 host-port init per
  phy-rockchip-inno-usb2.c, VBUS GPIO3_B7. The guest driver gets ONLY the 256 KiB
  controller register block — CRU/GRF are never granted out.
- Guest side: crates/eo9-ohci pure no_std core (ED/TD encode/decode, HCCA, done-queue
  walk, enumeration state machine, descriptor parse, HID boot-protocol decode; host
  unit tests pin every encoding incl. the HcFmInterval-restore gotcha). Two thin
  shells over a RegionIo trait: `usb.ohci` over eo9:platform (board) and a QEMU-only
  shell over eo9:pci driving `-device pci-ohci` — this makes the whole protocol stack
  battery-testable. D46 driver discipline verbatim (take/put slot, bring-up claim,
  bounded polls, typed errors never traps).

## 3. Milestone ladder

- M0 (no bench time): platform WIT + kernel provider + grant token (+ QEMU-profile
  region table, possibly empty; provider mechanics tested via typed-refusal
  integration tests); crates/eo9-ohci host tests; QEMU lane green: usbcheck
  (enumerate→descriptors) and hidcheck (boot reports) against
  `-device pci-ohci -device usb-kbd`. Full battery green, canonical demos unchanged.
- M1 (board): rk3588_usb.rs plumbing, print-before-touch each step (PD_USB → clocks
  → u2phy → VBUS); usbcheck claims both OHCIs, prints HcRevision (expect 0x10),
  HcRhDescriptorA (NDP=1), bounded port-status watch. Acceptance: plug flips CCS/CSC
  on exactly one controller; LSDA records device speed. No HcRevision → power/clock;
  revision OK but CCS never sets → VBUS vs PHY (keyboard backlight = free VBUS LED).
- M2 (board): port reset (≥50 ms), SET_ADDRESS, GET_DESCRIPTOR chain; print VID:PID +
  class/protocol (expect HID 3/1/1 kbd, 3/1/2 mouse) + endpoint bInterval/wMaxPacketSize.
  Byte-identical logic to the QEMU lane — divergence here = DMA coherence or PHY.
- M3 (board): SET_CONFIGURATION, SET_PROTOCOL(boot), SET_IDLE, interrupt-IN ED on the
  periodic list, done-queue poll; print raw + decoded reports, reports/s counter
  (the stranded-runnable backstop is an armed detector for dropped polls).
- M4: keystrokes → eosh stdin. Today's console input: UART RX → RX_RING (the idle
  backstop scavenge is currently the only producer — the GAPS 64-byte bug) → text
  provider read-line. Add kernel uart::inject_input(&[u8]) (second producer, same
  Ctrl-C scan), exposed as a narrow `console-sink` root capability (own grant token,
  never default-linked); the HID driver runs as a detached service decoding reports →
  console-sink. Serial + USB interleave in the one ring; >64-byte commands typed on
  USB survive (the UART FIFO is not in this path). Mechanics + a fake-HID injector
  fully QEMU-testable first.

## 4. QEMU vs board-only

QEMU/host covers: all encodings/parsers (host tests), real OHCI schedule behavior +
enumeration + HID end-to-end (pci-ohci + usb-kbd/usb-mouse), platform provider
semantics (typed refusals), console-sink routing (fake injector). Board-only:
PD/CRU/u2phy/VBUS plumbing, non-coherent DMA on real silicon, low-speed signaling
(QEMU usb-kbd is FS), CONFIGFLAG interaction with vendor U-Boot state.

## 5. Risks → discriminating tests

1. Keyboard on a port OHCI can't see → usb tree recon; M1 plug test. Standing bench
   rule: keyboard/mouse in the USB 2.0 type-A ports only.
2. u2phy init insufficient → HcRevision fine but CCS never sets with VBUS on; bench
   instrument: md-dump u2phy GRFs before/after `usb start`, diff = required init.
3. VBUS rail dead/polarity → keyboard backlight separates rail (3) from PHY (2).
4. PD_USB gated under `go` flow → M1 PD print before first touch; prompt-side md.
5. Non-coherent DMA → WDH never sets / HccaDoneHead stays 0; structurally covered by
   provider sweep brackets; debug knob: sweep whole DMA window per poll.
6. EHCI owns ports (prior `usb start` set CONFIGFLAG) → OHCI CCS=0 with device
   powered; driver always clears both CONFIGFLAGs first; test each milestone with and
   without prior `usb start`.
7. Polled cadence vs ~8–10 ms interrupt period → M3 reports/s under autorepeat
   (steady ~125/s expected; sagging = scheduling, garbled = DMA). SPIs 216/219
   recorded for the IRQ follow-up — surface shape settled, see §6.
8. The 64-byte UART FIFO bug is context, not a dependency: USB input bypasses the
   UART entirely (this lane is the long-term replacement for serial console input);
   bench automation still needs the UART kernel fix separately. Injection path keeps
   drop-with-counter ring-full policy.

## 6. Board interrupt-wait surface (design note — the risk-7 follow-up's shape)

Status 2026-06-09 (area/37, timer-crutch audit A1/A4): the QEMU leg is event-driven —
`usb.ohci-pci` asks `pci::enable-interrupts` for one INTx vector at bring-up; granted,
the shared core unmasks exactly WDH+RHSC (+MIE) via `Ohci::enable_events` and the
steady-state waits park on the interrupt (`usb::read` on WDH, `usb::watch-ports` on
RHSC), each wait bounded by the provider (`INTX_WAIT_BOUND`). Everything the board
leg needs above the kernel is ALREADY IN PLACE and capability-gated, not cfg-gated:

- **Guest side: nothing left to do.** `usb.ohci` calls
  `platform::enable-interrupts(region)` at bring-up today; the v1 kernel root answers
  `unsupported`, so the shell keeps `interrupt: None`, the core never unmasks, and
  reads stay short polls with the consumers pacing (`usb.kbd`/`hidcheck`
  `POLL_PACE_NS` = 2 ms, the connect watches 50/100 ms) — the documented v1 board
  residue. The moment `enable-interrupts` answers with a handle, the same shell
  bytes go event-driven.
- **Kernel side: mirror `pci_provider`'s `IntxWait` in `platform_provider`.** The
  region table gains a per-region GIC SPI (usb-host0-ohci → SPI 216, usb-host1-ohci
  → SPI 219, from the v6.12 dtsi — §0 table); `enable-interrupts(region)` validates
  the region carries a line, registers a per-line delivery counter with the arch GIC
  layer (the `kirq` mask-and-count protocol `pci_intx` already implements), and
  returns the handle; `wait` is the same arm→park→take future with the same
  2 s bound and Drop mask-and-drain discipline. No new WIT — the surface has been in
  `eo9:platform` since M0 ("interrupt ops present but `unsupported` in v1").
- **Acceptance when it lands:** the bring-up line flips from `events: polled` to
  `events: interrupt` on the bench transcript; the M3 autorepeat reports/s holds
  (~125/s — the hardware polls the endpoint at its interval either way; the
  interrupt only replaces the guest-side wake), and the kernel's one-shot
  "delivery served an interrupt wait" line appears. The liveness arm is already
  wired: a report found by the post-timeout drain prints a `liveness: usb.ohci:`
  line (rate-limited), so a half-routed SPI shows up loudly instead of silently
  degrading to the 2 s-paced fallback.
- **Watch out for:** level-vs-edge configuration of the SPIs at the GIC distributor
  (OHCI INTx is level), and the unmask-only-under-a-waiter rule — the kernel masks
  the line at the controller between waits, so an asserted-but-unawaited OHCI cause
  must not storm the IRQ handler.

## Critical files

- kernel/eo9-kernel/src/wasm/pci_provider.rs — provider to mirror; factor out shared DMA.
- kernel/eo9-kernel/src/arch/aarch64/rk3588_pcie.rs — SoC plumbing template for rk3588_usb.rs.
- wit/pci/pci.wit — WIT shapes that wit/platform/platform.wit mirrors.
- kernel/eo9-kernel/src/arch/aarch64/uart.rs + src/wasm/providers.rs — M4's RX ring + read-line.
- .claude/board-bringup/vendor-control-fdt.dtb — live board USB node ground truth.
