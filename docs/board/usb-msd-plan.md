# USB mass storage + network stick rewrite — implementation plan

Plan 2026-06-09. Owner feature: "get USB r/w working and then write [the boot stick]
over ethernet" — the board rewrites its own USB boot stick from a network-received
image, closing the self-hosted dev loop. Today the loop is Mac-side `dd` (needs sudo)
or serial (slow); the end state is `send_image --tcp-stick` → a board-side service
writes `EO9.IMG` onto the stick's FAT partition → reset boots the new image
hands-free. No code in this document; sources cited inline.

**Sources read:** docs/board/usb-boot-demo-plan.md (the stick section + A0 results),
usb-ohci-plan.md, sdcard-plan.md (disk.part + the layering precedents),
orange-pi-5-plus.md, wit/usb/usb.wit, wit/disk/disk.wit, crates/eo9-ohci/src/
(driver.rs, schedule.rs, lib.rs), guest/examples/oskexec/src/lib.rs,
kernel/eo9-kernel/src/wasm/svc.rs (the svc-grants posture), xtask check-usb /
check-kexec gate shapes, docs/design/component-manuals.md. **Verified live this
session:** QEMU 11.0.0 `-device usb-storage,bus=<pci-ohci>.0` attaches at
**Speed 12 Mb/s (full speed)** on the OHCI bus (`info usb` smoke test) — the QEMU leg
is real, same vehicle as check-usb.

## 0. The v1 shape in one paragraph

Four small pieces, strictly layered: **(1)** the eo9:usb WIT grows bulk endpoints and
the OHCI driver grows the bulk list (the encodings — ED, TD, `TdToggle::FromEd`
toggle-carry — already exist; the gap is list management); **(2)** a new `usb.msd`
component (core crate `crates/eo9-msd` + thin stub shell, the eo9-ohci/eo9-rtl8125
precedent) speaks Bulk-Only Transport + six SCSI commands over eo9:usb and **exports
eo9:disk** — msd is a disk provider, nothing more; **(3)** partition awareness stays
in `disk.part` (the sdcard plan's M0 component, shared infrastructure — this lane
co-owns its delivery) and FAT knowledge lives in a pure host-tested crate inside the
flasher, NOT a new fs component: `EO9.IMG` is padded to a **fixed slot size** at
stick-build time so every rewrite is a same-size in-place cluster overwrite — zero
FAT allocation logic; **(4)** a sibling-of-oskexec service `stickflash` (same wire
protocol, factored into a shared crate; different sink and a capability-narrower
world) receives the image over TCP, overwrites the EO9.IMG clusters, read-back-CRC
verifies, and writes `BOOT.SCR` **last** as the commit point — a torn write fails the
existing boot-time CRC gate and falls through to the prompt (fail-safe; recovery is
the serial path, accepted). Full loop ≈ 2–3 minutes at the full-speed ceiling —
honest, and fine for the dev loop.

## 1. usb.msd — the driver component

### 1.1 What the keyboard chain already provides, and the exact gap

eo9:usb today (wit/usb/usb.wit): `controller`/`port`/`attach`/`attach-child`,
`control-in`/`control-out`, `open-interrupt-in`/`read`. The WIT says it itself:
"Bulk and isochronous endpoints arrive with their first consumer" — this is the
first consumer. In crates/eo9-ohci:

- **Already there**: ED/TD encode/decode (schedule.rs), `TdToggle::FromEd` (0b00,
  toggle from the ED's toggleCarry — exactly what bulk needs; control uses explicit
  DATA0/1), done-queue walk, bounded polls, the bring-up that zeroes
  `HC_BULK_HEAD_ED`/`HC_BULK_CURRENT_ED` (driver.rs:237–239) and the register
  constants incl. BulkListEnable/BulkListFilled (lib.rs:71–134).
