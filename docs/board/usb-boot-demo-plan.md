# USB boot + the standalone demo (HDMI shell, USB keyboard, curl)

Plan 2026-06-08. Demo: power on → Eo9 boots from a USB stick → eosh on HDMI →
commands typed on the physical USB keyboard → `curl http://…` prints a real website.
Companion plans: usb-ohci-plan.md (keyboard lanes M1-M4), hdmi-simplefb-plan.md.

## Part A — USB boot, hands-free (CONFIRMED feasible with zero firmware changes)

Live `printenv` capture (bench, 2026-06-08, log .claude/board-bringup/logs/uboot-env-*):
- `boot_targets=mmc0 mmc1 nvme mtd2 mtd1 mtd0 usb0 pxe dhcp` — usb0 IS in the stock
  distro scan chain; with no other media the chain reaches it (today's fail-through
  to opi# proves the chain runs to its end).
- `boot_scripts=boot.scr.uimg boot.scr`; `scan_dev_for_boot_part` honors -bootable
  partitions else partition 1; `boot_a_script` loads + sources the script.
So: FAT32 partition 1 on the stick + `/boot.scr` → power-on boots Eo9, no setenv, no
saveenv, no typed commands; stick out = today's bench exactly.

### Load path facts (from the serial loader + kernel entry sources)
- The image is a flat arm64 Image linked run-in-place at 0x00200000, entry at offset 0.
  `fatload usb …:1 0x00200000 EO9.IMG` + `go 0x00200000` — the 0x04000000 stub is not
  involved. EL2 entry state identical to the serial path.
- Cache: kernel `_board_entry` does its own dc civac sweep of the whole footprint —
  no loader-side data sweep needed. Residual hazard: I-fetch of the first lines of
  entry code; gate = crc32-after-fatload + beacon check (Round A1); mitigations
  staged (cache-pressure crc32, dcache/icache cmds if present).
- **x0 problem and the chosen fix**: `go` passes junk x0 (kernel tolerates → no
  bootargs → no grants — fatal for the demo). Fix: kernel staged-bootargs fallback —
  reserve a page at 0x00100000 (board profile, heap-excluded); fdt::bootargs() tries
  x0 first (serial path unchanged), else sweeps + reads a plain text command line
  (or valid FDT) from the staging page. The stick carries `BOOTARGS.TXT`; boot.scr
  fatloads it to 0x00100000. Editing the demo composition = editing a text file.
  Rejected: a second-stage shim (new artifact + coherency bootstrap + baked FDT
  address); fdt-set games (control-FDT address stability + quoting); baked-in args.
  **A1 field finding (2026-06-09): "kernel tolerates junk x0" was FALSE.** The boot
  ran A…G + PCIe + USB peeks then hung before `D` into the 22 s watchdog loop:
  `go` puts argc (x0=1) in x0, and the old fallback chain's plain-cmdline probe
  *dereferenced* it — a cacheable read into the secure bottom MiB of DRAM (TF-A
  behind the DDR firewall), which stalls the interconnect with no exception.
  Hardened (area/43): a single fdt::validate() choke point — null/8-alignment/
  DRAM-window/magic/totalsize checks BEFORE any dereference, the FDT decided
  absent ONCE with one loud line, the plain-cmdline probe confined to x86_64 PVH,
  `I`/`J`/`K` stage beacons in the formerly silent window, and the junk-x0 matrix
  gated under QEMU (`cargo xtask check-x0`, the `x0matrix` boot token).

### boot.scr (mkimage -T script; xtask emits it with the image CRC baked)
```
fatload ${devtype} ${devnum}:${distro_bootpart} 0x00100000 BOOTARGS.TXT
fatload ${devtype} ${devnum}:${distro_bootpart} 0x00200000 EO9.IMG
if crc32 -v 0x00200000 ${filesize} <crc>; then go 0x00200000; else echo EO9: CRC mismatch; fi
```
(hush-if/crc32 -v availability = Round A0 recon; degrade to unconditional go + the
serially-proven-images rule if absent.)

### Stick + bench rules
- MBR, one FAT32 partition marked active, 8.3 uppercase names: EO9.IMG (52 MiB full /
  23 MiB min; structural cap 62 MiB before the stub home), BOOTARGS.TXT, BOOT.SCR.
- Mac write: diskutil partitionDisk + fdisk -e active flag; mount-policy fallback =
  mtools-built FAT image dd'd raw.
- Stick goes in a USB 3.0 port (keyboard/mouse own the USB2-A ports); U-Boot owns the
  stick only until `go`. A0 confirms vendor usb storage sees it there; fallback =
  stick in one USB2-A, mouse omitted.
- Dev loop STAYS serial; only serially-proven images get written to the stick (a
  pre-G wild jump can hang → power cycle with stick pulled). Post-G panics watchdog-
  reset back into the stick = self-healing demo appliance.
- Rounds: A0 recon → A1 manual fatload+go → A2 staged bootargs (kernel+QEMU) → A3
  hands-free power-on, stick in/out. ~4 bench rounds + 1-2 days dev.

## Part B — demo gaps

| Element | Status |
|---|---|
| USB boot | GAP — Part A |
| gfx provider | DONE (area/15, merging) |
| eosh on HDMI | GAP — fbcon kernel tee (below) |
| USB kbd protocol | DONE on QEMU (area/14, merging) |
| USB kbd silicon | GAP — M1→M3 (2-5 rounds, the critical path) |
| keys → eosh | GAP — M4 console-sink (1+1 rounds) |
| NIC/transport | DONE (merged; pin --advertise-max 1000) |
| DHCP | DONE (area/13, merging) |
| curl | GAP — new small example (below) |
| TLS | OUT by decision (feasibility note below) |
| boot composition | GAP — `station` config + fbcon token (small) |

### fbcon: kernel console tee to the framebuffer (the demo console)
8×16 blitter over the simplefb surface (100×30 cells, white-on-black = chroma-immune),
scroll + cursor, teeing every console byte at the UART output chokepoint; new `fbcon`
boot token, mutually exclusive with the `gfx` grant. Decisive arguments vs a gfx.text
component for THIS demo: (1) keystroke echo lives in the kernel read-line future — a
component console would not show typing until Enter; (2) boot banner / DHCP lease line
/ codegen narration / panics are kprintln's, visible only below the provider layer;
(3) init's `console =` takes a single program, no composition grammar exists. Output
policy: tee, not switch (serial transcripts remain the bench instrument). The gfx.text
component stays the principled follow-up on the gfx lane. ~300-500 kernel lines,
host-tested blitter math, capture-stick acceptance.

### curl (new example, l4check shape)
`eo9:net/l4` + text; `main(url, resolver: option)`. http:// only (https refuses typed,
naming the TLS decision); DNS via the l4check encoder factored into a shared module;
GET → status + headers count + body (16 KiB cap, honest truncation); one 301/302 hop;
typed failures; counts line. QEMU gate check-curl (python http.server fixture via
slirp host alias). Board: `--resolver 10.20.3.1 http://example.com` (neverssl.com as
anti-redirect fallback). Known caveats recorded: l4 graceful-close gap (harmless for
GET); lease DNS is logged-not-stored → curl takes --resolver (surfacing lease DNS is
a small net-lane follow-up).

