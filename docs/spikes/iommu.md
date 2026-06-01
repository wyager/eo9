# Spike: what does IOMMU support require for Eo9's PCI/driver stack?

*Branch `spike/12-iommu`, 2026-06-01. Investigation only — nothing here is wired into default
builds. The experimental `iommu` xtask argument exists solely so these experiments are
reproducible: `cargo xtask qemu aarch64 <args> iommu` boots the same machine with
`-M …,iommu=smmuv3`.*

## Why

User study 09 (driver developer) flagged that Eo9's DMA story is one-sided: `alloc-dma`
constrains what addresses a *driver* can request, but nothing constrains what addresses the
*device* actually emits on the bus. A buggy or malicious device — or a driver that encodes a
fabricated address into otherwise-legitimate-looking descriptors — can read or write any
physical memory the bus can reach. The kernel-side teardown quiesce (plan/12 D62) narrows the
window but cannot close it: only an IOMMU makes out-of-bounds DMA *structurally impossible*.

## The five questions, answered with evidence

### 1. Does the current kernel boot under `-M virt,iommu=smmuv3`? — **Yes, completely.**

`cargo xtask qemu aarch64 demo iommu` (QEMU 11.0.0) runs the entire wasm demo — preemption,
seed component, hello, async sleep, sync configure with the exact expected entropy values
(`0x505f147c387507b6, 0xe2e264775fe9be54`), on-target Cranelift codegen — and powers off
cleanly via PSCI. Identical output to the non-IOMMU boot.

What changes in the device tree (full dumps: `qemu-system-aarch64 -M virt,…,dumpdtb=…`):

```
smmuv3@9050000 {
    compatible = "arm,smmu-v3";
    reg = <0x00 0x9050000 0x00 0x20000>;          // 128 KiB of MMIO at 0x0905_0000
    interrupts = <… 0x4a … 0x4b … 0x4c … 0x4d …>; // 4 GIC SPIs: eventq, priq, cmdq-sync, gerror
    #iommu-cells = <0x01>;
    dma-coherent;
};

pcie@10000000 {
    iommu-map = <0x00 0x8004 0x00 0x10000>;       // every PCI RID maps 1:1 to an SMMU StreamID
    …
};
```

Two facts that matter for Eo9:

- The SMMU's MMIO window (`0x0905_0000..0x0907_0000`) lies inside the device gigabyte the
  kernel already identity-maps RW-NX, so a future SMMU driver needs **no MMU changes** to
  program it.
- The `iommu-map` is the identity mapping: PCI requester ID = SMMU StreamID. With at most a
  handful of functions on the virt machine, a **linear stream table** (the simple format)
  suffices.

The kernel itself parses neither node today (it reads only `/chosen/bootargs` and hardcodes the
ECAM base), which is exactly why the boot is unaffected.

### 2. Does DMA still work with the SMMU present but unconfigured? — **Yes: bypass.**

The strongest possible probe is the kernel's own storage path, because it does real DMA in both
directions at boot with no interaction needed: the in-kernel virtio-blk driver reads virtqueue
descriptors and data from RAM (device→RAM reads of the ring, RAM→device for writes) while
formatting and mounting the store disk.

`cargo xtask qemu aarch64 storedisk iommu`:

```
storedisk: virtio-blk 131072 sectors (64 MiB) claimed for the kernel store
storedisk: blank disk formatted with eofs (block 4096, lz4 on)
storedisk: eofs mounted (txg 1), 0 cached compile artifact(s), 0 saved program(s)
```

Identical to the non-IOMMU run. PCI enumeration (`pci program=lspci iommu`) is also identical
(`success(devices(3))`) — expected, since CPU→device accesses (ECAM, BARs) never traverse the
SMMU; it sits only on the device→memory path.

**Why this is the case, and why it matters:** the SMMUv3 architecture routes all traffic
through the *global bypass attribute* (`SMMU_GBPA`) while the SMMU is disabled
(`SMMU_CR0.SMMUEN = 0`). QEMU resets `GBPA.ABORT = 0`, i.e. *bypass*: transactions pass through
untranslated. The empirical consequence is the property we want for an incremental rollout:

