# Orange Pi 5 Plus (RK3588): board profile + bring-up status

Status: **alive and on the network.** This began as the pre-arrival draft; it now tracks
reality. First light 2026-06-07 — the Eo9 banner over UART2, a program run, and a clean
SYSTEM_RESET back to U-Boot. Within a day the board gained PCIe, real ethernet, a
network shell with DHCP, internet DNS, and pixels on HDMI. The DTB remains the authority
for constants; everything below is board-verified unless marked otherwise. The per-lane
plan docs carry the detail:

* [bringup-playbook.md](bringup-playbook.md) — the generalized doctrine, the verified
  constants table (§1.2), and the running-system incident classes (§7).
* [rk3588-pcie.md](rk3588-pcie.md) — the DW-PCIe config shim (board-validated).
* [hdmi-simplefb-plan.md](hdmi-simplefb-plan.md) — HDMI via the vendor scanout; the
  verified surface constants.
* [usb-ohci-plan.md](usb-ohci-plan.md) — USB HID keyboard/mouse (M0 done on QEMU;
  silicon milestones queued).
* [usb-boot-demo-plan.md](usb-boot-demo-plan.md) — zero-touch USB-stick boot + the
  standalone demo.

## Bring-up

The generalized lessons from this board's bring-up — transport doctrine (the serial
loader stub; vendor `booti` is treated as hostile after a live wedge), the PoC
cache-coherency rules for loader/kernel/FDT handoffs, loop-safety (exit-is-reset,
panic marker, watchdog, boot beacons), bench electrics, and the ordered checklist for
the next board — live in **[bringup-playbook.md](bringup-playbook.md)**. Note the
recipes below predate the board: the live bring-up loaded over serial via
`boards/opi5-serial-loader/` (linked at `0x0020_0000`), not the SD `booti` flow this
draft sketched.

## Goals achieved (owner's hardware goals, 2026-06-04: HDMI + USB keyboard + ethernet)

1. **First light** (2026-06-07): boot to the banner and an interactive eosh over UART2;
   exit-is-reset, panic marker, the DW-WDT armed, boot beacons — the full loop-safety
   set proven on silicon (playbook §4). The dev loop is the serial-loader stub + `go`
   (~155 s for the 22 MiB acceptance image at 1.5 Mbaud).
2. **Ethernet** (2026-06-08): `net.rtl8125`, a wasm driver over `eo9:pci` exporting
   `eo9:net/l2`, drives the onboard RTL8125s through the trained PCIe links at
   **1000 Mb/s**. 2.5G RX is a known-dead deferred item — every board composition pins
   `--advertise-max 1000`. The saga's load-bearing fixes, each its own lesson: the
   OOB/management-MCU ownership takeover, the rge(4) bring-up tables (MAC MCU, EPHY,
   GPHY MCU patch, EEE off), DMA cache maintenance in the pci provider (RK3588 PCIe
   masters are **not dma-coherent**), MAC/PHY loopback self-tests at every bring-up,
   and line-granularity RX re-arm (4 descriptors share a 64-byte maintenance line;
   per-slot re-arm raced the NIC — found in merge review). Full history: plan/09
   entry 46.
3. **Network shell + DHCP** (2026-06-08): `telnetd --nic net.rtl8125 --advertise-max
   1000 --address dhcp` serves eosh sessions over the office LAN (static `--address`
   also exists but see the network policy facts below). `--address dhcp` (smoltcp's
   DHCPv4 client in the l4 middleware) acquires a real lease; the printed
   `dhcp acquired <addr>/<prefix> gw … dns … lease …` line is how the operator learns
   where to telnet. Internet DNS works (example.com resolved through a lease
   resolver). Cleartext, unauthenticated — trusted-LAN only (GAPS: the SSH decision).
4. **On-target codegen is fiber-sliced** (2026-06-08): compiles yield every ~5 ms, the
   watchdog is patted between slices (only after real progress — the pat stays
   honest), and a throttled `codegen: still compiling` line shows liveness.
   Compositions whose compiles exceed the 22.4 s watchdog period no longer
   hardware-reset the board (plan/12, the sliced-codegen entry; playbook §7.2).