### TLS (deferred lane, honest note)
embedded-tls (no_std TLS 1.3, RustCrypto) is the first candidate; rustls 0.23 no_std
needs a RustCrypto provider. The hard parts on Eo9 are NOT the ciphers: root-store
bytes are fine, but certificate validation needs WALL-CLOCK TIME (board RTC starts at
epoch — everything "not yet valid" until PMIC RTC or NTP lands) and entropy
provenance. Plan after the demo ships.

### Boot composition (LANDED 2026-06-09, area/29-svc-grants — fbcon still a gap)
BOOTARGS.TXT: `station pci platform console-sink fbcon kexec`. The `station` boot
token bakes the config: `kbd = usb.ohci --region usb-host0-ohci $ usb.kbd restart
restart.always` + `console = eosh` (restart always; the QEMU build's variant chains
`usb.ohci-pci` — no OHCI platform region there). What landed to enable it: init's
config grammar accepts `$` chains (names + `--flag value` + `$`, nothing richer),
and the kernel service registry links the boot-granted operator roots
(pci/platform/gfx/kexec/console-sink) plus ambient time/entropy into services —
operator-authored services are console-equivalent trust (SPEC "Services and
detachment", executor-model.md "kernel refinement"). Acceptance gate:
`cargo xtask check-station` (boot with the station token, zero typed commands,
QMP keys execute at the prompt). The network is deliberately NOT auto-started:
typing `net.rtl8125 --advertise-max 1000 $ (net.l4.over-l2 --address dhcp) $ curl …`
on the physical keyboard IS the demo — capability composition performed live, with
the sliced-codegen narration visible on HDMI during the fusion compile.

### Dependency graph + order
Merge train (13→14→15) blocks everything; then four parallel lanes: usb-boot (A0-A3),
usb silicon (M1-M4 + usb.kbd; THE critical path, 2-5 rounds), fbcon, curl. `station`
needs M4+fbcon+curl. Final: full-demo rehearsal over SERIAL first (same bootargs),
then the stick, then 3 consecutive cold-boot dress rehearsals.
Sizing: ~10-15 bench rounds total; dev ~5-7 days across lanes.

### Demo-specific re-validation note
A USB boot means vendor `usb start` ran (controllers touched, keyboard addressed,
EHCI CONFIGFLAGs possibly set). The OHCI driver already clears CONFIGFLAGs and does
its own port reset; each USB milestone adds a "after vendor usb start" arm (USB boot
makes it the production arm).

## Part A final revision (after the live env dump settled the open questions)

- **extlinux landmine**: `scan_dev_for_boot` checks `extlinux/extlinux.conf` BEFORE
  boot scripts, and `boot_extlinux`/sysboot funnels into the bootm/booti-class
  launcher — the exact family that data-aborted and wedged the board. The stick must
  carry NO /extlinux/ and NO /boot/ directory, and no boot.scr.uimg (scanned before
  boot.scr).
- **usb_boot runs `usb start` itself** — the script never needs to.
- **scriptaddr collision gate**: scriptaddr=0x00500000 sits INSIDE our image
  footprint; loading EO9.IMG overwrites the sourced script's load address mid-
  execution. 2017.09's `source` → run_command_list mallocs a heap copy (relocated
  high RAM) so this should be safe — GATED in Round A1 (`echo one; load big; echo
  two`); degradation = two-stage chain-load via a boot2.scr at 0x10000000.
