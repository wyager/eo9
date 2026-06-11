# microSD on the Orange Pi 5 Plus — implementation plan

Plan 2026-06-09. Two lanes over one card: **(A)** boot Eo9 from SD under the vendor distro-boot chain (riding usb-boot-demo-plan.md Part A verbatim), and **(B)** the SD card as a block device — `disk.sdmmc` over `eo9:platform` feeding `fs.eofs` and the `storedisk` compile cache. No code in this document; sources cited inline.

**Sources read:** docs/board/usb-boot-demo-plan.md, usb-ohci-plan.md §0–§5, orange-pi-5-plus.md, bringup-playbook references, wit/disk/disk.wit, guest/stubs/fs-eofs/src/lib.rs, crates/eo9-eofs/src/device.rs, kernel/eo9-kernel/src/virtio_blk.rs, src/wasm/{platform_provider,diskcache}.rs, src/arch/aarch64/{mod,rk3588_usb}.rs, docs/study/disk-iops-audit.md, GAPS.md. **Web (fetched this session):** Linux v6.12 `rk3588-base.dtsi` lines 1831–1880, `rk3588-orangepi-5-plus.dts` lines 19–22/450–462, `drivers/mmc/host/dw_mmc*.c` (line counts below).

## 0. Hardware facts (verified from Linux v6.12 sources this session)

| fact | value | source |
|---|---|---|
| microSD controller | Synopsys DW MSHC (`rockchip,rk3588-dw-mshc`) at **0xfe2c0000**, reg size 0x4000 | rk3588-base.dtsi:1831–1833 |
| IRQ | GIC SPI **203** (record only; v1 is polled) | dtsi:1834 |
| FIFO | depth 0x100 (256 entries) | dtsi:1838 |
| Clocks | **biu/ciu are SCMI-owned** (`SCMI_HCLK_SD`, `SCMI_CCLK_SD` — TF-A BL31 owns them, not the CRU); only drv/sample phase clocks are CRU-side | dtsi:1835–1837 — *load-bearing, see risk R2* |
| Power domain | `RK3588_PD_SDMMC` (same PMU hiword discipline as PD_USB in rk3588_usb.rs) | dtsi:1842 |
| Board slot config | bus-width 4, `cap-sd-highspeed`, `sd-uhs-sdr104`, cd-gpios **GPIO0_A4 active-low**, vmmc `vcc_3v3_s3`, vqmmc `vccio_sd_s0` (PMIC LDO — 1.8 V switchable) | rk3588-orangepi-5-plus.dts:450–462 |
| eMMC | a **separate controller** — `rockchip,rk3588-dwcmshc` (SDHCI-class) at 0xfe2e0000, populated on this board (the shipped eMMC carries the vendor U-Boot, orange-pi-5-plus.md) | dtsi:1861, dts:439 |
| mmc aliases (mainline) | **mmc0 = sdhci (eMMC), mmc1 = sdmmc (SD)** | dts:19–22 |
| Address window | 0xfe2c0000 falls in the board profile's existing Device GiB (same GiB 3 as the UART at 0xfeb50000 and WDT at 0xfeaf0000) — no MMU change | mmu.rs precedent, usb-ohci-plan §0 |
| Reference driver sizes | `dw_mmc.c` **3,629** lines, `dw_mmc.h` **592**, `dw_mmc-rockchip.c` **594** (v6.12, counted from fetched files) | honest sizing input, §B1 |

Mainline aliases are not authority for the vendor U-Boot 2017.09's `mmc` indices — A0 verifies live (`mmc list`). Either way `boot_targets=mmc0 mmc1 nvme … usb0 …` scans **both** mmc devices before usb0, and `boot.scr` uses `${devtype} ${devnum}:${distro_bootpart}`, so the design does not depend on the index.

---

# PART A — SD boot

## A.1 The boot path rides the USB plan verbatim

Everything in usb-boot-demo-plan.md Part A transfers unchanged, because distro-boot is medium-agnostic:

