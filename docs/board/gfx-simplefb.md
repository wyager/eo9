# gfx.simplefb: the dumb-framebuffer provider for real boards (design note)

## As built (M1+M2, 2026-06-08) — the Orange Pi 5 Plus variant

The Round-0 bench recon (docs/board/hdmi-simplefb-plan.md) changed the locator: vendor
U-Boot publishes **no** simple-framebuffer node on this board (its fixups only run in
bootm/booti, which we never use), but it leaves VOP2's Esmart0 window live across the
`go`/serial-loader handoff. So the FDT-reader path below is deferred, and the shipped
locator is:

* **Primary**: board-profile constants — base `0xee01a000`, 800×480, packed RGB888
  (3 bytes/px), stride 2400 (`kernel/eo9-kernel/src/gfxfb.rs`).
* **Cross-check**: the live Esmart0 window registers at `0xfdd91800` (MST/VIR/ACT),
  read through the syndrome-valid mmio accessors. Both are printed (`fb: profile …` /
  `fb: vop2 …`), a disagreement warns `MISMATCH`, and a DRAM-plausible MST wins —
  U-Boot heap-allocates the buffer, so the base may move across power cycles.
* **Map check before first touch**: the surface must sit in the identity map's
  fourth-GiB Device-nGnRnE block (RW+PXN+UXN — W^X holds without new tables); anything
  else prints `fb: unmapped 0x…` and stops cleanly. Device mapping is the v1 choice
  (384 KiB frames; a Normal-NC MAIR attribute is the recorded perf follow-up), and the
  first present prints `gfx: first present (mapping=device)` as the A/B datum.

The provider (`kernel/eo9-kernel/src/wasm/gfx_provider.rs`) is gfx.mem semantics over
`base + y*2400 + x*3`, converting the WIT's xrgb8888 ↔ packed RGB888 at the boundary
(present drops the X byte; read synthesizes X = 0; framebuffer byte order pinned to the
little-endian DRM RGB888 convention B,G,R — grayscale-invariant, see below). `mode()`
reports 800×480 xrgb8888 with stride = width×4 (the API's pixel model, honoring the
WIT's `stride >= width * 4`; the RGB888 backing stride is a provider internal) and
refuses any other live scanout geometry with the typed `io` error. The grant is the
bare `gfx` boot token (the `pci` grammar, never linked by default); the `gfxprobe`
token runs the M1 first-light band paint. The rect/stride/conversion math is the pure
host-tested `gfxfb` module: the cross-backend checksum identity with gfx.mem at
800×480 is pinned there and in tests/eo9-integration/tests/gfx.rs
(`0xd66b49ee575ff0d9` for draw's frame 1).

**Colorspace caveat (docs, not contract):** the board's HDMI link can mangle chroma —
the buffer is scanned as RGB while the link/downstream may interpret YCbCr, in which
case saturated colors render consistently wrong while grayscale (r=g=b) always renders
faithfully. The M1+M2 acceptance boot (2026-06-08) showed the mangling is
**boot-state-dependent**: colors rendered correctly that cycle after U-Boot's HDMI
re-init (see the plan doc's observed variance). Pixel values are stored and read back
faithfully regardless; the deferred follow-up is making the link colorspace
deterministic at the HDMI TX (AVI InfoFrame), not a provider change.

The sections below are the original design note (the simple-framebuffer FDT path stays
the plan for boards whose firmware publishes the node).

---

The `eo9:gfx` API was designed framebuffer-first precisely so that a firmware-configured
scanout can implement it (plan/02 D28: mode + present/read with damage rects + clear,
xrgb8888, provider owns the stride). On the Orange Pi 5 Plus the cheapest path to pixels
is **not** a VOP2/HDMI driver (a giant rabbit hole) but U-Boot's already-configured
framebuffer, advertised to the OS as a `simple-framebuffer` device-tree node:

```dts
chosen {
    framebuffer@f8000000 {            // address/size: whatever U-Boot allocated
        compatible = "simple-framebuffer";
        reg = <0x0 0xf8000000 0x0 0x7e9000>;
        width = <1920>; height = <1080>; stride = <7680>;
        format = "x8r8g8b8";          // = eo9:gfx xrgb8888 when stride/format match
        status = "okay";
    };
};
```

## The provider

`gfx.simplefb` is a **kernel-side root provider** (like the QEMU gfx paths), not a wasm
stub: the framebuffer is raw physical memory handed over by firmware, exactly the kind of
hardware root the OS core owns.

* `mode()` → width/height/stride/format from the node (reject any format other than
  `x8r8g8b8`/`a8r8g8b8` with the API's typed unsupported error — the WIT already allows a
  fallible mode for exactly this).
* `present(buffer, rect)` → bounds-check against mode (the existing gfx.mem logic), copy
  tightly-packed rows into `base + y*stride + x*4`, return the buffer. The mapping must be
  **write-combining/normal-non-cacheable** (a per-board MMU attribute for the framebuffer
  range — plain Device memory makes full-screen presents painfully slow; normal-NC is the
  standard choice).
* `read(buffer, rect)` → copy back out (the API's self-verification property — works on
  scanout memory too).
* `clear(rect)` → present of zeroes, as in gfx.mem.

No interrupts, no vsync (deliberately absent from gfx v1), no DMA — it is gfx.mem with a
firmware-provided base pointer. Estimated size: ~150 lines + the MMU attribute plumbing.

## What is implementable blind (before the board)

1. **The minimal FDT reader.** The kernel has no DTB parser; this is the first consumer.
   Scope it to exactly what's needed: walk the flattened tree for a node whose
   `compatible` is `simple-framebuffer`, extract `reg`/`width`/`height`/`stride`/`format`.
   A few hundred lines of no_std code over the FDT spec (magic `0xd00dfeed`, structure
   block tokens BEGIN_NODE/PROP/END), **unit-testable host-side against a hand-built blob**
   (`dtc` can compile the snippet above in CI-free fashion: check in the compiled `.dtb`
   bytes as a test fixture). This parser is also the down payment on board profiles
   eventually coming from the DTB instead of constants (orange-pi-5-plus.md item 8).
2. **The provider skeleton** against a fake node: parse fixture → construct the provider
   over a RAM-backed "framebuffer" → run the existing gfx integration checksum suite
   against it (it is gfx.mem semantics, so the cross-backend checksum identity must hold).
3. **Not** implementable blind: the MMU attribute for the real range, whether U-Boot on
   this board actually publishes the node (mainline U-Boot needs `CONFIG_VIDEO` +
   simplefb fixup enabled for RK3588 HDMI — support is recent and may require the vendor
   U-Boot instead **[verify-on-board]**), and the real-monitor pixel check.

## Arrival-day verification plan

1. At the U-Boot prompt: does video init print? `fdt print /chosen` — is there a
   `framebuffer@…` node? If not: try `setenv stdout serial,vidconsole` + reinit, or fall
   back to the vendor U-Boot, or defer (the board is fully usable headless; simplefb is
   the demo payoff, not the bring-up blocker).
2. If the node exists: boot Eo9 with the parser pointed at the DTB (`x0`), map the range,
   run `gfx.simplefb $ draw` — success = the test pattern on the monitor and
   `presented(<the canonical checksum>)` over serial, the same dual-verification the QEMU
   gpu path established.
