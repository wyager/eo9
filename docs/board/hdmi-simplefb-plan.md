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

## The small-image fix: filling the negotiated timing (area/39 study, 2026-06-09)

Bench bug: the HDMI picture is small — the console occupies only the center of the
capture dongle's frame. Study only (no board access this lane); the planner runs the
experiments below.

### Diagnosis from the Round-0 readbacks

The Esmart0 scaler is ALREADY engaged: ACT_INFO 0x01df031f (800×480 source) vs
DSP_INFO 0x034805f0 (1521×841 on screen). So vendor U-Boot scales the logo window up,
but its destination rectangle stops well short of the timing the dongle negotiated
(1521×841 is neither 800×480's aspect nor any standard mode — it is whatever the
vendor logo path computed that boot). The black frame around the console is simply
the unfilled remainder of the video port's active area. Two readbacks pin the truth
on the next bench session (VOP2 base 0xfdd90000, VP0 regs at +0xc00):

- `md.l 0xfdd90c4c 1` / `md.l 0xfdd90c54 1` — VP0 DSP_HACT_ST_END / DSP_VACT_ST_END:
  the negotiated timing's active start/end (end−start = active width/height).
- `md.l 0xfdd91830 4` — Esmart0 REGION0_SCL_CTRL + SCL_FACTOR_YRGB as vendor left
  them (which filter the logo path uses; we mimic).
- `md.l 0xfdd90c34 2` — VP0 POST_DSP_HACT_INFO / POST_DSP_VACT_INFO (the port's
  post-scaler placement), and OVL_PORT_SEL at +0x608 if there is any doubt that
  Esmart0 is on VP0.

### The options, costed

**(a) Make vendor U-Boot scan out a bigger surface (logo.bmp swap).** The vendor DRM
logo path allocates the framebuffer at the BMP's own dimensions (that is where
800×480 comes from — the shipped logo, not a mode choice; the env has no splashimage
or size knobs per Round-0). Replacing logo.bmp on the boot medium with e.g. a
1920×1080 BMP gives a full-size surface with no kernel register writes at all — but
it invalidates every verified constant (base moves, 0x119400 → 0x5ef400 bytes, VIR,
ACT, the locator gates), grows the U-Boot-heap framebuffer to ~6 MB, multiplies every
fbcon repaint/scroll by ~5.4× device-write cost (the scroll repaint is already the
long BUSY window), and re-opens the does-the-base-move question at the new size. A
re-verification bench day plus geometry churn across gfxfb/fbcon. NOT recommended
while (b) exists.

**(b) RECOMMENDED — stretch the Esmart0 window over the full active area.** Keep the
verified 800×480 RGB888 surface and every kernel constant untouched; reprogram only
the window's destination rectangle and scale factors, exactly what the vendor logo
path already does with smaller numbers. The scaler is luma-safe (bilinear on r=g=b
stays r=g=b), Esmart scale-up tops out at 8× (mainline `max_upscale_factor = 8` —
even 3840/800 = 4.8 fits), and the kernel keeps writing 384 KB frames while the
hardware does the filling. Register-level sketch (offsets from mainline
rockchip_vop2_reg.c / rockchip_drm_vop2.h; Esmart0 region0 base 0xfdd91800):

```
W = active width, H = active height        # from the VP0 readbacks above
hfac = ceil((800-1)<<16 / (W-1)) - 1       # vop2_scale_factor(), scale-up shift 16
vfac = ceil((480-1)<<16 / (H-1)) - 1
+0x24 DSP_INFO        = (H-1)<<16 | (W-1)
+0x28 DSP_ST          = 0                   # or center if aspect-preserving (below)
+0x30 SCL_CTRL        = hor_scl_mode[1:0]=1 (up) | hscl_filter[3:2]=1 (bilinear)
                      | ver_scl_mode[5:4]=1 (up) | vscl_filter[7:6]=1 (bilinear)
                        (verify against the vendor readback; mimic its filter)
+0x34 SCL_FACTOR_YRGB = vfac<<16 | hfac
then strobe the shadow-register commit:
0xfdd90000 REG_CFG_DONE = BIT(15) | BIT(vp_id) | BIT(vp_id)<<16
                        # GLB_CFG_DONE_EN + per-VP done; high half is the
                        # write-enable mask convention — confirm by readback
```

Worked example if the timing turns out 1920×1080: DSP_INFO = 0x0437077f, hfac =
0x6a96, vfac = 0x71a5, SCL_FACTOR_YRGB = 0x71a56a96.

Aspect: 800×480 is 5:3; a 16:9 timing stretched full-frame distorts ~11% — for a
text console that is fine and maximizes glyph size (recommended default). The
aspect-preserving alternative is height-fill (dst H×(H·5/3), DSP_ST centering the
pillarbox), one extra line of arithmetic — owner's call on the bench day.

Experiment plan (planner, one session, all from the U-Boot prompt before any kernel
code): read the VP0 timing, compute the four values, `mw.l` them, strobe CFG_DONE,
confirm the logo fills the frame on the capture stick. Risks are all visible-and-
recoverable: a wrong CFG_DONE mask simply never latches (logo stays small), a wrong
factor renders a stretched/garbled logo until the next power cycle. No CRU, no
mode-set, no irreversible state. Once proven, the kernel leg is a small `fbscale`
boot-token step in fbcon/gfx activation (~40 lines: VP readback, factor math, five
MMIO writes, evidence line) — gated like every scanout-touching leg on the locate
checks, and refusing (serial-only, small picture) on any unexpected readback.

**(c) Proper mode-set (own the timing).** Unchanged from the top of this doc: the
dw-hdmi-qp + HDPTX + VOP2 bring-up lane (~1.5-2.5k lines, multiple bench sessions).
Still the only path that controls WHICH timing is negotiated (e.g. forcing 1080p over
the dongle's 4K preference to cut scan bandwidth) and the eventual home of the
colorspace/InfoFrame fix. Stays deferred; (b) does not prejudice it — everything (b)
programs is window-local and gets rewritten by any future mode-set anyway.

### Coordination notes

- area/40 (eosh wrap-aware repaint): fbcon renders the lane's full documented
  emission alphabet (the contract table in guest/eosh/eosh-inc/src/editor.rs module
  docs): `\b \b`, `\r`, `\r\n`, `ESC[K` (EL0), `ESC[A` (CUU — emitted bare; counts
  accepted defensively, clamped at the top row, never a reverse scroll),
  `ESC[<n>G` (CHA — 1-based, clamped to [1, COLS]), and SGR 31/0 (7/27 consumed
  zero-width). The wrap-boundary backspace composite (`CSI A` `CSI <width>G`
  `CSI K`) and the recall replace composite (`\r` `CSI K` + (`CSI A` `CSI K`)×rows +
  re-emit) are pinned by fbcon host tests against the grid. Anything beyond this
  alphabet is consumed-and-ignored: extend `csi_dispatch`
  (kernel/eo9-kernel/src/fbcon.rs) in lockstep with the editor contract rather than
  letting new sequences strip.
- The fbcon scroll repaint is the long render window feeding the new tee-ring
  backlog; (b) does not change its cost (the surface stays 800×480). Option (a)
  would have multiplied it — one more reason (b) wins for the console use case.