- **Same artifacts**: FAT32 partition 1, `BOOT.SCR` (mkimage -T script), `EO9.IMG` (flat image, fatload to 0x00200000, `go` — ≤62 MiB structural cap; bigger images use the BOOTSTRAP-image + network-kexec path from net-kexec.md), `BOOTARGS.TXT` fatloaded to the staged-bootargs page 0x00100000. The staged-bootargs kernel fallback is **MERGED and working** (orange-pi-5-plus.md: the 0x00100000 page is reserved in mmu.rs, `fdt::bootargs` reads it last, x0-valid always wins) — zero new kernel work for Part A.
- **Same boot.scr**, byte-identical: `${devtype}/${devnum}` resolve to `mmc 1` instead of `usb 0`. The CRC gate construction is the A0-proven one (no `crc32 -v`/`itest`/`setexpr`: `crc32 addr ${filesize} 0x00180000` + `mw.l` expected + `if cmp.l … then go`). xtask's stick-builder becomes a **medium-agnostic image-builder** — only the partition layout differs (SD adds partition 2, §B3).
- **Same extlinux landmine, identically**: the FAT partition must carry **no `/extlinux/`, no `/boot/` directory, no `boot.scr.uimg`** — `scan_dev_for_boot` checks extlinux first and funnels into the `bootm/booti` family that data-aborted and wedged the board (playbook §2.3; booti is banned).
- **Same recovery doctrine**: bootdelay=0 is non-interruptible; bad-card recovery = pull the card, the chain falls through to `opi#` exactly as captured. CRC-fail path stays fail-safe (scan chain continues benignly to the prompt). Post-`go` panics watchdog-reset back into the card — the self-healing appliance story carries over.
- **Same coherency posture**: the kernel `_board_entry` self-sweep owns data coherency; mmc reads land via U-Boot's own path; residual I-fetch hazard gated by crc32+beacon exactly as characterized on the USB rounds.

## A.2 Differences worth calling out

1. **mmc scans before usb0** — `boot_targets=mmc0 mmc1 nvme mtd2 mtd1 mtd0 usb0 …`. An SD card with a valid `/BOOT.SCR` **wins over the USB stick unconditionally**. Corollary: having both media inserted is a precedence trap; the bench must pick one canonical medium.
2. **eMMC is scanned first and is populated** (vendor firmware). Today's chain demonstrably falls through the entire list to the prompt, so the eMMC contributes no successful boot script — the SD's boot.scr is the first hit. A0 re-confirms nothing on eMMC partition 1 shadows us (one `fatls`/`part list` on mmc 0).
3. **Operator ergonomics favor SD**: the card is written on the Mac with a normal SD reader (mount the FAT, copy three files) — no port juggling, no USB-3-port reservation; the USB2-A ports stay dedicated to keyboard/mouse and the USB3 ports go free.
4. **Recommendation — standardize the bench on the SD card** as the boot medium once A2 passes, keeping the USB stick as the documented, already-characterized fallback. Reasons: scan precedence makes SD the deciding medium whenever present anyway; better operator loop; and **one card does both** — partition 2 carries the eofs store (Part B), so the boot medium and the persistent storage are the same physical object: the demo appliance gains a compile cache that survives power cycles (§B4) with no extra hardware.
5. **Partition geometry hygiene**: start partition 1 at ≥ 32768 sectors (16 MiB), clear of the Rockchip loader offsets (idbloader at sector 64, u-boot.itb at 16384 — orange-pi-5-plus.md "SD card layout"). The BootROM boots from eMMC before SD here so nothing on our card is ROM-read today, but this keeps the card forward-compatible with a from-scratch mainline-U-Boot card and aliases nothing.
6. **Mac handling rule (new)**: macOS will offer to "initialize" the unreadable eofs partition 2 every insertion — the operator must decline; only the FAT partition is ever touched from the Mac. (Whole-card rewrites go through the xtask image + `dd`, which rebuilds both partitions.)

## A.3 Bench rounds (mirroring the USB A-ladder; most gates already burned down)

Already done by the USB/kexec work, **not repeated**: staged bootargs (merged, QEMU-regressed), CRC-gate construction (A0 recon: no dcache/icache cmds, `crc32`-save + `cmp.l` form proven available), scriptaddr-collision characterization, hands-free chain characterization, extlinux avoidance, `bdinfo`/env ground truth. **SD's incremental risk is exactly one question: does vendor U-Boot read this slot.**

