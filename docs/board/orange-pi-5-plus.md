# Orange Pi 5 Plus (RK3588): U-Boot recipe + day-one bring-up

Status: **draft, written before the board arrived.** Everything tagged
**[verify-on-board]** must be confirmed against the running U-Boot / the board's DTB on
day one — the DTB is the authority, this doc is the map. Sources: mainline U-Boot
(`orangepi-5-plus-rk3588_defconfig`, supported since v2024.04), mainline Linux
`rk3588-orangepi-5-plus.dts`, the RK3588 TRM.

## Bring-up

The generalized lessons from this board's bring-up — transport doctrine (the serial
loader stub; vendor `booti` is treated as hostile after a live wedge), the PoC
cache-coherency rules for loader/kernel/FDT handoffs, loop-safety (exit-is-reset,
panic marker, watchdog, boot beacons), bench electrics, and the ordered checklist for
the next board — live in **[bringup-playbook.md](bringup-playbook.md)**. Note the
recipes below predate the board: the live bring-up loaded over serial via
`boards/opi5-serial-loader/` (linked at `0x0020_0000`), not the SD `booti` flow this
draft sketches.

## What the kernel must gain (the known list, in dependency order)

| # | Change | Size | Notes |
|---|---|---|---|
| 1 | **Linux `Image` boot header** — `booti` requires the 64-byte aarch64 Linux header (magic `ARM\x64`, text_offset, image_size) on a *flat binary*; QEMU loads our ELF directly so we never needed it | small | emit the header in `boot.rs` + an `objcopy -O binary` step in xtask (`cargo xtask image aarch64`) |
| 2 | **Load/link address** — the kernel links for QEMU virt's RAM at `0x4000_0000`; RK3588 DRAM starts at `0x0` (with low carve-outs for TF-A) | small-medium | simplest v1: a second linker script / `--defsym` RAM base per board profile; PIE later |
| 3 | **UART driver variant** — debug UART2, `snps,dw-apb-uart` at `0xfeb5_0000` **[verify-on-board]**, clock 24 MHz, **reg-shift=2 / reg-io-width=4** (32-bit registers at stride 4) vs our byte-stride 16550 | small | parameterize stride+width in the existing 16550 driver; **1500000 baud** is the Rockchip convention — and *do not reprogram the divisor on day one*: U-Boot leaves the line configured, just use it (our riscv64 driver already follows the don't-touch-the-divisor pattern) |
| 4 | **MMU device window** — QEMU virt's devices live in the low gigabyte; RK3588 peripherals sit at `0xfd00_0000`–`0xfe9f_ffff` | small | extend the identity device map with a per-board range table |
| 5 | **GIC bases from board profile** — GIC-600 (GICv3): GICD `0xfe60_0000`, GICR frames `0xfe68_0000` (8 PEs × `0x20000`) **[verify-on-board]** | small | the GICv3 driver itself already exists (this branch — verified under `qemu … gicv3`); only the constants move to the board profile; boot on **CPU 0 = a Cortex-A55** (the boot core), redistributor frame 0 |
| 6 | **PSCI conduit** — QEMU virt uses HVC; on the board TF-A serves PSCI via **SMC** | tiny | conduit selection in the board profile (DTB `psci.method` says `smc`) |
| 7 | **Timer frequency** — read `CNTFRQ_EL0` as today (24 MHz on RK3588); no change expected | none | listed for completeness |
| 8 | **DTB intake** — `x0` carries the DTB pointer per the Linux boot protocol; v1 boards profile = compiled-in constants, DTB parsing deferred (see gfx-simplefb.md for the minimal parser that changes this) | — | print `x0` on boot day one, keep the pointer for later |

Items 1–2 are **preparable now** (QEMU can boot the flat Image via `-kernel` too, so the
header + objcopy path is verifiable before arrival). 3–6 are small and board-blocked.

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

Vendor (Orange Pi) U-Boot images use the same offsets. If the shipped SD/eMMC already
boots to a U-Boot prompt, **skip all of this on day one** and use the existing loader —
the recipe matters only for a from-scratch card.

## Boot flow (day one: load over serial-interactive U-Boot from SD)

```
# at the U-Boot prompt (1500000 8N1 on the 3-pin debug header):
bdinfo                                   # confirm DRAM base/size        [verify-on-board]
fdt addr ${fdtcontroladdr}; fdt print /chosen
load mmc 1:1 0x00480000 eo9-kernel.img   # flat Image with Linux header (item 1)
load mmc 1:1 0x0a000000 rk3588-orangepi-5-plus.dtb
booti 0x00480000 - 0x0a000000
```

`text_offset` in the Image header must match the load offset convention (`0x0` with
`booti` placing the kernel at a 2 MiB-aligned address it likes; print-and-verify day one).

## Day-one smoke plan (in order, each step has a visible success)

1. **Serial sanity**: U-Boot banner at 1500000 baud (adapter must genuinely do 1.5 Mbaud —
   FT232/CP2102-class; cheap 115200-only adapters garble).
2. **`bdinfo` + `fdt print`**: record DRAM base/size, UART base, GIC bases, the five
   `pcie@…` nodes → fill every [verify-on-board] in these docs.
3. **Boot the header-only kernel** (items 1–3 done): success = the Eo9 banner + timer
   frequency line over UART2. No GIC, no wfi — `kprintln` only, park.
4. **+GIC (item 5)**: success = the `gic: v3` line + a timer-interrupt-driven sleep
   (the `demo` path's sleepy canary, no store needed).
5. **+wfi idle + UART RX**: success = the eosh prompt accepting keystrokes (the full
   interactive shell, store baked as on QEMU).
6. **PCIe enumeration** (the rk3588-pcie.md shim): success = `lspci` listing the NVMe
   stick / RTL8125s through the DW shim.
7. Stretch: `gfx.simplefb` against U-Boot's framebuffer (see gfx-simplefb.md).

Steps 3–5 are one afternoon if the doc's addresses survive contact with the DTB; PCIe is
its own session.

## Known unknowns (decide on the board, don't pre-engineer)

* **Which core boots** (TF-A usually parks secondaries; we want core 0 = A55) — affects
  nothing yet (single-core kernel) but record the MPIDR day one.
* **Vendor vs mainline U-Boot** on the shipped media — whichever shows a prompt wins;
  only the recipe section cares.
* **eMMC vs SD priority** — BootROM order is SPI-NOR → eMMC → SD; if the board ships with
  eMMC firmware, the SD card may need the BootROM pin shorted or the eMMC image replaced.
  Day-one fallback: vendor image on eMMC boots → use its U-Boot to load our kernel from SD
  partition 1.
* **The 1.5 Mbaud adapter** — have a known-good one before the board arrives.

## Hardware goals (owner, 2026-06-04)

Real-hardware "done" = HDMI + USB keyboard + ethernet working, in planned order:

1. **First light** (in progress): boot to eosh over UART2.
2. **Ethernet** — the 2× 2.5GbE NICs are RTL8125 **on PCIe**, so this is our existing driver
   model end-to-end: the DW-PCIe config shim (rk3588-pcie.md) → enumerate → an rtl8125 wasm
   driver exporting eo9:net/l2 → the entire existing l4/switch/bridge stack unchanged on top.
   Highest value, least new machinery.
3. **HDMI** — via gfx.simplefb (gfx-simplefb.md): the vendor U-Boot already brings up
   VOP2/HDMI (DRM v1.0.1 + plane configs in the boot log); we read its framebuffer rather
   than writing a display driver. Console-on-HDMI additionally needs a text renderer over
   eo9:gfx (font blitter — new, small).
4. **USB keyboard** — the biggest new machinery: EHCI on RK3588 is a *platform* MMIO device,
   so this motivates extending the wasm-driver model beyond eo9:pci to platform MMIO+IRQ
   grants (SPEC already anticipates "MMIO regions, interrupt lines" as hardware roots), then
   EHCI + hub + HID boot-protocol keyboard.