5. **HDMI first light + `gfx.simplefb`** (2026-06-08; area/15, merging): the kernel
   adopts vendor U-Boot's live VOP2 scanout — base `0xee01a000`, 800×480 RGB888
   packed, stride 2400, located by reading the Esmart0 window registers (the vendor
   U-Boot publishes **no** /chosen simplefb node; see hdmi-simplefb-plan.md) — and
   `draw` through `gfx.simplefb` reproduces the QEMU `gfx.mem` checksum pin on the
   monitor: cross-backend identity on silicon. The chroma mangling observed in early
   captures is boot-state-dependent (U-Boot HDMI re-init variance), not a pipeline
   property.
6. **USB M0 on QEMU** (2026-06-08; area/14, merging): the `eo9:platform` root
   (platform MMIO+DMA grants — pci's sibling for non-PCI devices), the `eo9-ohci`
   no_std core, and HID boot-protocol decode, validated against QEMU's pci-ohci +
   usb-kbd; `usb tree` recon confirmed the board routing (keyboard/mouse on the
   USB 2.0 type-A ports land on OHCI; the USB 3.0-A ports sit behind a hub on xHCI,
   out of v1 scope).

## Goals in flight

* **USB keyboard on silicon** — usb-ohci-plan.md M1–M4 (PD/clock/PHY/VBUS plumbing →
  enumeration → HID reports → keystrokes into eosh via the console-sink). The demo's
  critical path.
* **Zero-touch USB boot** — the vendor distro-boot chain reaches usb0 with a stock
  environment (boot.scr + EO9.IMG + BOOTARGS.TXT on a FAT32 stick; no setenv, no
  firmware changes); A0 recon done (no dcache/icache commands; the CRC gate is
  crc32-save + cmp.l). usb-boot-demo-plan.md Part A.
* **The standalone demo** — power on → boot from the stick → eosh on HDMI (the fbcon
  kernel console tee, area/17) → USB keyboard → `curl http://…` (area/16) prints a
  real website. usb-boot-demo-plan.md Part B.
* **Deferred, recorded**: 2.5GBASE-T RX (everything pins 1000), the kernel INTx demux
  for the DW controllers (drivers are polled v1), true-color HDMI (the link
  colorspace fix), and the UART RX FIFO kernel fix (next section).

## Console reality (bench-critical)

Board console input truncates at exactly **64 bytes** per line: the DW-APB UART RX
FIFO is 64 bytes deep and the board profile's RX interrupt path never drains it —
input reaches eosh only when the kernel's idle backstop scavenges the FIFO; between
scavenges it overflows silently. GAPS'd for a kernel lane; until then the bench types
in chunks (eosh_cmd.py: 40-byte chunks, scavenge pauses, a redundant trailing
newline). Details and the doctrine in playbook §7.1.

## The network: policy facts the bench learned the hard way

* **The office LAN validates source IPs against its DHCP-snooping bindings** — traffic
  from an address the switch has no lease binding for is silently filtered: no ICMP,
  no log, dead air indistinguishable from a driver bug. This policy was entangled in
  the original "TX blocked" mystery (together with the hardcoded-source confound —
  playbook §7.4/§7.5). **DHCP addressing is canonical on this network**; static
  `--address` invocations are a lab convenience this network does not reliably honor.
* Leases are short (~10 min) and the address is arbitrary — the `dhcp acquired` line
  on the serial console is the operator's source of truth for the board's address.

## Verified constants

The board-verified constants table (DRAM, UART, GIC, entry EL, PSCI, control FDT,
watchdog) lives in **playbook §1.2** — one authority, not duplicated here. Per-lane
constants live with their lanes: PCIe bases/rails in rk3588-pcie.md, the scanout
surface in hdmi-simplefb-plan.md, USB controller bases/PHYs/VBUS in usb-ohci-plan.md,
the boot-environment facts in usb-boot-demo-plan.md. Headlines for orientation: UART2
DW-APB `0xfeb5_0000` @ 1.5 Mbaud, stride 4 (never reprogram the divisor); GICv3 GICD
`0xfe60_0000` / GICR `0xfe68_0000`; image linked run-in-place at `0x0020_0000`; TF-A
hands us EL2 (the image carries the EL2→EL1 trampoline); PSCI via SMC; control FDT at
`bdinfo`'s `fdt_blob` (`0xeb9f6c38`; the `fdtcontroladdr` env var is unset); DW-WDT
`0xfeaf_0000`, 22.4 s; NIC MAC `c0:74:2b:f8:22:33` (XID 0x641, 8125B+).

## Bench inventory (current physical layout)

* **Serial**: the 1.5 Mbaud FTDI-class adapter on the debug UART — the planner owns
  the port; it is both the dev transport (serial-loader stub + `go`) and the capture
  instrument (always line-buffered and tee'd).
* **HDMI**: the board's HDMI feeds a **Cam Link 4K** capture stick on the bench Mac —
  `imagesnap -d "Cam Link 4K" -w 2 <out>.png` closes the visual loop autonomously.
* **USB**: keyboard + mouse go in the **USB 2.0 type-A ports only** (those route to
  OHCI); the boot stick goes in a **USB 3.0 port** (behind the onboard hub — U-Boot
  owns it only until `go`). The USB-C OTG port stays unplugged (it kills the console —
  playbook §5).
* **Power**: the labeled known-good 5 V/4 A supply. When the board looks dead, check
  supply and LEDs before suspecting anything you did (a swapped charger cost a bench
  day).
* Vendor `booti` is banned (data-abort wedge, playbook §2.3); every exit, panic, and
  hang returns to the U-Boot prompt by itself (exit-is-reset, panic marker, the
  22.4 s watchdog).

## The board profile (the pre-arrival list, all landed)

The draft-era "what the kernel must gain" list — the arm64 `Image` header, the
`0x0020_0000` link address, the DW-APB UART variant, the device MMIO window, the GICv3
constants, the SMC PSCI conduit, the 24 MHz timer, DTB intake — landed in full
(plan/12 entries 79/80 are the record; the constants live in playbook §1.2). The one
item that outgrew its sketch: DTB intake is no longer "print `x0` and defer" — the
kernel PoC-sweeps and parses the control FDT for bootargs (playbook §3, incident b),
and the USB-boot lane adds a staged-bootargs fallback for `go`-without-`x0`
(usb-boot-demo-plan.md).

## SD card layout (Rockchip standard)

```
sector 64      (32 KiB):   idbloader.img   (TPL: DRAM init + SPL)
sector 16384   (8 MiB):    u-boot.itb      (U-Boot proper + TF-A BL31)
sector 32768+  (16 MiB+):  partition table / boot partition (kernel Image + eo9.dtb)
```

```sh
# from the mainline U-Boot build (orangepi-5-plus-rk3588_defconfig + rkbin TPL/BL31):
dd if=idbloader.img of=/dev/sdX seek=64 conv=notrunc
dd if=u-boot.itb    of=/dev/sdX seek=16384 conv=notrunc
```

Vendor (Orange Pi) U-Boot images use the same offsets. The shipped eMMC boots to a
vendor U-Boot prompt, so this recipe matters only for a from-scratch card; the
zero-touch boot path being built is a USB stick under the vendor distro-boot chain
(usb-boot-demo-plan.md), not a rewritten SD card.

## Boot flow (as actually flown: the serial loader, not SD/`booti`)

The live flow is the serial loader stub: bootstrap it once over the prompt
(prompt-paced `mm.l` + a `crc32` cross-check), launch with `go`, then
`boards/opi5-serial-loader/tools/send_image.py` streams the flat image to
`0x0020_0000` and jumps with the control FDT address in `x0` — the full recipe and
doctrine live in [bringup-playbook.md](bringup-playbook.md) §2.

The SD/`booti` recipe this draft originally sketched here is retired as a launch
path. Historical incident (2026-06-04): the vendor U-Boot's `booti` data-aborted
inside its own image heuristics on our minimal image (ESR `0x96000010`) and its
recovery reset then failed, wedging the board until a physical power cycle
(playbook §2.3) — vendor boot commands are treated as hostile. The image keeps its
64-byte arm64 `Image` header anyway, for mainline U-Boot or any future sane
bootloader. Two prompt-side cautions that survive from the draft: take the control
FDT address from `bdinfo`'s `fdt_blob` (the `fdtcontroladdr` environment variable was
unset on the vendor U-Boot), and print-and-verify any load address before jumping.