- **SD-A0 (recon, one prompt session, minutes)**: `mmc list` (which index is SD; eMMC present as which); `mmc dev <sd>` + `mmc info` (capacity, **negotiated bus mode/voltage — feeds risk R5**); insert the prepared card, `fatls mmc <sd>:1` (sees BOOT.SCR/EO9.IMG/BOOTARGS.TXT); `part list mmc <sd>`; quick `fatls mmc <emmc>:1` for shadow check. Probe-first-resume doctrine applies; never touch `/dev/cu.usbserial*` rules unchanged.
- **SD-A1 (manual)**: `fatload mmc <sd>:1 0x00100000 BOOTARGS.TXT`, `fatload … 0x00200000 EO9.IMG`, CRC gate, `go` — expect byte-identical behavior to the USB A1 round (the kernel cannot tell the media apart).
- **SD-A2 (hands-free)**: power-on with card in → Eo9 boots, zero typed commands, ×3 cold boots; card out → today's bench exactly; card in + corrupted EO9.IMG → CRC gate fails through to prompt (fail-safe pin).

Sizing: 2 short bench rounds (A0+A1 can share a session), ~half a day including card prep tooling, **given** the USB lane's xtask builder exists to extend.

---

# PART B — SD as a block device for eofs

## B.1 The driver: `eo9-sdmmc` core + `disk.sdmmc` shell (the OHCI precedent, exactly)

Placement follows usb-ohci-plan §2 / plan/09 D46 verbatim:

- **`crates/eo9-sdmmc`** — pure no_std core: the SD card state machine (CMD0 → CMD8 (check pattern 0xAA) → ACMD41 loop with HCS, busy-bit 31 → CMD2 CID → CMD3 RCA → CMD9 CSD (capacity) → CMD7 select → ACMD6 4-bit → CMD16 where SDSC), CSD/CID/SCR decode, and the DW MSHC register HAL over the same `RegionIo`-style trait eo9-ohci uses: CMD/CMDARG with start-bit + update-clock-only sequencing, RINTSTS poll-and-clear, CTRL FIFO/DMA resets, CLKDIV/CLKENA bracketed by clock-update commands, PWREN, CDETECT, FIFO data port. Host unit tests pin every command encoding and the init FSM against **scripted register traces** (the eo9-ohci encode/decode-pin discipline — this is the QEMU-gap compensation, §B6).
- **`disk.sdmmc`** — thin guest shell over `eo9:platform`: claims the `sdmmc` region, runs the core, exports `eo9:disk/disk` (the WIT needs **zero changes** — `size`/`read`/`write`/`flush` with the owned-buffer round-trip map directly). D46 driver discipline verbatim: take/put slot, bring-up claim, bounded polls, typed errors never traps. Byte-addressed `eo9:disk` over 512-byte sectors = read-modify-write at the edges, same as `virtio_blk.rs::write_bytes` (lines 528–553) — lift that shape, not new design.
- **Kernel embedding for storedisk** (§B4): the same core crate behind eofs's sync `BlockDevice` trait (device.rs) — exactly how `eo9-eofs` already serves both the kernel (`diskcache.rs` over `VirtioBlk`) and the guest (`fs.eofs` over `AsyncBlockDevice`). One protocol implementation, two shells.
- **SoC plumbing stays kernel-side** in `arch/aarch64/rk3588_sdmmc.rs` (the rk3588_usb.rs template: every constant cited, print-before-touch, idempotent hiword writes): PD_SDMMC check (expected ON — U-Boot scanned mmc this boot), clock state recon, pinctrl assumed inherited from firmware. CRU/PMU/SCMI are **never granted out**; the guest gets only the 16 KiB register block.

**Data path, honestly sized.** v1 = **PIO** through the 256-deep FIFO, **single-block CMD17/CMD24 first, multi-block CMD18/CMD25 + auto-stop in the same lane** (eofs blocks are 4 KiB = 8 sectors; single-block-only makes every fs op 8 command cycles — multi-block is days, not weeks, and the DW MSHC SEND_AUTO_STOP bit does CMD12 for us). Expected PIO throughput ~1–4 MiB/s at HS-50 MHz — slow but tiny and with **zero DMA surface**. v2 (recorded, not scheduled) = **IDMAC descriptor rings**: the controller is **not dma-coherent** (no `dma-coherent` on the node, same as PCIe/USB), so the full civac bracket discipline from `dma.rs`/the pci provider applies to descriptors and data buffers; that rung exists for storedisk cache loads of multi-MiB artifacts. Sizing against the reference: dw_mmc.c's 3,629 lines cover SDIO, eMMC, UHS tuning, interrupts, pm, fault paths we are explicitly not doing; the v1 core is estimated **~900–1,300 lines + tests** (FSM + HAL + PIO), comparable to eo9-ohci's core.