- **bootdelay=0**: autoboot non-interruptible; recovery from any bad-stick state =
  pull the stick (chain fails through to opi# exactly as captured). Firmware stays
  untouched — we do not raise bootdelay.
- **fdtcontroladdr confirmed ABSENT** from the env; `fdt_addr_r=0x0a100000` is a
  distro load target, NOT the control FDT. gd->fdt_blob (0xeb9f6c38) is fixed at
  relocation, flow-independent, stable across all campaigns; Option 1 avoids baking
  it anywhere regardless.
- **Bootargs handoff menu (final)**: Option 1 (recommended) = BOOTARGS.TXT staged at
  0x00100000 + ~10-line kernel board fallback (x0-valid always wins; bounded
  first-line parse defends against warm-reset DRAM residue). Option 2 (zero kernel
  change fallback) = a script-poked 6-word trampoline at 0x00180000 setting
  x0=0xeb9f6c38 then br to the image (the mm.l-stub pattern in miniature; costs
  hand-assembled words + baked FDT address + re-mkimage per bootargs change).
- **Cache ranking**: the kernel's own entry self-sweep owns data coherency; USB
  mass-storage DMA lands at PoC (better than the serial mm.l case); residual = first
  ~3 I-fetch lines, gated by crc32+beacon in A1; staged mitigations: dcache/icache
  cmds (A0 recon `help dcache`), the proven crc32 cache-pressure eviction, or the
  trampoline growing a civac loop.
- **CRC-fail path**: scan chain continues benignly (pxe/dhcp time out, the
  boot_android/bootrkp tail fails as captured today) → prompt. Fail-safe, hands-free.
- **A0 narrowed** (printenv done): `help dcache/icache/crc32/itest` + stick-behind-
  the-xhci1-hub `usb storage` check. Then A1 (manual load+go + the two gates), A2
  (kernel fallback + QEMU regression + cmdline bench), A3 (hands-in-pockets ×3).
- Freebie: stdout=serial,vidconsole means the monitor narrates from power-on until
  fbcon takes over — demo continuity for free.

## Round A0 results (bench, 2026-06-08 evening)
- `dcache`/`icache` commands ABSENT (no CONFIG_CMD_CACHE) — cache mitigation if A1's
  beacon gate fails = the proven crc32 cache-pressure eviction or the trampoline.
- `crc32 address count [addr]` exists (no -v verify form); `itest` ABSENT; `setexpr`
  ABSENT; `test` EXISTS; `source` EXISTS; hush `if` proven by the env's own scripts.
- **CRC gate construction without -v/itest/setexpr**: `crc32 0x00200000 ${filesize}
  0x00180000` (save), `mw.l 0x00184000 <expected> 1`, `if cmp.l 0x00180000 0x00184000
  1; then go 0x00200000; fi` — cmp returns success on equality. A1 verifies `cmp`
  exists (standard mem command family; mm/md proven present).
- Pending A0 residue: stick-behind-xhci1-hub `usb storage` check (stick not yet
  plugged; operator asked to put it in a USB 3.0 port).