- **The gap, precisely**: nothing ever populates the bulk list. Needed: a bulk ED per
  endpoint (IN and OUT) hung off HcBulkHeadED; TD queueing with FromEd toggles;
  setting BulkListFilled in HcCommandStatus after queueing and BulkListEnable in
  HcControl; done-queue completion plumbed to a bulk read/write call; halted-ED
  recovery (clear the ED Halt bit + CLEAR_FEATURE(ENDPOINT_HALT) via the existing
  `control-out`, then reset toggleCarry — BOT stall recovery requires this).
  Structurally identical to the control path minus the setup/status stages, as
  predicted. One TD spans up to two 4 KiB pages; v1 issues one TD at a time and
  loops (8 KiB per round is plenty against a 1 MB/s bus) — TD chaining is a recorded
  follow-up, not v1.
- **Mock**: MockOhci grows `process_bulk_list()` next to `process_control_list()` +
  a scripted bulk device; the encode/decode-pin discipline carries over.

WIT addition (transfer-shaped, mirroring `open-interrupt-in`):
`open-bulk-in(d, endpoint, max-packet) -> endpoint`,
`open-bulk-out(d, endpoint, max-packet) -> endpoint`,
`bulk-read(e, length) -> list<u8>` (short reads normal, same contract as `read`),
`bulk-write(e, data)` — toggle state lives in the provider per endpoint. Both shells
(`usb.ohci`, `usb.ohci-pci`) pick the new functions up from the shared core; `usb.deny`
grows the refusals; the worlds/refusal tests follow the existing pattern.

### 1.2 The msd protocol layer

New pure no_std core **`crates/eo9-msd`** (the eo9-ohci/eo9-rtl8125 D46 placement,
controller-agnostic, consumer-side):

- BOT (USB Mass Storage Class Bulk-Only Transport 1.0): CBW encode (31 bytes,
  `USBC` signature, tag, transfer length, direction, LUN, CB), CSW decode (13 bytes,
  `USBS`, tag match, residue, status), the phase rules, and the error recovery
  ladder (CSW status 1 → REQUEST SENSE; stall → clear-halt + retry; status 2 /
  protocol garbage → BOT mass-storage reset (class request 0xFF) + both clear-halts).
- Minimal SCSI set: **INQUIRY, TEST UNIT READY, REQUEST SENSE, READ CAPACITY(10),
  READ(10), WRITE(10)** — six commands, fixed-size CDBs, all encode/decode
  host-test-pinned against byte fixtures. READ CAPACITY(10) caps at 2 TiB which is
  comically sufficient for a boot stick; READ(16) is a non-goal.