**Scope cuts (deliberate, recorded):** 3.3 V high-speed only — **no 1.8 V switch, no SDR104, no tuning loop** (that is what `dw_mmc-rockchip.c`'s 594 lines mostly are; the sample-phase loop only matters for UHS rates). No SDIO, no eMMC (different controller anyway), no card hot-swap mid-boot (card present at claim or typed error; CD is GPIO0_A4, not the controller's CDETECT — see R4).

## B.2 The capability story

- **Region grant**: one new board `RegionDef { name: "sdmmc", base: 0xfe2c0000, size: 0x4000, has_irq: true /* SPI 203, unused v1 */ }` in `platform_regions` (arch/aarch64/mod.rs). Grantable as `platform=sdmmc` (least authority — a boot that grants the SD slot does not hand out the OHCIs) or swept in by the bare `platform` token. Claims are machine-wide-exclusive already (platform_provider.rs) — that is load-bearing for the storedisk story below.
- **Quiesce hook from day one** (the OHCI freed-heap lesson, GAPS "Platform-provider DMA teardown" — fixed via `RegionDef::quiesce`): for PIO v1 the device has no autonomous DMA, but the hook still must leave the controller inert: CLKENA off + CTRL FIFO/DMA reset + (when IDMAC lands) BMOD software-reset and wait — so the v2 DMA rung inherits a correct release path instead of re-learning the HCCA-into-freed-heap incident on descriptors.
- **Warm vs cold init** (the CONFIGFLAG analogue): vendor U-Boot scanned and initialized the card this boot — the card sits selected, possibly in 4-bit/HS, **possibly in 1.8 V signaling if vendor U-Boot did UHS** (the dts advertises sdr104; R5). The driver always **cold-inits**: PWREN off → ~10 ms → on (a real VDD power cycle through the slot rail, which is the only thing that exits a latched 1.8 V signaling state) → 400 kHz ID clock → CMD0. Idempotent regardless of firmware history, same doctrine as the OHCI CONFIGFLAG-clear-first rule. U-Boot's prior init is treated as **recon material only** (M1 dumps CLKDIV/CLKENA/UHS_REG before touching anything).
- **Clock authority — the one genuinely new wrinkle (R2)**: biu/ciu are SCMI clocks (TF-A owns them). Plan A: never call SCMI — U-Boot left CCLK_SD running at a working rate to enumerate the card; derive the 400 kHz ID clock and the runtime clock with the **IP-internal CLKDIV** from whatever rate U-Boot left (read-don't-write recon at M1 establishes the base rate by measuring against the 24 MHz timer if needed). Plan B (only if A's discriminator fails): a minimal SCMI-over-SMC clock client (~200–400 kernel lines; BL31 is present, PSCI-over-SMC already works). Plan A is strongly preferred — no new firmware interface surface.

## B.3 eofs over it: partitions, durability, one-card-does-both

**How fs.eofs binds today**: `fs.eofs` imports `eo9:disk/disk` and mounts/auto-formats via `default()` over the *whole* device (`disk.mem $ fs.eofs $ program`; the flagship metal chain `pci.filtered $ disk.virtio $ fs.eofs`). There is no partition concept anywhere in the disk chain — and the SD card **must** carry the boot FAT (partition 1) plus eofs storage without either trampling the other.

**New component: `disk.part`** — a pure attenuation middleware in the `disk.readonly` mold (wit/disk already shows the world shape: import disk, export disk + config): reads the MBR once at bind, exposes partition N as a windowed `eo9:disk` (offset translation + `size()` = partition length, out-of-range typed). `disk.sdmmc $ disk.part --partition 2 $ fs.eofs $ program`. This is **fully usermode/QEMU-testable** over `disk.mem`/`disk.virtio` with an MBR fixture image — it lands in M0 with complete coverage before any silicon. (Whole-device raw eofs is rejected for this card: it would destroy the boot partition; `disk.part` is also what protects p1 from a misconfigured chain — pointing `fs.eofs` at partition 1 hits the foreign-image refusal, which refuses FAT outright rather than reformatting. The hardened blank-check (GAPS round-3 ruling B) makes a fresh all-zero p2 auto-format correctly.)

**Card layout (one card does both):**

```
MBR
p1  FAT32, active, from 16 MiB, ~256 MiB:  BOOT.SCR, EO9.IMG, BOOTARGS.TXT   (U-Boot reads; Mac writes)
p2  type 0xDA (non-FS data), rest-of-card: eofs                              (Eo9 writes; nobody else touches)
```