> **Adding the SMMU to the machine model changes nothing until the kernel chooses to program
> it.** There is no flag day. The QEMU flag, the DTB parsing, the SMMU driver, and the
> per-device translation can land as four separate verified steps.

(The flip side: bypass-by-default means *presence* of an IOMMU provides zero protection. Real
hardware may even reset `GBPA.ABORT = 1` — abort everything until programmed — which is why
step 1 of any real-board bring-up must be "decide what GBPA should be".)

### 3. What would a minimal Eo9 SMMUv3 driver need? — **~800–1,200 lines, no new concepts.**

Stage-2-only translation (the hypervisor-style mode: device addresses are "guest physical",
translated once to real physical) is all Eo9 needs — there is no second stage to nest, and
stage-2 table entries are nearly identical to the CPU page-table entries the kernel already
builds in `arch/aarch64/mmu.rs`.

| Piece | What it is | Size / difficulty |
| --- | --- | --- |
| Register bring-up | Read `IDR0..5` capabilities; program `STRTAB_BASE`, `CMDQ_BASE`, `EVENTQ_BASE`; set `GBPA.ABORT = 1`; enable `CR0.{CMDQEN,EVENTQEN,SMMUEN}` | ~150 lines, mechanical |
| Linear stream table | One 64-byte STE per StreamID (= PCI RID); STE fields: valid, config = stage-2 translate, VTTBR (the device's stage-2 table root), VTCR attributes | ~100–150 lines of careful bit-packing (the STE layout is the fiddliest part) |
| Command queue | Circular buffer of 16-byte commands (`CFGI_STE`, `TLBI_S2_IPA`, `TLBI_NSNH_ALL`, `SYNC`), producer/consumer indices with wrap bits, polled completion | ~150–200 lines |
| Event queue | Circular buffer the SMMU writes fault records into; drain it, attribute the StreamID to the owning task, surface a typed error | ~100 lines (polled; the four SMMU interrupts can be ignored in v1) |
| Stage-2 page tables | Per claimed device: a 4 KiB-granule table mapping *exactly* the task's `alloc-dma` buffers, identity (IPA = PA); everything else unmapped | ~150–250 lines — an adaptation of the existing `mmu.rs` walker (S2 descriptors use S2AP/MemAttr instead of AP/AttrIndx; no ASID/nG) |
| PCI-provider integration | `open` → allocate stage-2 root + write STE + `CFGI_STE`+`SYNC`; `alloc-dma` → map pages + `TLBI`+`SYNC`; `close-buffer`/quiesce → unmap + `TLBI`; teardown → STE → abort | ~150 lines in `wasm/pci_provider.rs`, slotting into the same lifecycle hooks the D62 quiesce work added |

Hard parts: none conceptually new. The STE/command encodings are tedious; everything else
(page-table building, polled rings, per-task device lifecycle) already exists in the kernel in
some form. The work is a sibling of `pci.rs` (450 lines) + `virtio_blk.rs` (520 lines) in both
size and character. Estimate: **one to two focused sessions** with QEMU verification at each
step.

### 4. The payoff — out-of-bounds DMA becomes a fault, not a corruption. **Confirmed by architecture.**

With a device's STE set to stage-2 translate and its stage-2 table mapping only that task's
`alloc-dma` buffers:

- Every transaction the device emits is translated through the stage-2 table keyed by its
  StreamID (which the device cannot forge — it is wired into the bus topology, the
  `iommu-map`).
- An address outside the mapped buffers misses the stage-2 table → the SMMU terminates the
  transaction (the device sees an error completion, memory is never touched) and writes an
  `F_TRANSLATION` record carrying the StreamID and faulting address into the event queue.
- The kernel drains the event queue and can attribute the fault to the exact device and owning
  task — surfacing it as a typed driver error (or killing the task), instead of the silent
  memory corruption that study 09 called disqualifying.

Two further wins beyond the study's scenario:

- With `GBPA.ABORT = 1`, a device that was *never claimed* cannot DMA at all — protection
  exists even before any driver runs.
- The teardown race the D62 quiesce fix narrows is closed completely: a stale in-flight DMA
  after table teardown hits an empty stage-2 table and faults harmlessly.

### 5. SMMUv3 vs virtio-iommu — **SMMUv3 is the only path that exists on real hardware.**

| | SMMUv3 (`-M virt,iommu=smmuv3`) | virtio-iommu (`-device virtio-iommu-pci`) |
| --- | --- | --- |
| Programming model | MMIO registers + in-memory stream table + command/event queues | Virtqueue requests (`ATTACH`, `MAP`, `UNMAP`) — Eo9 already has virtqueue code |
| Estimated effort | ~800–1,200 lines | ~400–500 lines |
| Exists on real aarch64 boards | **Yes** — SMMUv3 (often as ARM MMU-600/700) is the system IOMMU on server-class and recent SoCs (Graviton, Ampere Altra, RK3588-class parts); SMMUv2 on older ones. (Hobbyist boards like Raspberry Pi have *no* system IOMMU at all.) | **No** — paravirtual, exists only under a hypervisor |
| Verdict | The real answer; QEMU work transfers 1:1 to real-board bring-up | A QEMU-only dead end: we would still need the SMMUv3 driver for hardware, so the cheaper option buys nothing permanent |

## Recommendation

1. **Build the SMMUv3 driver, stage-2-only, polled, default-deny (`GBPA.ABORT = 1`)** — but
   schedule it as part of **real-board prep**, not before. It is the natural next rung after
   the current driver substrate, the QEMU work transfers entirely, and nothing else on the
   roadmap is blocked by it. (The interim honest position, already recorded by study 09's
   triage: drivers are containment-limited by the absence of an IOMMU; the boot grants — `pci`
   token + device flags — are what bound the exposure today.)
2. **Skip virtio-iommu entirely.**
3. **Incremental plan** (each step independently verifiable under QEMU):
   1. Make the `iommu` machine flag a supported xtask argument (it is currently experimental in
      this spike) and have the kernel *parse* the SMMU + `iommu-map` DTB nodes (depends on the
      "read ECAM from DTB" follow-up already recorded in plan/12).
   2. SMMU bring-up with `GBPA.ABORT = 1` and **bypass STEs** for every present device —
      behavior identical to today, but now opt-in per stream rather than global.
   3. Stage-2 tables per claimed device, mapping exactly its `alloc-dma` buffers; flip claimed
      devices' STEs from bypass to translate. The `pci disk` / `pci net` demos must run
      unchanged.
   4. The negative test that proves the payoff: a deliberately-corrupted descriptor (a
      test-only driver that requests DMA to an unmapped address) → the fault record appears,
      the task gets a typed error, kernel memory is untouched.
4. **`wit/pci` needs no changes.** The API already traffics in opaque DMA addresses returned by
   `alloc-dma`; whether the kernel hands out bypass physical addresses (today) or
   stage-2-mapped IPAs (with the SMMU) is invisible to drivers. The containment language in the
   DMA-contract doc comments (plan/02 D22) gets *stronger*, not different.

## Reproduction

```
# baseline (no IOMMU)             # with SMMUv3
cargo xtask qemu aarch64 demo     cargo xtask qemu aarch64 demo iommu
cargo xtask qemu aarch64 pci program=lspci
                                  cargo xtask qemu aarch64 pci program=lspci iommu
cargo xtask qemu aarch64 storedisk
                                  cargo xtask qemu aarch64 storedisk iommu

# device-tree dumps
qemu-system-aarch64 -M virt,gic-version=2,highmem=off,dumpdtb=/tmp/a.dtb -cpu max -m 512M -nographic
qemu-system-aarch64 -M virt,gic-version=2,highmem=off,iommu=smmuv3,dumpdtb=/tmp/b.dtb -cpu max -m 512M -nographic
dtc -I dtb -O dts /tmp/b.dtb | less    # smmuv3@9050000, pcie iommu-map
```
