# HDMI on the Orange Pi 5 Plus — simplefb-handoff plan + Round-0 results

Plan produced 2026-06-08 (planning agent); Round-0 bench recon executed the same day.
This document records the decision and the verified constants. Companion design note:
docs/board/gfx-simplefb.md.

## The decision: ride vendor U-Boot's scanout (Path A) — CONFIRMED ON THE BENCH

Native VOP2+HDMI bring-up was costed honestly and rejected for now: mainline Linux
v6.12 cannot light RK3588 HDMI at all (dw-hdmi-qp landed in v6.13; ~9-10k lines of
reference across VOP2 / HDMI-QP TX / Samsung HDPTX PHY → realistically 1.5-2.5k kernel
lines, multiple bench sessions, silent-black-screen failure modes). Recorded as the
deferred follow-up lane.

## Round-0 recon results (2026-06-08, vendor U-Boot 2017.09-orangepi Aug 30 2024)

- `stdout=stderr=serial,vidconsole`; no `splashimage` var.
- /chosen has NO framebuffer node (as predicted: vendor fixups only run in bootm/booti,
  which we never use). /reserved-memory has `drm-logo@0` with ALL-ZERO reg.
- The display IS live: vendor logo on green background (verified via an HDMI capture
  stick on the bench Mac — `imagesnap -d "Cam Link 4K"` closes the visual loop
  autonomously; captures in .claude/board-bringup/captures/).
- VOP2 sniff at the prompt (cluster windows at 0xfdd91000 all zero; Esmart0 at
  0xfdd91800 live):
  - Esmart0 MST = **0xee01a000** (the scanout buffer, in U-Boot heap territory)
  - ACT_INFO 0x01df031f = **800×480** logo window; VIR = 0x258
  - DSP_INFO 0x034805f0 (window scaled up on the display mode)
- **Paint probes VERIFIED the buffer live** (three rounds, each captured):
  (1) bulk 0xff fill → window magenta; (2) banded fills → bands at computed offsets;
  (3) the stride discriminator — 24000 white bytes on black → a ~10-row line, pinning
  **stride 2400 bytes = 800 px × 3 = RGB888 packed** (VOP2 Esmart format 6, VIR 0x258
  = 600 words ✓). Surface = base 0xee01a000, 800×480, RGB888, 0x119400 bytes.
- **The "wrong colors" are a LINK colorspace mismatch, not a buffer property**:
  U-Boot scans out RGB while the HDMI signaling/capture path interprets YCbCr 4:4:4.
  Proof by fixed points: 0xff fill → magenta (Y/Cb/Cr all 255), 0x00 → green
  (Y/Cb/Cr 0), 0x80 → CORRECT gray (gray is the invariant of the confusion).
  Consequence: **grayscale (r=g=b) renders correctly through the mismatch** — the
  luma-first v1 needs no pipeline fix at all. True color later = AVI InfoFrame /
  colorspace fix at the HDMI TX, recorded as the deferred color follow-up.
  - **Observed variance (M1+M2 acceptance boot, 2026-06-08): the mangling did NOT
    recur** — `draw`'s pattern rendered with correct colors after that boot's U-Boot
    HDMI re-init, so the mismatch is boot-state-dependent (whatever AVI/colorspace
    state the TX negotiates that cycle), not a fixed property of the link. The
    provider semantics are unaffected either way (pixel values are stored and read
    back faithfully; the identity checksum passed on the same boot); the deferred
    color follow-up becomes "make the colorspace deterministic", not "fix it".

## Consequences for the lane (luma-first, per owner direction)

The surface is RGB888 already; only the LINK mangles chroma. So:
- v1 (`gfx.simplefb`) writes RGB888 pixels (3 bytes/px, stride 2400) and the provider
  converts the WIT's xrgb8888 → packed RGB888 at the boundary. NO VOP2 register writes
  (we adopt the running scanout untouched). Grayscale content displays faithfully;
  colored content displays consistently-wrong until the link colorspace fix — fine
  for console/text v1 (owner's luma-first call).
- mode() reports 800×480 xrgb8888 honestly; the colorspace caveat lives in the docs,
  not the contract (pixel VALUES are stored faithfully; presentation chroma is the
  link's problem).
- Deferred color follow-up: AVI InfoFrame/colorspace at the HDMI TX (or reprogram the
  window + CSC) — a bounded register fix against a running pipeline, not a bring-up.

## Milestone ladder (revised by Round-0)

- M1: kernel paints grayscale bands (RGB888 r=g=b) at boot (gfxprobe bootargs token),
  prints `fb: profile base=0xee01a000 800x480 rgb888 stride=2400` + painted crc;
  verified by capture stick. Risk burned down: does scanout survive the `go` handoff + our MMU/EL2 path
  (nothing in the kernel touches CRU/VOP — expected yes; this round proves it).
- M2: `gfx.simplefb` kernel root provider (gfx.mem semantics over the Y plane with
  RGB→luma at the boundary; PXN+UXN mapping; the gfx grant token like pci).
  Normal-NC MAIR attribute optional here — Device mapping is fine for 384 KB frames.
- M3: cross-backend checksum identity (gfx.mem at 800×480 vs gfx.simplefb) + the
  draw demo on the monitor; perf datum for the mapping choice.
- M4 (stretch): eosh on HDMI via a font-blitting text component over eo9:gfx.
- Open question for M1: does the fb base move across power cycles (U-Boot heap alloc)?
  Measure across 3 cycles; if unstable, the kernel-side Esmart0 MST read at boot IS
  the locator (~20 lines, read-only).

## Bench instruments

- Frame capture: `imagesnap -d "Cam Link 4K" -w 2 <out>.png` on the bench Mac.
- Paint probe at the U-Boot prompt: `mw.l 0xee01a000 <word> <count>` then
  `crc32 0x10000000 0x1000000` (cache-pressure eviction — U-Boot writes cacheable,
  VOP2 scans DRAM; the playbook §3 lesson applies to paints too).
- VOP2 window dump: `md.l 0xfdd91800 0x40` (Esmart0; MST at +0x14).
