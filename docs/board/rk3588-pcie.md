# RK3588 PCIe: the DesignWare config-access shim (design note)

Status: **implemented** (area/12-pcie-eth) — the `ConfigAccess` shim + DW implementation
live in `kernel/eo9-kernel/src/pci.rs`, the board constants + full bring-up (clocks,
resets, combphy, PERST, LTSSM) in `kernel/eo9-kernel/src/arch/aarch64/rk3588_pcie.rs`
(every constant cited to its Linux v6.12 source there). The QEMU half (ECAM behind the
same trait) is verified bit-for-bit; the DW half awaits its first board run. Two design
notes below were superseded by implementation findings, marked **[superseded]**.

**Board-derived facts replacing the old [verify-on-board] marks** (the captured vendor
control FDT `.claude/board-bringup/vendor-control-fdt.dtb` + mainline
`rk3588-orangepi-5-plus.dts` / `rk3588-base.dtsi`):

* The two onboard RTL8125 NICs hang off **`pcie2x1l1`** (right port, combphy2, PERST
  GPIO3_B3) and **`pcie2x1l2`** (left port, combphy0, PERST GPIO4_A2) — *not* the
  controllers this note originally guessed. `pcie2x1l0` is the M.2 E-key.
* **Vendor U-Boot pre-initializes none of them.** Its control FDT contains exactly one
  PCIe node (`pcie@fe150000` = pcie3x4, the M.2 M-key); "PCIe-0 Link Fail" in the boot
  log was the empty M.2 socket. The day-one "rely on U-Boot bring-up" shortcut from the
  table below is therefore **void**: the kernel owns the full PHY/clock/reset/LTSSM
  sequence from the first run.
* DBI blocks live at `0x0a_40c0_0000` / `0x0a_4100_0000` — **above 4 GiB**, which forced
  the board MMU profile to T0SZ=28 (36-bit VA) with a Device block at level-1 entry 41
  (mmu.rs). The config apertures (`0xf300_0000`/`0xf400_0000`), APB blocks
  (`0xfe18_0000`/`0xfe19_0000`) and 32-bit mem windows stay inside the existing
  fourth-gigabyte device map.
* Both NICs share one GPIO-gated 3.3 V rail (`vcc3v3_pcie_eth`, GPIO3_B4 **active low**,
  50 ms startup) — a bring-up step no PCIe doc mentions.

## The problem

Eo9's `kernel/src/pci.rs` assumes **ECAM**: configuration space is one flat memory window
where `bus:dev:fn:offset` linearly maps to an address (`(b << 20) | (d << 15) | (f << 12) |
offset`). QEMU's `virt` GPEX host bridge provides exactly that, so today config access is a
single `read_volatile` against `0x3f00_0000 + ecam_offset`.

The RK3588's five PCIe controllers are Synopsys DesignWare cores, and DW does **not**
expose ECAM:

* the **root port's own** config header lives in the controller's DBI register space (a
  separate MMIO block per controller);
* **downstream** devices are reached by programming an **outbound iATU window**: you pick a
  small MMIO aperture inside the controller's address space, configure ATU region N with
  type `CFG0` (for the secondary bus — the device right below the root port) or `CFG1`
  (buses further down), set the target to `(bus << 24) | (dev << 19) | (fn << 16)`, and
  then ordinary loads/stores through the aperture become config TLPs.

So one `pci.rs` "read config dword" becomes: *(maybe) reprogram the ATU target → read
through the aperture*. Two MMIO writes + a read instead of one read — entirely manageable,
but it has to go behind an abstraction.

## The shim: a `ConfigAccess` trait