- GET MAX LUN (class request) read once, LUN 0 used unconditionally (typed error if
  the stick insists otherwise — sticks don't).

Guest stub **`guest/stubs/usb-msd`** → component **`usb.msd`**: imports eo9:usb,
exports **eo9:disk** (`size` from READ CAPACITY, byte-addressed `read`/`write` over
512-byte sectors with read-modify-write at the edges — lift the
`virtio_blk.rs::write_bytes` shape as sdcard-plan §B.1 already prescribes; `flush` =
no-op-with-a-comment: BOT has no cache-control verb in this command set, the same
"durability is the underlying device's" honesty as fs-eofs, and the flasher's
read-back-verify is the real durability check). Enumeration: `attach` the port,
read descriptors via `control-in`, find the mass-storage interface (class 08,
subclass 06 SCSI-transparent, protocol 0x50 BOT — typed `unsupported` otherwise),
SET_CONFIGURATION, open the bulk pair, TEST UNIT READY until ready (bounded;
REQUEST SENSE eats the post-reset UNIT ATTENTION), READ CAPACITY. D46 driver
discipline verbatim: take/put slot, bring-up claim, bounded polls, typed errors
never traps. **Warm-state doctrine carries over**: vendor U-Boot read the stick this
boot (`usb start` + fatload); usb.msd always does its own port reset + enumeration,
exactly the CONFIGFLAG-clear-first / SD PWREN-power-cycle posture.

### 1.3 Reality checks, stated honestly

- **Speed**: the stick enumerates FULL SPEED on OHCI (HS-capable sticks fall back —
  there is no EHCI driver, and EHCI's CONFIGFLAG=0 routes the port to the companion
  OHCI). FS bulk line ceiling = 19 × 64 B per 1 ms frame ≈ **1.2 MB/s**; our polled
  QD1 driver will see ~0.5–1.0 MiB/s. A 52 MiB image (current full size,
  usb-boot-demo-plan) writes in ~60–100 s, plus the same again for read-back-verify:
  **the full network rewrite is ~2–3 minutes**. Fine for the loop (vs sudo-dd +
  stick-shuffling); not a file-transfer product. **EHCI is the explicit non-goal**:
  it would give ~40 MB/s but no EHCI driver exists and writing one (async schedule,
  QH/qTD, port routing) is a full lane of its own — recorded as the future rung if
  flash time ever matters.
- **Port topology — the one bench-rule change this lane forces**: the boot stick
  currently lives in a USB 3.0 port (behind the onboard hub on **xhci1** — usb-boot
  plan stick rules). OHCI **cannot reach that port at all**. For the board to rewrite
  the stick, the stick must sit in one of the two USB2-A ports (the direct OHCI
  ports). New bench standard for this lane: **keyboard in USB2-A #1, stick in
  USB2-A #2, mouse omitted** (the usb-boot plan already names exactly this as its
  fallback layout). Two consequences to verify at R0: (a) U-Boot's `usb_boot` finds
  and boots the stick from a USB2-A port (it scans all controllers; high confidence,
  zero proof yet), and (b) keyboard and stick coexist across the two OHCI
  controllers (they are separate controllers — usb_host0 vs usb_host1 — so the
  claim story is two regions, no sharing).

## 2. Exposing it as a disk — the layering

Consistent with the disk.* family and sdcard-plan §B.3, unchanged in spirit:

- **usb.msd is a disk provider** (like disk.virtio, like the planned disk.sdmmc).
  The eo9:disk WIT needs **zero changes**.
- **Partition awareness stays in `disk.part`** — the sdcard plan's pure attenuation
  middleware (MBR parse at bind, windowed offset translation, typed out-of-range).
  It is not yet in the tree (guest/stubs has no disk-part): **this lane co-owns its
  delivery with the sdcard lane** — whichever lane lands first builds it; it is fully
  usermode-testable over disk.mem with an MBR fixture, no silicon dependency, no
  duplication risk if both lanes read this paragraph.
- The flasher chain is therefore:
  `usb.ohci --region usb-host1-ohci $ usb.msd $ disk.part --partition 1 $ …` — the FAT
  partition as a windowed eo9:disk, p-boot-layout knowledge nowhere in the driver.

## 3. The FAT file rewrite — smallest correct v1

### 3.1 Decision: fixed-size slot + in-place cluster overwrite, FAT logic as a crate

The stick (usb-boot-demo-plan): MBR, one FAT32 partition, 8.3 names — `EO9.IMG`,
`BOOTARGS.TXT`, `BOOT.SCR` (mkimage script with the image CRC **baked in**). Image
sizes vary per build (23–52 MiB) — naive in-place overwrite breaks the moment the
image grows. Rather than implement FAT cluster allocation, **change the stick
build**: xtask pads `EO9.IMG` with zeros to a **fixed slot size** (default 56 MiB —
covers the 52 MiB full build with headroom, under the 62 MiB structural cap;
configurable) and renders `BOOT.SCR` with **fixed-width** hex CRC and size fields, so
both files are byte-count-invariant across builds. Then every rewrite is:

1. Parse the FAT32 boot sector + root directory, locate `EO9.IMG` and `BOOT.SCR`,
   walk their cluster chains once (read-only against the FAT).
2. Overwrite EO9.IMG's clusters in chain order with the padded image. **No FAT
   writes, no directory-entry writes, no allocation, no free-space accounting.**