U-Boot owns the card only until `go` and only ever reads p1; after `go` the kernel/guest owns the controller. On a watchdog reset U-Boot re-reads p1 — eofs commits are windowed to p2 by `disk.part`, so the boot path can never be corrupted by fs traffic. xtask grows one card-image builder (extends the USB stick builder; emits the whole-card image for `dd` and the three files for FAT-mount updates).

**Write durability — honest section.** `eo9:disk/flush` promises "every completed write durable before return". On SD that promise is **not fully keepable**: the SD command set has no standard flush/FUA (the SD 6.x cache-flush extension is optional and not v1 scope). `disk.sdmmc::flush` = drain the controller (DATA_BUSY clear) + poll CMD13 SEND_STATUS until the card returns to `tran` (programming complete). That makes writes *card-accepted*, but a consumer-grade card's FTL can still lose or — worse — **tear whole erase-block neighborhoods (hundreds of KiB) on power cut**, which is strictly nastier than the torn-sector model eofs's checksums were designed around: a single power cut can plausibly take out **both adjacent uberblock slots** (the open S7-9 geometry decision, GAPS "Uberblock geometry" — SD makes that decision *more* urgent; this lane should be cited as new input to it, not silently absorb the risk). Posture: document `flush` semantics in the provider header the way fs-eofs documents durability today ("durability is the underlying device's"); the M3 power-cut soak (risk table) measures reality; the storedisk payload is the right first tenant *because* it is a cache — every entry is MAC-verified and recompute-on-miss, so worst-case corruption costs a recompile, never correctness (diskcache.rs trust model carries over unchanged).

**storedisk on SD — the dev-loop win.** Today `diskcache.rs` only backs onto `VirtioBlk` (PCI probe), so on the board the `storedisk` token degrades to "no cache" — **the board has no persistent storage at all today**, and every composition's fiber-sliced on-target compile is repaid after every watchdog reset/power cycle. M4 gives `diskcache` a second backend: the `eo9-sdmmc` kernel embedding implementing `BlockDevice` over p2 (board profile selects sdmmc, QEMU keeps virtio — one cfg seam in diskcache.rs). Result: the compile cache and the writable MAC-verified `/bin` (`save` at the prompt) **survive reboots on the bench** — pre-warmed compositions come back at cache-hit speed (~0.3 s analog vs multi-second compiles), and the demo appliance's first typed command stops paying the cold-compile tax after its first life.