**[superseded]** The shipped trait is *address-mapping* shaped (`map(bus, dev, fn,
offset) -> Option<usize>`, Linux's `pci_ops.map_bus` pattern) rather than the
read32/write32 pair sketched here: deriving sub-word writes from a 32-bit RMW would
write Status (offset 0x06, RW1C bits) back when a driver writes Command (0x04, Word) —
a real bug the sketch contained. With `map`, every access width hits the bus exactly as
the pre-shim ECAM code did. The sketch, for the record:

```rust
/// How configuration space is reached on this machine. All methods take the
/// canonical (segment, bus, device, function) + register offset.
trait ConfigAccess {
    fn read32(&self, bdf: Bdf, offset: u16) -> u32;
    fn write32(&self, bdf: Bdf, offset: u16, value: u32);
    // read8/16 + write8/16 derived from the 32-bit ops (RMW for sub-word writes,
    // exactly as pci.rs already does internally for ECAM).   // <- wrong, see above
}

struct Ecam { base: usize }                  // QEMU virt: today's behavior, verbatim.

struct DwPcie {
    dbi: usize,        // root port config + iATU registers      [verify-on-board: DTB "apb"/"dbi" regs]
    cfg_aperture: usize, // small outbound window for config TLPs [verify-on-board: DTB ranges]
    cfg_size: usize,
    last_target: Cell<u32>, // cache: skip the ATU reprogram when bus/dev/fn repeats
}
```

* `DwPcie::read32(bdf, off)`:
  * `bdf.bus == root.secondary` and `bdf.device == 0` → DBI direct (`dbi + off`); the DW
    core only implements device 0 on the root bus — **other device numbers on bus 0/1 must
    return `0xFFFF_FFFF`** instead of generating a TLP (mainline does the same; otherwise
    enumeration sees ghost devices).
  * deeper → if `target != last_target`, program ATU region 0: `IATU_LWR_TARGET_ADDR =
    target`, type CFG0/CFG1, `IATU_REGION_CTRL_2.EN = 1`, **read back CTRL_2 until EN
    sticks** (the DW manual's required settle check — the Linux driver polls up to 10 µs);
    then `read_volatile(cfg_aperture + (off & 0xFFF))`.
* iATU register block: at `dbi + 0x300000` on DW ≥ 4.80 ("unrolled" iATU, which the RK3588
  uses) — region stride `0x200`, outbound region N at `0x300000 + N*0x200`.
  **[verify-on-board]** by reading `IATU_VIEWPORT` behavior (unrolled cores have no
  viewport register).

`pci.rs` keeps its enumeration/BAR/capability logic untouched; it just goes generic over
`ConfigAccess`. The QEMU build instantiates `Ecam` (zero behavior change — this refactor
can land and be fully verified *before* the board, and is the one piece of this note worth
implementing blind).

## What else differs from the GPEX world

| Concern | QEMU virt (today) | RK3588 (arrival) |
|---|---|---|
| Controllers | one host bridge, one segment | five independent DW instances (M.2 x4 = `pcie3x4`; three x1 for the two RTL8125 NICs + E-key) — model as separate segments, enumerate independently |
| Link bring-up | always up | per-controller PHY + LTSSM start, link-up poll (mainline `rockchip_pcie_start_link`); U-Boot may have done it already if booted with PCIe enabled — day-one shortcut: rely on U-Boot bring-up first, own the LTSSM later |
| INTx | GPEX SPIs 35-38, one per pin | one **combined "legacy" SPI per controller** [verify-on-board: DTB `interrupts` of each `pcie@…` node]; demux = read the controller's `PCIE_CLIENT_INTR_STATUS_LEGACY` register, then the existing mask/record flow per virtual line |
| MSI | not used (INTx only) | available via the DW core's internal MSI block (doorbell write to a controller-owned address, one SPI upstream) — **defer**; INTx first, exactly like the QEMU bring-up did |
| ECAM `highmem=off` pin | keeps ECAM < 1 GiB | n/a — but the DBI/aperture addresses live at `0xf500_0000`–`0xfe9f_ffff`, so the kernel's identity-mapped device window must cover that range (see the board doc's MMU item) |

## Suggested order on the board **[superseded by the implemented bring-up]**

What actually shipped (rk3588_pcie.rs `init`): PD_PCIE check/poke → PPLL diagnostic →
NIC rail on (50 ms) → shared clock roots → per controller: PERST low, controller resets
asserted, combphy (resets asserted → clocks + 100 MHz refclk div/mux → GRF PCIe mode →
PHY tuning regs → resets deasserted), controller resets deasserted + clocks, APB RC
mode, DBI sanity + root-port setup (x1 lanes, buses 0/1/15, bridge class, mem window +
identity MEM iATU region 1), LTSSM enable, 100 ms, PERST high, ≤1.1 s bounded link poll
→ `link UP genX x1` / `link DOWN (ltssm …, debug …)`, controller marked FULL/ROOT_ONLY
for the shim. Every wait bounded; every step prints before touching a new block. The
original sketch:

1. Refactor `pci.rs` over `ConfigAccess` under QEMU (no behavior change, full battery green) — *can be done before arrival*.
2. Day one: read the DTB's `pcie@…` nodes for dbi/aperture/ranges/interrupts; hardcode the
   M.2 controller's values into a `boards/rk3588.rs` constants module (same pattern as the
   QEMU constants today).
3. Bring up `DwPcie` against an NVMe stick in the M.2 slot (lspci through the shim is the
   acceptance: vendor/device IDs of the stick).
4. INTx demux; then `disk.virtio`… does **not** apply (NVMe ≠ virtio) — the first real
   storage driver on the board is either an NVMe wasm driver (new work, post-arrival) or
   the RTL8125 path for net. Set expectations accordingly: **day-one PCIe success =
   enumeration + config access, not a filesystem**.

What is testable only on the board: ATU settle timing, the combined-INTx demux register,
link training, and every absolute address. Everything in the trait + ECAM refactor is
testable today.