3. Read every cluster back; CRC the padded length; compare against the wire CRC.
4. Overwrite BOOT.SCR's cluster(s) with the new script (new baked CRC) — the
   **commit point** (§3.2).

This lives in a pure no_std crate (working name **`crates/eo9-fatwalk`**: boot-sector
parse, 8.3 root-directory lookup, cluster-chain walk, cluster-window read/write over
a `disk`-shaped trait) — host-tested against an mtools/fixture-built FAT image, the
load-as-test-input discipline. It is **used by stickflash, not exposed as a
component**. A general `fat.put`/`fs.fat` middleware is the recorded follow-up (it
becomes worth it when BOOTARGS.TXT/BOOT.SCR need ad-hoc replaceability or a read
surface — BOOTARGS edits today stay a Mac-side FAT-mount one-liner, which is fine:
they change rarely and carry no CRC coupling). Folding into eofs is rejected: eofs
is its own filesystem and its foreign-image refusal exists precisely to *not*
understand FAT.

v1 scope cuts (recorded): EO9.IMG-only rewrite; FAT32 + 8.3 + first-FAT-only
(mirror FATs are read-skipped, never written — we never change the FAT);
no long-file-name parsing (the xtask-built stick guarantees 8.3); no fragmented-chain
optimization (the chain walk handles fragmentation correctly anyway — order comes
from the chain, not from contiguity assumptions).

### 3.2 Corruption-safety story, stated plainly

The boot-time CRC gate that already ships (boot.scr: crc32-save + `cmp.l` + `go`,
A0-proven construction) **doubles as the torn-write gate**:

- Power cut / reset mid-step-2 (image clusters): old BOOT.SCR's baked CRC no longer
  matches the half-new image → CRC gate fails → chain falls through to the prompt,
  benignly, exactly as characterized. The board is NOT bricked; recovery = serial
  boot, re-run stickflash (or pull the stick to get today's bench verbatim).
  **Accepted**: a torn write costs a serial round, never a Mac dd.
- Power cut between steps 3 and 4: image is complete and verified but BOOT.SCR still
  carries the old CRC → same benign fail-through. Same recovery.
- Step 4 itself is one or two clusters (the script is < 4 KiB) — near-atomic; a torn
  BOOT.SCR fails U-Boot's script verification / produces a CRC mismatch → same
  fail-through. There is no ordering where the stick silently boots a wrong image:
  **success requires the freshly-baked CRC to match the freshly-written bytes.**
- Declared success on the wire (the `K` verdict) is sent only after step 4 completes
  and a final read-back of BOOT.SCR matches — write-then-verify before declaring
  success, both files.

## 4. The network service — `stickflash`

### 4.1 Sibling vs oskexec mode flag: **sibling, shared protocol crate**

Weighed honestly:

- A `--target stick|kexec` mode flag on oskexec means one world importing **both**
  eo9:kexec and eo9:disk — wider authority than either job needs, against the
  capability posture, and it couples the kexec image ceiling/staging semantics to a
  path that has neither. Rejected.
- A sibling **`guest/examples/stickflash`** reuses the wire protocol **verbatim**
  (EO9L magic, ≥16-byte preshared secret with the full-length compare, 24-byte
  header, streamed payload with `k` acks per 64 KiB, trailing CRC, `K`/`G`
  verdict-then-go-ahead — send_image.py needs only a new port/flag, the host-side
  stall alarm carries over). The receive/auth/ack loop is factored out of oskexec
  into a shared module (working name **`crates/eo9-flashwire`** or a guest shared
  module, the l4check DNS-encoder precedent) — oskexec shrinks, stickflash stays
  ~sink-only. Default port 9910 (9909 is oskexec).

Differences from oskexec, by design: the sink is fatwalk-over-disk instead of
`kexec.stage`; the ceiling is the stick slot size (refuse images > slot, padded
length is the CRC'd length); **not one-shot** in the same way — one flash per
session, but the service is restart-supervised (below) so the listener is always
there, which is the point of the loop; after `K`/`G` the program **exits with a
success outcome** and prints "reset to boot it" — v1 does NOT auto-reset (the
operator chooses when the board dies; exit-is-reset semantics and a
flash-then-kexec-same-bytes chaining are recorded follow-ups, the latter giving
instant-boot + persistent-stick in one shot).

### 4.2 Supervision and grants (the svc-grants machinery, area/29, merged)

Init config line (the station grammar — `$` chains, `--flag value`):

```
flash = net.rtl8125 --advertise-max 1000 $ (net.l4.over-l2 --address dhcp)
        $ usb.ohci --region usb-host1-ohci $ usb.msd $ disk.part --partition 1
        $ stickflash --secret-… restart restart.always
```

(Exact chain syntax to be confirmed against init's grammar at implementation —
stickflash imports l4 + disk + text; the algebra passes non-matching imports
through a linear chain today for curl's l4+text, and this chain is the same shape
one capability wider. If pass-through has a gap here, that is a finding for the
ledger, and the fallback is a two-line composition saved as one artifact.)

Grant set, per the svc.rs posture (detached services get the **boot-granted operator
roots** — pci/platform/gfx/kexec/console-sink — plus what the detacher composed in;
net/fs/exec/svc never ambient): the board boot needs **`platform`** (or least-authority
`platform=usb-host1-ohci`) for the OHCI region and **`pci`** for net.rtl8125; net
arrives **via composition** as shown (or via the net-grant gate if/when that merges —
not a dependency of this lane). The QEMU gate variant chains `usb.ohci-pci` and
`net.virtio` under the `pci` grant alone. stickflash itself never holds kexec, fs,
or exec — its blast radius is exactly the stick partition window.

## 5. Rounds

### 5.1 QEMU legs — feasibility VERIFIED, gate design

**Verdict: feasible, verified live this session** on the repo's QEMU 11.0.0:
`-device pci-ohci,id=eo9ohci -drive file=…,if=none,format=raw,id=stick -device
usb-storage,bus=eo9ohci.0,drive=stick` → `info usb` shows "QEMU USB MSD,
Speed 12 Mb/s" — full speed on the OHCI bus, the same `-M virt` machine config as
check-usb, mirroring the usb-kbd gate pattern exactly.

- **check-msd**: xtask builds a FAT32 fixture image (mtools or the fatwalk crate's
  own test builder — same fixture both places) with a dummy EO9.IMG + BOOT.SCR;
  boots with pci-ohci + usb-storage; drives at the eosh prompt:
  `usb.ohci-pci $ usb.msd $ disk.part --partition 1 $ readwrite`-style scratch
  write+read-back (the existing readwrite example or a thin mdcheck), then
  `usb.ohci $ usb.msd` for the typed no-controller refusal — the check-usb
  three-composition pattern verbatim.
- **check-stickflash**: the check-kexec shape (slirp + hostfwd) — boot with the
  fixture stick, run the flash chain, drive send_image's tcp path from the host
  side, then **re-read the image clusters via the same msd chain and compare CRC
  against what was sent**, and assert the rewritten BOOT.SCR carries the new baked
  CRC. This pins the whole loop minus silicon and minus an actual reboot-from-stick
  (QEMU -M virt cannot distro-boot our FAT stick; the reboot leg is bench-only).

Host tests under everything: eo9-msd CBW/CSW/SCSI byte pins, fatwalk against fixture
images (including a deliberately fragmented chain), the OHCI mock's bulk list, the
flashwire protocol module against scripted sessions (both oskexec and stickflash
consume it — one protocol, two sinks, the tests stay shared).

### 5.2 Bench rounds (the bench protocol + watchdog doctrine as standing)

- **R0 (recon, minutes, shares a session)**: stick in USB2-A #2 → vendor prompt
  `usb start; usb tree; usb storage` (stick visible on a USB2 root port?); then the
  one open topology question: power-on with stick in USB2-A #2 → does usb_boot boot
  it (the §1.3 bench-rule change gate). Keyboard untouched in #1.
- **R1 — enumerate**: serial-boot a build with the msd chain; usb.msd prints
  VID:PID, INQUIRY strings, READ CAPACITY (must match the physical stick).
  Failure localization: no device → port/VBUS (keyboard rounds burned this down);
  enumerates but bulk times out → bulk-list/DMA-coherence (the provider sweep
  brackets are structural, same as interrupt EDs — but bulk is the first OUT-heavy
  DMA path on silicon, watch it).
- **R2 — read**: sector 0 (MBR matches the stick we built), then a **full EO9.IMG
  read via the chain, CRC against the xtask-baked value** — end-to-end read
  integrity against ground truth the bench already owns (the sdcard M2 trick).
  Timed: this measures the real FS throughput and calibrates the §1.3 estimate.
- **R3 — write**: scratch-area write+read-back on the FAT partition's free space
  first, then a full stickflash run **over serial-started service + Mac
  `send_image --tcp-stick`** — the first network-driven rewrite. Stick into the Mac
  once afterward for an out-of-band fsck/byte-compare (one-time confidence, not part
  of the loop).
- **R4 — the loop demo**: rewritten stick + reset → USB boot of the new image,
  zero hands (needs the area/43 x0/staged-bootargs leg merged and the R0 USB2-port
  boot verdict). Then the full ouroboros: boot from stick → stickflash a *different*
  build onto it → reset → the different build's banner. ×2 for confidence.

## 6. Lanes, sizing, dependency order

| Lane | Content | Size | Depends on |
|---|---|---|---|
| **L1 bulk** | eo9:usb WIT bulk funcs + OHCI bulk list + mock + deny/refusal tests | ~1–2 dev days | — |
| **L2 msd** | crates/eo9-msd + usb.msd stub + manual + msd-enumerate QEMU smoke | ~2–3 dev days | L1 |
| **L3 disk.part** | the sdcard-plan M0 component + usermode tests + manual (shared with the sdcard lane — coordinate, build once) | ~1 dev day | — |
| **L4 fat+wire** | eo9-fatwalk crate + fixture builder; flashwire factor-out of oskexec; xtask fixed-slot padding + fixed-width BOOT.SCR; send_image --tcp-stick | ~2 dev days | — (pure host work) |
| **L5 stickflash** | the example + world + manual + init config line | ~1 dev day | L2, L3, L4 |
| **L6 gates** | check-msd + check-stickflash | ~1 dev day | L2, L3 (check-msd); L5 (check-stickflash) |
| **L7 bench** | R0–R4 | 3–4 bench rounds | R0 needs nothing but the bench; R1+ need L2/L3; R3+ need L5/L6 green; R4 needs area/43 |

**Parallelism**: L1, L3, L4 are mutually independent and can run as three concurrent
lanes from minute zero (L4 touches oskexec — keep that refactor its own reviewable
commit). L2 follows L1; L5 is the join; R0 can run tonight independent of all code.
Critical path: L1 → L2 → L5 → L6 → R1–R4 ≈ **5–7 dev days + 3–4 bench rounds**.

## 7. Conventions cross-check

- **Naming**: `usb.msd` follows usb.ohci/usb.kbd (bus.class); it joins the disk
  provider family by *export*, not by name — consistent with usb.kbd exporting
  console traffic, and disk.part/disk.sdmmc keep the disk.* middleware/provider
  naming. Stub dirs `guest/stubs/usb-msd`, `guest/stubs/disk-part`; example
  `guest/examples/stickflash`; crates `eo9-msd`, `eo9-fatwalk`, `eo9-flashwire`.
- **Manuals**: usb.msd, disk.part, and stickflash each ship an `eo9-manual` section
  (component-manuals.md v1; the no-component-ships-undescribed discipline).
- **Workaround ledger**: the implementation lanes report every workaround per the
  standing rule — pre-registered candidates: any bulk NAK/timeout tuning constant,
  any stick-specific quirk (CSW residue lies, UNIT ATTENTION loops), any init-chain
  pass-through gap (§4.2), and any QEMU usb-storage behavior divergence from silicon.
- **Timer-flush rule**: the bulk completion path is done-queue-driven; if any
  backstop poll ever rescues a completion the done queue should have delivered,
  that is a liveness_finding, wired like the M3 stranded-runnable detector.

## 8. Risks → discriminating tests

| # | Risk | Discriminating test |
|---|---|---|
| 1 | U-Boot won't boot from a USB2-A port (bench rule change) | R0: power-on, stick in USB2-A #2 — settles it in one session |
| 2 | Bulk OUT DMA coherence on silicon (first OUT-heavy path) | R3 scratch write+read-back; provider sweep brackets structural; debug knob = whole-window sweep per poll |
| 3 | Stick quirks (CSW residue, UNIT ATTENTION storms, slow ready) | eo9-msd recovery ladder host-pinned; R1 against the actual bench stick; second stick model on the bench as cross-check |
| 4 | FS throughput materially below 1 MB/s (polled QD1) | R2 timed full-image read; if the loop exceeds ~5 min, TD chaining (recorded follow-up) is the first lever, EHCI the last |
| 5 | Init chain pass-through gap for l4+disk+text (§4.2) | L5 compose test in QEMU before any bench time; fallback = saved composition artifact |
| 6 | fatwalk meets a stick formatted differently than xtask built it | Non-risk by construction: only xtask-built sticks are supported; foreign layout → typed refusal, never a write |
| 7 | Keyboard + stick claim collision | Two separate OHCI controllers/regions; check-usb's restricted-grant pattern pins per-region claims; R1 runs with the keyboard plugged |
| 8 | QEMU usb-storage models BOT more politely than silicon | Known class of gap (sdcard §B.6 posture): host-pinned protocol + R1/R3 as the silicon truth; divergences → ledger |

## 9. Workarounds / assumptions (standing rule: every one reported)

1. **Fixed-slot padding trades flash time for FAT simplicity** — ~4 MiB of zero pad
   per flash (~5 s at FS rates) buys the elimination of all FAT write logic. The
   slot size is an xtask constant; growing it requires one Mac-side stick rebuild.
2. **`usb.msd::flush` is a documented no-op** (BOT in this command set has no cache
   verb; sticks generally don't cache like SD FTLs) — the flasher's read-back-verify
   is the durability check that matters; same honesty class as the SD flush note.
3. **U-Boot USB2-A-port boot is assumed, not proven** (usb_boot scans all
   controllers; the usb-boot plan's own fallback names this layout) — R0 is the gate
   before any bench investment past R0.
4. **The QEMU smoke test ran against QEMU 11.0.0 on this machine** (`info usb`,
   full-speed attach confirmed); the gate hardcodes nothing version-specific beyond
   what check-usb already assumes.
5. **disk.part is co-owned with the sdcard lane** — whichever lands first builds it;
   double-build is the failure mode this sentence exists to prevent.
6. **Planning session was read-only** except this document; no serial port touched;
   the one external action was the local QEMU smoke test in /tmp.

## R0 status: PROVEN (bench, 2026-06-09 night — before this plan merged)

The USB-boot round A1 ran with the stick in USB2-A #2 (mouse omitted, keyboard in
#1 — exactly this plan's required layout): the vendor distro chain scanned usb,
found the stick, sourced /boot.scr, and loaded BOOTARGS.TXT + EO9.IMG at 25.9 MiB/s.
(The kernel then hit the junk-x0 hang — area/43's lane — but that is past every
U-Boot leg R0 gates on.) All further lanes are unblocked; workaround 3 is retired.