**Sharing rule (explicit, improved over the virtio caveat).** virtio_blk.rs documents "kernel claims the function; don't also grant it to a guest" as a *convention*. The SD kernel embedding must do better: it **registers its claim in the machine-wide platform claim table**, so a same-boot guest `disk.sdmmc` claim gets the typed `busy` refusal instead of two masters on one FIFO. One boot = one controller owner: M3 boots grant `platform=sdmmc` to guests (no storedisk token); M4 boots use `storedisk` (kernel owns it, guests get typed busy). A kernel-owned controller *serving* partitions to guests (the shared-eofs/call-gate candidate from the IOPS audit's composition section) is the recorded refinement, not v1.

## B.4 Where this lands on the IOPS ladder (disk-iops-audit.md, stated plainly)

The kernel/board block path is the audit's bottom rung — "polled, single-request" with `QUEUE_SIZE=16` declared but QD1 actual (audit Part 1, virtio finding; named bottleneck #6) — and **`disk.sdmmc` v1 deliberately joins it there**: polled, one request in flight, PIO. eofs-on-SD additionally inherits bottleneck #5 (fs.eofs serializes; concurrent fs ops are typed-busy) and the several-disk-ops-per-fs-op commit amplification. On top, the *medium* is the floor: consumer microSD does ~1–3k random-read and ~100–500 random-write IOPS — below even the polled path's ceiling, so **the driver will not be the bottleneck; the card is**, and no rung of the host ladder changes that. Sequential storedisk artifact loads are where PIO (~1–4 MiB/s) visibly hurts vs IDMAC; that is the v2 motivation, with measured numbers from M4 to justify it. The batched `submit/reap` WIT rung (audit Part 3) is noted as the future shape — IDMAC descriptor chains are exactly its natural device mapping, same as the audit says for virtio rung 3 — but **nothing in this plan depends on it**.

## B.5 Milestone ladder (bench-style; M0 burns everything burnable off-board)

- **M0 (no bench time)**: `eo9-sdmmc` core + host tests (command encodings, init FSM over scripted register traces, CSD capacity math incl. SDSC/SDHC split); `disk.part` + usermode tests over `disk.mem` (MBR fixture, window math, p1-protection refusal via foreign-image probe); board region table entry + quiesce hook; xtask card-image builder; full battery green, canonical demos unchanged.
- **M1 (board) — controller probe + CID**: rk3588_sdmmc.rs print-before-touch (PD_SDMMC read, clock/divider recon dump, CDETECT + GPIO0_A4 observation with card in/out); PWREN power-cycle; 400 kHz ID sequence; **acceptance: printed CID/CSD matches the physical card's label and capacity**. Failure localization mirrors the USB table: no register reads → PD/HCLK; CMD0 ok but CMD8 dead → clock rate or voltage (R2/R5); ACMD41 never ready → power-cycle insufficiency.
- **M2 (board) — single-block read + boot-partition fatcheck**: CMD7/ACMD6/CMD17 PIO; read sector 0, parse and print the MBR (must match the card we built); read p1's boot sector, verify the FAT signature, locate and CRC `BOOT.SCR` — **acceptance: the printed CRC equals the one xtask baked**, proving end-to-end read integrity against ground truth the bench already owns. CMD24 write + read-back to a p2 scratch offset. Multi-block CMD18/25 + auto-stop close this milestone.
- **M3 (board) — eofs mount + reboot-survives**: `disk.sdmmc $ disk.part --partition 2 $ fs.eofs $ <fscheck>`: format-on-blank, write a file, read back; reboot (exit-is-reset); remount; **acceptance: the file survives a cold power cycle**. Plus the power-cut soak (risk R6): N pulls mid-commit-loop, every remount either clean or warn-and-fall-back (the S7-1 rollback posture), never unmountable.
- **M4 (board) — storedisk on SD**: diskcache over the kernel embedding + the claim-table registration; boot `storedisk`-token; **acceptance: compile a composition, power-cycle, recompose — cache hit (timed); `save` at the prompt survives the cycle**; and the both-grants boot proves the typed-busy sharing rule.
- **Recorded follow-ups (not scheduled)**: IDMAC + civac (v2), IRQ SPI 203, SD-cache-extension flush probing, the kernel-owned-controller partition-serving refinement, batched WIT rung.

Sizing: M0 ~2–3 dev days; M1–M4 ~3–5 bench rounds (M1+M2 plausibly one round — the protocol is far more deterministic than USB enumeration), each round bounded by the standing bench doctrine (serial dev loop, probe-first resume, watchdog self-recovery).

## B.6 QEMU coverage — honest assessment

- **QEMU does not model the DW MSHC.** `-M virt` has no SD controller at all, and QEMU's SD host models (generic `sdhci-pci`/`sdhci`, pl181, allwinner, bcm2835) are all **different programming models** — unlike the OHCI lane, where `pci-ohci` was the *same* controller, there is no QEMU vehicle for our HAL. Driving `-device sdhci-pci -device sd-card` would test QEMU's SDHCI, not our dw_mshc code; building an SDHCI shell just to battery-test the card FSM is extra driver surface and is **rejected for v1** (recorded as an option if the FSM ever grows tuning complexity).
- **QEMU/host-testable anyway**: the entire card FSM and register HAL via scripted-trace host tests (M0); `disk.part`, fs.eofs-over-partition, the foreign-image p1 refusal, and the diskcache logic — all over `disk.mem`/`disk.virtio`/file-backed images, which is where eofs coverage already lives. The storedisk *cache semantics* stay QEMU-covered via the existing virtio backend; only the SD `BlockDevice` adapter itself is board-only.
- **Board-only residue (named)**: PD/clock/divider plumbing, the SCMI question, PWREN power-cycling, 3.3 V-vs-warm-1.8 V behavior, PIO FIFO pacing, CD wiring, real power-cut durability. Every one has an M1–M3 discriminator above.

## B.7 Risks → discriminating tests

| # | Risk | Discriminating test |
|---|---|---|
| R1 | Vendor U-Boot can't read the SD slot / wrong index | SD-A0: `mmc list`, `mmc dev`, `fatls` — one prompt session settles Part A's entire incremental risk |
| R2 | SCMI-owned biu/ciu: kernel can't derive a card clock without TF-A calls | M1 recon: dump CLKDIV/CLKENA, attempt update-clock-only command, CMD0/CMD8 at divided rate. Pass → Plan A holds; fail → minimal SCMI-SMC set_rate (Plan B, ~200–400 lines) |
| R3 | PD_SDMMC gated under the `go` flow | M1 print-before-touch PMU read (rk3588_usb pattern); prompt-side `md` cross-check. Expected ON (U-Boot scanned mmc) |
| R4 | Card-detect wiring (CD is GPIO0_A4, not controller CDETECT — the muxed `sdmmc_det` may read constant) | M1: CDETECT + GPIO0_A4 with card in/out. v1 doesn't depend on CD either way: absent card = bounded CMD timeout → typed error |
| R5 | Vendor U-Boot left the card in 1.8 V UHS signaling; our 3.3 V re-init fails | SD-A0 `mmc info` shows the negotiated mode; driver's PWREN VDD power-cycle is the unconditional mitigation; discriminator on board: CMD8 dead only when warm-after-U-Boot but fine after manual power cycle |
| R6 | SD power-cut tears erase-block-sized regions — both uberblock slots in one event (S7-9 worsened) | M3 power-cut soak (N pulls mid-commit; remount verdict each). Finding feeds the open S7-9 owner decision — not silently absorbed |
| R7 | Two masters on one controller (storedisk + guest driver) | M4 both-grants boot: guest claim must answer typed `busy` (claim-table registration), pinned by a check gate |
| R8 | PIO FIFO underrun/overrun at HS rates (polled loop vs 256-deep FIFO) | M2 multi-block read of a known-CRC region at increasing clock; garbled-at-rate → drop to 25 MHz (typed, logged), record for the IDMAC rung |
| R9 | fatload-from-mmc coherency differs from USB path | Same crc32+beacon gate as USB A1 — already designed, just re-run on SD-A1 |
| R10 | Mac "initialize?" prompt nukes p2 | Bench rule (§A.2.6) + xtask whole-card image makes recovery one `dd` |

## Workarounds / assumptions (standing rule: every one reported)

1. **U-Boot mmc index assumed mmc1=SD from mainline aliases** (dts:19–22) — vendor 2017.09 may number differently; SD-A0 is the verification, and no artifact depends on the index (distro-boot variables).
2. **Vendor U-Boot 2017.09 SD readability is assumed, not yet proven** (it lists mmc0/mmc1 in boot_targets and shipped firmware boots from eMMC via the same MSHC family, so confidence is high) — SD-A0 is the gate before any further Part A work.
3. **The eMMC is left entirely alone** (vendor firmware lives there; it is also a different controller — `dwcmshc` SDHCI-class — that this lane deliberately does not touch).
4. **Plan A clocking rides U-Boot's warm CCLK state** (SCMI clocks untouched) — a deliberate dependency on firmware-left state, the same class as the VOP2 scanout adoption in gfx.simplefb; Plan B (SCMI client) is sized if M1 falsifies it.
5. **`flush` on SD is "card-accepted", not "FTL-durable"** — documented in the provider, soaked at M3, and the first tenant (storedisk) is corruption-tolerant by construction.
6. **Line counts and register facts** were verified against fetched v6.12 sources this session (dtsi/dts/dw_mmc*); DW MSHC register-level offsets (CLKDIV/RINTSTS/etc.) are from the dw_mmc.h fetched copy and will be cited per-constant in `rk3588_sdmmc.rs` at implementation time, the rk3588_usb.rs discipline.
7. **Stale memory note**: the auto-memory "Orange Pi BLOCKED 06-07, no serial" entry is superseded by orange-pi-5-plus.md (board alive, on network, 06-08 — the swapped-supply suspicion was confirmed as the cause and is now a bench rule).
8. Read-only session: no repo files touched, no serial port touched; web fetches limited to kernel.org-mirrored raw files on GitHub (cited above).

**Dependency note for sequencing**: Part A is independent and cheap (ride the USB lane's tooling; 2 rounds). Part B's M0 is independent of all board lanes; M1–M4 want the platform-provider claim table as-is (merged) and should sequence after the USB silicon lane's bench-critical path (the demo) unless the owner wants persistent storage sooner — M4 is the single biggest dev-loop improvement on the board (compile cache surviving reboots) and is a defensible queue-jumper.
