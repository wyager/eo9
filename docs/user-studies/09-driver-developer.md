# User study 09 — driver/firmware developer

## Metadata

- **Date:** 2026-05-31
- **Participant:** a driver/firmware developer persona (10+ years: Linux kernel modules, NIC and NVMe
  drivers, some DPDK/SPDK userspace-driver work, virtio internals, embedded firmware; cares about DMA
  safety, interrupt latency, what happens when hardware misbehaves, and how much ceremony a framework
  puts between them and the registers). The participant had no prior knowledge of Eo9 and saw only what
  the facilitator showed; they did not read the repository.
- **Facilitator:** played the demoer; ran every command live and relayed real, trimmed output; nothing
  was fabricated or beautified. Where something does not exist or was not demonstrated, that is said
  outright and recorded here.
- **Code under test:** branch `docs/study-09` at 5985249 (clean, equal to master), guest components
  built with `cargo xtask build-guest`, kernels with `cargo xtask build-kernel aarch64` /
  `cargo xtask build-kernel riscv64`.
- **Environment:** QEMU `virt` under TCG on an Apple-silicon macOS host
  (`qemu-system-aarch64 -M virt,gic-version=2,highmem=off -cpu max -smp 1 -m 512M -nographic
  -device virtio-rng-pci`, plus `-device virtio-blk-pci,disable-legacy=on` over a 64 MiB raw scratch
  image for the disk demos; the riscv64 invocation mirrors it with `-M virt,aia=none` and OpenSBI).
  All timings are emulated (TCG) numbers and were presented to the participant as such.
- **Session shape:** 3 participant rounds. Round 1: pitch + the `eo9:pci` API surface + the
  `disk.virtio` driver source → participant reactions and asks. Round 2: metal demos (enumeration,
  the interrupt-driven storage stack, persistence, trap containment) + code-grounded answers to the
  participant's questions + one failure injection the participant asked for (device IOERR via QEMU
  blkdebug) → reactions and follow-ups. Round 3: containment/attenuation demos (including a bug found
  live), riscv64 parity, two more live runs answering follow-ups (warm-path timing; net-driver
  teardown check) → verdict. The facilitator ran all QEMU sessions for real via a scripted serial
  console; the participant saw only transcripts and code excerpts.

## The demo script (what the participant was shown)

### Round 1 — pitch, the PCI API surface, and the driver source

#### The pitch (two paragraphs, as given)

"Eo9 is a capability OS where programs are wasm components and **imports are permissions**. A device
driver here is not a kernel module: it is an unprivileged wasm component that *imports* the PCI
capability (`eo9:pci/pci`) and *exports* a standard device API (the block driver exports
`eo9:disk/disk`). The kernel's only driver-facing job is to be the root provider of `eo9:pci` —
enumeration, config space, BARs, interrupts, DMA allocation. Everything device-specific lives in the
component. The same driver binary runs unchanged on aarch64 and riscv64; whether it *can* see any
device at all is decided by whoever composes it, not by the driver."

"At the shell you type `disk.virtio $ fs.eofs $ ls` — the virtio-blk driver, under our filesystem,
under a program — and the kernel fuses that composition and compiles it to native code on-target with
its own Cranelift. Containment is by construction: the driver can only touch the devices its provider
shows it, DMA only through buffers the provider allocates, and a misbehaving driver fails with a typed
error or a contained trap, never a kernel panic. Want the driver to see exactly one device? Compose
`pci.filtered --allow "[{…address…}]"` in front of it. Want to refuse it? `pci.deny`. The driver code
does not change."

#### The API surface: `wit/pci/pci.wit`, pasted in full and walked

The facilitator pasted the entire WIT file (242 lines) and walked it top to bottom. Key excerpts as
shown to the participant:

The package doc-comment (the honesty statement about DMA, verbatim):

```
/// This is the capability a *driver* holds. It deliberately carries no device-class
/// semantics (what the registers mean is the driver's business) and no kernel policy
/// (which devices a program may see is the provider's business — see the `filtered`
/// world). A PCI device capability that can enable bus mastering is, absent an IOMMU,
/// effectively full-memory authority; providers are expected to hand out the narrowest
/// device handle that works (see SPEC: Security, The capability algebra).
```

The resource/handle model — five resources, all owned by the provider:

```
resource pci-impl;     // the root handle: a view of (some of) a PCI hierarchy
resource device;       // an opened PCI function: the unit of grant and of driver ownership
resource bar;          // an opened BAR: a register window into one region of one device
resource interrupt;    // one allocated interrupt vector; await it repeatedly
resource dma-buffer;   // a DMA-able region allocated for (and mapped to) one device
```

Enumeration and claiming:

```
enumerate: async func(p: borrow<pci-impl>) -> result<list<device-info>, pci-error>;
open: async func(p: borrow<pci-impl>, address: device-address) -> result<device, pci-error>;
```

Register access — width-explicit (byte/word/dword/qword), config space and BARs symmetric:

```
config-read:  async func(dev: borrow<device>, offset: u32, width: access-width) -> result<u64, pci-error>;
config-write: async func(dev: borrow<device>, offset: u32, width: access-width, value: u64) -> result<_, pci-error>;
bars:     async func(dev: borrow<device>) -> result<list<bar-info>, pci-error>;
open-bar: async func(dev: borrow<device>, index: u8) -> result<bar, pci-error>;
bar-read:  async func(b: borrow<bar>, offset: u64, width: access-width) -> result<u64, pci-error>;
bar-write: async func(b: borrow<bar>, offset: u64, width: access-width, value: u64) -> result<_, pci-error>;
```

Device control, interrupts, and DMA:

```
set-bus-master: async func(dev: borrow<device>, enable: bool) -> result<_, pci-error>;
reset: async func(dev: borrow<device>) -> result<_, pci-error>;

enable-interrupts: async func(dev: borrow<device>, kind: interrupt-kind, count: u32)
    -> result<list<interrupt>, pci-error>;          // intx | msi | msi-x
wait: async func(i: borrow<interrupt>) -> result<u64, pci-error>;   // deliveries coalesced since last wait

alloc-dma: async func(dev: borrow<device>, len: u64) -> result<dma-buffer, pci-error>;
dma-address: func(b: borrow<dma-buffer>) -> u64;    // the device-visible (bus/IOVA) address
dma-len: func(b: borrow<dma-buffer>) -> u64;
dma-read:  func(b: borrow<dma-buffer>, offset: u64, len: u64) -> list<u8>;
dma-write: func(b: borrow<dma-buffer>, offset: u64, bytes: list<u8>);
```

The typed error vocabulary:

```
variant pci-error {
    denied,        // refused by policy (pci.deny / outside a filter's allow-list)
    not-found,     // no device at that address (or no such BAR / vector)
    busy,          // already claimed by another driver
    out-of-range,  // access outside the config space, BAR, or DMA buffer
    unsupported,   // e.g. MSI-X on a device without the capability, qword config access
    exhausted,     // no interrupt vectors or DMA address space left
    io(string),
}
```

And the three stub worlds that make policy composable: `pci.none` (the capability was not granted —
optional flavor answers `none`), `pci.deny` (every operation fails `denied`), and `pci.filtered`
(attenuation: an allow-list of device addresses is visible; everything else answers `denied`):

```
/// pci.filtered — attenuation over an underlying PCI capability: only the configured
/// allow-list of device addresses is visible through `enumerate` and openable; everything
/// else answers `denied`. This is how "exactly this one device" grants are composed.
world filtered {
    import pci;
    export pci;
    export filtered-config;
}
```

Facilitator's verbal annotations while walking it:

- Everything is `async func` and operations return `result<_, pci-error>` — device weirdness is a
  value, not a trap, and not a kernel oops.
- DMA is **allocate-only**: there is no "map this guest memory for DMA" call. The driver asks the
  provider for a buffer, gets back a handle, and reads/writes it through `dma-read`/`dma-write` (copies
  in/out of wasm linear memory). The driver never sees a CPU virtual address, and the device-visible
  address (`dma-address`) is just a number the driver writes into descriptors.
- Interrupts are pull, not push: `wait(interrupt)` is an async call that completes on the next
  delivery, returning a coalescing count. There is no callback registration, no ISR running in the
  driver, no interrupt context.
- `open` is an exclusive claim (`busy` for the second claimant) — driver ownership of a function is
  provider-enforced.
- The width-explicit register accessors were a deliberate choice over buffer-oriented reads: register
  access is width-sensitive, so the width is in the signature.

#### The driver source: `guest/stubs/disk-virtio/src/lib.rs`, structure + key excerpts

The facilitator showed the driver's world first (its entire authority):

```
package eo9:disk-virtio@0.1.0;

world virtio {
    import eo9:pci/pci@0.1.0;
    import eo9:text/text@0.1.0;     // one diagnostic line on first use; works without it

    export eo9:disk/types@0.1.0;
    export eo9:disk/disk@0.1.0;
}
```

Then the source structure — a single `#![no_std]` Rust file, **950 lines** including comments, for a
real virtio-blk driver (virtio 1.0 modern, split virtqueue, INTx + polled fallback, flush support):

| Section | Lines (approx) | What it does |
|---|---|---|
| Constants | ~90 | PCI cap IDs, virtio common-config offsets, status bits, queue layout, request types |
| Eager-driving helpers | ~30 | `poll_eager` / `pci_call`: drive the async pci imports to completion, flatten errors |
| `Driver::bring_up()` | ~115 | enumerate → claim → walk capabilities → open BARs → alloc DMA → negotiate → request INTx vector |
| `Driver::start()` | ~110 | reset, ACKNOWLEDGE/DRIVER, feature negotiation, FEATURES_OK readback, bus-master, virtqueue setup, DRIVER_OK, capacity |
| Register helpers | ~50 | common/device-config/notify accesses routed through opened BARs |
| `transfer()` + completion | ~120 | 3-descriptor chain, avail-ring publish, notify kick, interrupt wait / polled fallback |
| `flush_device()` | ~40 | `VIRTIO_BLK_T_FLUSH` when negotiated; no-op (per spec) when the device is write-through |
| Byte↔sector adaptation | ~75 | byte-addressed `eo9:disk` over the 512-byte-sector device; RMW for partial edge sectors |
| Capability-walk (`find_windows`) | ~55 | find the virtio common/notify/ISR/device-config windows in config space |
| The exported `eo9:disk` provider | ~75 | size / flush / read / write glue, typed `ReadError`/`WriteError` |

Excerpt 1 — probe and claim (the start of `bring_up`); the participant was told this is what runs the
first time anything calls the exported disk API ("claim the first virtio-blk function on first use" is
the documented default; choosing a *specific* device is `pci.filtered`'s job, composed in front):

```rust
fn bring_up() -> Result<Driver, String> {
    let root = pci::default();
    let devices = pci_call("disk.virtio: enumerate", pci::enumerate(&root))?;
    let target = devices
        .iter()
        .find(|d| d.vendor_id == VIRTIO_VENDOR && d.device_id == VIRTIO_BLK_MODERN)
        .or_else(|| { /* transitional id 0x1001, accepted only with modern capabilities */ })
        .ok_or_else(|| {
            String::from(
                "disk.virtio: no virtio-blk function is visible through the granted \
                 pci capability (expected vendor 0x1af4, device 0x1042)",
            )
        })?;
    let device = pci_call("disk.virtio: open", pci::open(&root, target.address))?;

    // Walk the vendor-specific capabilities to find the virtio register windows.
    let (common, notify_base, notify_multiplier, device_config, isr) = find_windows(&device)?;
    ...
    // DMA buffers: the ring page, the request header/status page, the data bounce buffer.
    let ring = pci_call("disk.virtio: alloc-dma (ring)", pci::alloc_dma(&device, RING_BYTES))?;
    let request = pci_call("disk.virtio: alloc-dma (request)", pci::alloc_dma(&device, REQ_BYTES))?;
    let data = pci_call("disk.virtio: alloc-dma (data)", pci::alloc_dma(&device, DATA_BYTES))?;
```

Excerpt 2 — the interrupt request at bring-up (feature-detected, never assumed):

```rust
    // Interrupt delivery: ask the provider for one INTx vector. `unsupported` (or any
    // other failure) means this platform/provider does not route PCI interrupts —
    // completion then falls back to polling the used ring, which works everywhere.
    // Interrupt mode also needs the ISR window (reading it clears the device-side
    // cause), so without one the vector is not requested at all.
    if driver.isr.is_some() {
        driver.interrupt = match poll_eager(pci::enable_interrupts(
            &driver._device,
            pci::InterruptKind::Intx,
            1,
        )) {
            Some(Ok(mut vectors)) if !vectors.is_empty() => Some(vectors.remove(0)),
            _ => None,
        };
    }

    // One best-effort diagnostic line so a metal session shows what was probed and how
    // completions are observed.
    let line = format!(
        "disk.virtio: virtio-blk {} sectors ({} MiB), queue size {}, completion: {}",
        driver.capacity_bytes / SECTOR,
        driver.capacity_bytes / (1024 * 1024),
        driver.queue_size,
        if driver.interrupt.is_some() { "INTx interrupt" } else { "polled" },
    );
```

Excerpt 3 — the completion wait (the heart of the interrupts-vs-polling story):

```rust
/// Interrupt mode (a vector was granted at bring-up): ask the provider to `wait` for the
/// device's INTx — the kernel halts the core until the device interrupts — then confirm
/// against the used ring and read the ISR register, which clears the device-side cause
/// so the level-triggered line deasserts before the next wait re-arms it. Any wait
/// failure (bound expiry, console interrupt, no routing) falls back to the polled loop,
/// which works everywhere and is preemptible (it burns guest fuel), so a Ctrl-C that
/// aborted a blocked wait still kills the composition promptly.
fn wait_for_completion(&mut self, what: &str) -> Result<(), String> {
    if let Some(vector) = self.interrupt.take() {
        let mut waits = 0u32;
        let completed = loop {
            if self.used_advanced() {
                break true;
            }
            if waits >= INTERRUPT_WAIT_RETRIES {
                break false;
            }
            waits += 1;
            match poll_eager(pci::wait(&vector)) {
                // A delivery arrived (possibly coalesced/spurious): clear the device's
                // ISR so the line deasserts, then re-check the used ring.
                Some(Ok(_deliveries)) => self.acknowledge_isr(),
                // Bound expiry, console interrupt, a suspending provider, or any other
                // failure: fall back to polling below.
                Some(Err(_)) | None => break false,
            }
        };
        self.interrupt = Some(vector);
        if completed {
            return Ok(());
        }
    }

    // Polled mode / fallback: spin on the used ring (each iteration is a host call).
    let mut spins: u64 = 0;
    loop {
        if self.used_advanced() {
            ...
            return Ok(());
        }
        spins += 1;
        if spins > POLL_LIMIT {
            return Err(format!(
                "disk.virtio: the device did not complete {what} (poll limit)"
            ));
        }
    }
}
```

Excerpt 4 — one request: the three-descriptor chain, built entirely through `dma-write` calls (the
driver never has a pointer into the ring; it computes offsets and asks the provider to write bytes):

```rust
    // Three-descriptor chain at slots 0..2 of the descriptor table.
    let request_address = pci::dma_address(&self.request);
    let data_address = pci::dma_address(&self.data);
    self.write_descriptor(0, request_address + REQ_HEADER_OFFSET, 16, DESC_F_NEXT, 1);
    self.write_descriptor(1, data_address, byte_len as u32, data_flags, 2);
    self.write_descriptor(2, request_address + REQ_STATUS_OFFSET, 1, DESC_F_WRITE, 0);

    // Publish descriptor 0 in the avail ring, then bump avail.idx.
    let slot = u64::from(self.avail_index % self.queue_size);
    pci::dma_write(&self.ring, AVAIL_OFFSET + 4 + 2 * slot, &0u16.to_le_bytes());
    self.avail_index = self.avail_index.wrapping_add(1);
    pci::dma_write(&self.ring, AVAIL_OFFSET + 2, &self.avail_index.to_le_bytes());

    // Kick the device and wait (interrupt) or poll (fallback) for the completion.
    self.notify_queue()?;
    self.wait_for_completion("the request")?;

    let status = pci::dma_read(&self.request, REQ_STATUS_OFFSET, 1)[0];
    if status != BLK_S_OK {
        return Err(format!("disk.virtio: the device reported request status {status}"));
    }
```

Facilitator's verbal annotations on the driver:

- Every device interaction is a host call through the capability. There is no MMIO pointer, no
  `ioremap`, no `volatile` reads — `bar-read`/`bar-write`/`dma-read`/`dma-write` are function calls into
  the provider. The cost model is therefore "one host call per register access / per ring peek", which
  is the honest price of the containment.
- The driver is single-request, run-to-completion: one 3-descriptor chain in flight, one 64 KiB bounce
  buffer reused for every request, queue depth 16 but used one slot at a time. This is a deliberate
  v0 simplification, not an API limit.
- Device misbehavior surfaces as typed errors with a label naming the step
  (`"disk.virtio: the device rejected the negotiated feature set"`, `"… did not complete the request
  (poll limit)"`, `"… reported request status 2"`); a driver bug (out-of-bounds DMA access) traps the
  *component*, which the shell reports as a contained `abnormal: trapped` outcome.
- The same binary adapts at runtime: INTx where the provider routes interrupts (aarch64 GIC,
  riscv64 PLIC), polled used-ring where it does not (x86_64 today, or any provider answering
  `unsupported`). The probe line names which mode it is in.

#### Participant reactions to Round 1 (condensed; their numbering)

1. **"The DMA honesty statement is the most credible thing in the pitch"** — but it means the
   containment claim "has an asterisk the size of the device: the *component* is contained, the
   *device it programs* is not." Nothing shown stops a driver from writing a kernel physical address
   into a descriptor. "Is there an SMMU/IOMMU and does the provider program it? If yes, the
   containment is real. If no, this is the same trust model as a kernel module with extra steps."
2. **The cost model is honest but unquantified.** One 4 KiB read ≈ 12–15 host calls plus 3+ copies in
   a "bounce-buffer-by-fiat architecture." "For virtio-blk at v0, fine. For NVMe at queue depth 256
   or a NIC at line rate, this is exactly the model DPDK exists to kill." Wants ns/host-call.
3. **Interrupts-as-pull is the VFIO eventfd model and the right call** — but only INTx with one
   vector was shown; `enable-interrupts(msi-x, count)` "exists in the signature and is otherwise
   vapor … Until I see MSI-X with N>1 vectors actually firing, the multi-queue story doesn't exist."
4. **The polled fallback has no wall-clock bound** the developer can reason about.
5. **"The async WIT surface is fake at the driver level."** `poll_eager` drives everything
   synchronously; "the driver is a blocking, run-to-completion state machine wearing async clothing"
   — so queue depth 1 may be structural, not a v0 simplification. "Show me the API actually supports
   two requests in flight or stop claiming it does."
6. **The virtio specifics are competent** (FEATURES_OK readback, ISR-read deassert before re-arm,
   used-ring check before the wait closing the lost-interrupt race, transitional-ID handling, flush
   semantics). "950 self-contained lines for a working modern virtio-blk … is compact. Credit where
   due."
7. **Memory ordering and cache coherency are unaddressed.** Are host calls full barriers? Is
   `alloc-dma` memory non-cacheable, or does someone do cache maintenance? "This is the difference
   between 'works in QEMU' and 'works on hardware.'"
8. **Device teardown is the missing safety story.** Who quiesces the device when the driver traps or
   is killed? "DMA into reused memory — the classic use-after-free … it's not optional."
9. **Missing API surface for real devices:** AER, hot-plug/surprise removal, defined `reset`
   semantics, DMA alignment/contiguity guarantees in the contract.
10. **Lazy bring-up** pushes probe failures to first I/O instead of composition time.
11. **"One printf line and a contained trap … is not a debugging story."** What does a trap show?
    Can ring state be dumped? Host calls traced?

Their asks (10): host-call microbenchmark; throughput vs Linux on the same hardware; the DMA escape
test; hung-device timing + Ctrl-C behavior; bad-status injection; driver-bug-trap output; the
exclusive claim demonstrated; `pci.filtered`/`pci.deny` live; MSI-X with 4 vectors; surprise removal.

### Round 2 — metal demos: enumeration, the interrupt-driven storage stack, persistence, containment

Everything below is real output from QEMU sessions run during the study (trimmed: boot banners and
build noise removed; nothing else edited). The serial console was driven by a scripted harness that
waits for the `eosh> ` prompt, paces input, and timestamps every send→next-prompt interval.

#### Demo 2a — enumeration: `cargo xtask qemu aarch64 pci program=lspci`

The `pci` token on the kernel command line is what grants the `eo9:pci` root capability for this
boot; `program=lspci` runs the program headless and powers off.

```
cmdline: pci program=lspci
store: 22 components baked in (1956 KiB components, 15442 KiB artifacts): eosh, hello, outcomes,
cruncher, readwrite, lspci, entropy.seeded, time.frozen, disk.virtio, fs.eofs, pci.filtered,
net.virtio, l2check, net.l4.over-l2, l4check, ls, cat, echo, wc, head, stat, rm
runner: selected `lspci` from the kernel command line
runner: lspci (552448 byte artifact) with kernel text/time/entropy providers
0000:00:00.0 1b36:0008 class 06.00.00 rev 00 endpoint
0000:00:01.0 1af4:1000 class 02.00.00 rev 00 endpoint
0000:00:02.0 1af4:1005 class 00.ff.00 rev 00 endpoint
runner: lspci outcome = success(devices(3))
runner: instantiate + main took 51335 us
[   72568 us] kernel run complete; requesting PSCI SYSTEM_OFF
```

Host bridge, QEMU's default virtio-net, virtio-rng. 51 ms instantiate+run from the precompiled
artifact, ~73 ms boot-to-poweroff total (TCG).

#### Demo 2b — the storage stack on a blank disk: `cargo xtask qemu aarch64 pci disk`, interactive

`disk` attaches a blank 64 MiB raw image as a modern virtio-blk function. First boot, empty disk:

```
eosh> lspci
0000:00:00.0 1b36:0008 class 06.00.00 rev 00 endpoint
0000:00:01.0 1af4:1000 class 02.00.00 rev 00 endpoint
0000:00:02.0 1af4:1005 class 00.ff.00 rev 00 endpoint
0000:00:03.0 1af4:1042 class 01.00.00 rev 01 endpoint
ok: devices(4)
eosh> disk.virtio $ fs.eofs $ ls
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
pci: INTx delivery on line 3 served an interrupt wait (the cpu halted instead of polling)
ok: listed(0)
eosh> disk.virtio $ fs.eofs $ readwrite /hello.txt eo9-on-real-disk
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
ok: round-tripped(16)
eosh> disk.virtio $ fs.eofs $ cat /hello.txt
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
eo9-on-real-diskok: printed(16)
```

Timings (send → next prompt, TCG, no compile cache on this boot): `ls` 38.7 s, `readwrite` 20.5 s,
`cat` 12.3 s — dominated by on-target Cranelift compilation of each composition; `lspci` (precompiled
artifact) 1.1 s. The `pci: INTx delivery …` line is printed by the kernel once per boot, the first
time an interrupt delivery actually serves a driver's `wait` — it is the proof the completion path is
interrupt-driven rather than polled. The driver's own probe line (`completion: INTx interrupt`) shows
which mode the driver negotiated.

Facilitator notes given alongside: the driver re-probes (full device reset + feature negotiation +
queue setup) on every composition run — each command is a fresh component instance; eofs formats the
blank device in place on first mount; `cat`'s file contents run into the outcome line (no trailing
newline) — cosmetic but real.

#### Demo 2c — power-cycle persistence (full QEMU restart, same disk image)

```
eosh> disk.virtio $ fs.eofs $ ls
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
pci: INTx delivery on line 3 served an interrupt wait (the cpu halted instead of polling)
hello.txt
ok: listed(1)
eosh> disk.virtio $ fs.eofs $ cat /hello.txt
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
eo9-on-real-diskok: printed(16)
```

Real persistence: wasm driver → eofs copy-on-write commits → virtio-blk → the raw image, across a
power cycle.

#### Demo 2d — fault containment in the same boot as the driver

```
eosh> outcomes --mode trap --detail driver-study
abnormal: trapped: guest panicked: outcomes: trapping as requested at examples/outcomes/src/lib.rs:40 — error while executing at wasm backtrace:
    0:    0xd14 - eo9_example_outcomes.wasm!_RNvCsfDopLXnaLPZ_7___rustc17rust_begin_unwind
    1:   0x348e - eo9_example_outcomes.wasm!_RNvNtCs9VUmvJJ0cbu_4core9panicking9panic_fmt
    2:    0x94f - eo9_example_outcomes.wasm!main

Caused by:
    wasm trap: wasm `unreachable` instruction executed

eosh> disk.virtio $ fs.eofs $ cat /rv64.txt
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
pci: INTx delivery on line 3 served an interrupt wait (the cpu halted instead of polling)
written-on-riscv64ok: printed(18)
```

A trapping guest is reported as a typed `abnormal` outcome with the panic message and a wasm
backtrace; the shell survives and the very next command runs the storage stack normally. (The same
containment applies to a buggy driver: an out-of-bounds `dma-read`/`dma-write` traps the *component*,
not the kernel.)

#### Demo 2e — device-error injection (run because the participant asked for it)

EIO injected at the QEMU block layer (blkdebug under a qcow2 image: every guest read request returns
`VIRTIO_BLK_S_IOERR`):

```
eosh> disk.virtio $ fs.eofs $ cat /hello.txt
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
error: fs("FsError::Io(\"device error: device i/o failure\")")
```

Bring-up still succeeds (capacity comes from the device-config BAR window, not a virtio request). The
driver correctly detects the IOERR status — its error at that point is
`"disk.virtio: the device reported request status 1"` — **but that message never reaches the user**:
the filesystem provider maps every disk error onto a message-less unit error
(`eofs-core`'s `DeviceError::Io`), so a real hardware error, a hung device, and a composition-machinery
bug (Round 3) all present identically as `device i/o failure` at the console.

#### Facilitator answers given in Round 2 (each grounded in source or a live run)

- **IOMMU: none.** Provider module docs, verbatim: "A PCI device that can bus-master is, absent an
  IOMMU (QEMU `virt` has none configured), effectively full-memory authority, so this provider is
  never linked by default" and "DMA buffers are plain kernel-heap allocations: with the identity map
  the CPU address *is* the bus address." The participant's DMA escape test would succeed; this was
  conceded rather than demonstrated.
- **MSI/MSI-X, function-level `reset`, I/O-space BARs:** all return `unsupported` from the kernel
  root. INTx only.
- **Wait bounds:** one `wait` blocks ≤ 2 s (`INTX_WAIT_BOUND_NS`); the driver retries 4 waits; the
  polled fallback is bounded by 50 M host-call iterations — an iteration bound, not a time bound;
  never measured against a genuinely hung device.
- **Cache coherency:** provider source, verbatim: "QEMU keeps DMA cache-coherent; real hardware will
  need non-cacheable mappings or explicit maintenance here."
- **Teardown:** dropping a `device` resource clears a table slot — no bus-master disable, no quiesce,
  no reset; dropping a `dma-buffer` frees the heap allocation; only the interrupt handle's drop does
  device-relevant work (masks the line). Confirmed from `close_device`/`close_buffer` in the provider.
- **Exclusive claim:** the `busy` check is per-task; machine-wide exclusivity is a recorded follow-up,
  not implemented.
- **DMA contract:** allocations are physically contiguous, page-aligned, ≤ 4 MiB each, ≤ 64 live per
  task — none of which is stated in the WIT.
- **Host-call cost / throughput vs Linux:** declined as unmeasurable (no real hardware target, no perf
  instrumentation, TCG numbers meaningless) rather than measured badly.

#### Participant reactions to Round 2 (condensed; their numbering)

1. "**The honesty is the most valuable artifact of this round** … This doesn't make the system
   better, but it makes evaluating it possible."
2. "**Disqualifying as shipped: the no-IOMMU + no-teardown combination.** Each alone would be a known
   limitation; together they are a kernel-corruption bug reachable by normal user action … a driver
   is killed mid-request → DMA buffers freed → device still bus-master enabled → device DMAs into
   reused kernel memory. No malice required, no driver bug required. And for virtio-net it's not even
   a window — rx DMA is continuous, so a *busy LAN* does the corrupting for you between commands …
   the failure mode is **silent, delayed, unattributable heap corruption**, which is strictly worse
   than the kernel panic the pitch brags about avoiding."
3. "**The minimum fix is small and the real fix is available today.** Minimum: disable bus-master and
   reset the function *before* any of that task's DMA buffers are freed — that's ordering, ~20 lines.
   Real fix: QEMU virt supports `-machine virt,iommu=smmuv3` (and virtio-iommu; recent QEMU has
   riscv-iommu). 'QEMU virt has none configured' is a choice, not a constraint … Until one of these
   lands, the claim should be 'fault isolation for non-DMA bugs,' not 'containment.'"
4. "**The per-command driver lifetime is a bigger architectural finding than you're treating it
   as** … For a NIC it's structurally broken — a driver that lives for one command can't hold an ARP
   cache, can't hold a TCP connection, can't keep rx buffers posted *safely* … This isn't a v0
   simplification; it's a missing concept: where does a long-running driver live in this model?"
5. "**Performance: what this round establishes is that nothing is established** … the honest
   comparison against kernel/DPDK/SPDK isn't 'slower,' it's 'unmeasured and unmeasurable.'" Asked for
   the warm-path number (same composition twice in one boot).
6. "**The INTx path is real and the kernel attestation is a genuinely good touch** … that's the right
   party making the claim."
7. "**Trap containment: the report surface is better than an oops, the consequences aren't
   contained** … Containment of the report: proven. Containment of the consequences: disproven by
   your own answer."
8. "**The error-swallowing defect is immature, but what it reveals about process is more important:
   nobody has ever debugged a real device failure through this stack**, because if they had, this
   would have been fixed the same afternoon."
9. "**The conceded-vapor list is individually fine and collectively telling: the API has exactly one
   driver's worth of validation** … The 50M-iteration poll bound is not a bound, it's a prayer."
10. "**What survives this round as genuinely real:** enumeration through a granted capability, a
    working interrupt-driven virtio-blk path with correct virtio semantics, persistence across power
    cycles onto a real image, trap reporting with symbols, and a policy model that demonstrably gates
    whether the device exists for the program at all. That's a real vertical slice. It's just a much
    narrower slice than the pitch describes."

Their remaining asks: the teardown fix's status; net.virtio's clean-exit behavior from source;
`pci.filtered` from the driver's perspective + the promised broken thing; the `pci.deny` non-finding;
riscv64 byte-equality + whether its INTx attestation is real; warm-path timing; whether a driver trap
under `fs.eofs` survives to the shell or gets swallowed like the IOERR message.

### Round 3 — containment, attenuation, what breaks, and riscv64 parity

#### Demo 3a — no grant, no devices: boot **without** the `pci` token

`cargo xtask qemu aarch64 disk` — the virtio-blk hardware is attached, but the boot does not grant
the PCI capability:

```
eosh> lspci
error: spawn failed: the program requires PCI device access, which this boot did not grant (add the
`pci` token to the kernel command line — `cargo xtask qemu aarch64 pci` — to provide it) (refused at
instantiation)
eosh> disk.virtio $ fs.eofs $ ls
error: spawn failed: the program requires PCI device access, which this boot did not grant (add the
`pci` token to the kernel command line — `cargo xtask qemu aarch64 pci` — to provide it) (refused at
instantiation)
```

The hardware is physically present; nothing in this boot can touch it. The refusal names the missing
capability and the exact remediation. Caveats the facilitator pointed out from the timing data: the
`lspci` refusal is instant (0.09 s), but the composed `disk.virtio $ fs.eofs $ ls` refusal took
29.5 s — the kernel compiled the whole three-component composition on-target *first* and only then
refused at instantiation.

The shell's own capability report on this boot (`env`) lists text/time/entropy/fs/exec — and, notably,
says nothing about PCI either way (see findings).

`describe` shows a driver's full authority surface at the prompt:

```
eosh> describe disk.virtio
kind: provider
args: (none)
imports:
  required eo9:pci/types (eo9:pci/types@0.1.0)
  required eo9:pci/pci (eo9:pci/pci@0.1.0)
  required eo9:text/types (eo9:text/types@0.1.0)
  required eo9:text/text (eo9:text/text@0.1.0)
  required eo9:io/buffers (eo9:io/buffers@0.1.0)
  required eo9:rt/diagnostics (eo9:rt/diagnostics@0.1.0)
exports:
  eo9:disk/types (eo9:disk/types@0.1.0)
  eo9:disk/disk (eo9:disk/disk@0.1.0)
eosh> describe pci.filtered
kind: provider
args:
  --allow: list<device-address>
...
```

#### Demo 3b — single-device attenuation: `pci.filtered`

Back on the `pci disk` boot (4 visible functions). The allow-list is a baked, compose-time list of
records:

```
eosh> pci.filtered --allow "[{segment: 0, bus: 0, device: 1, function: 0}]" $ lspci
0000:00:01.0 1af4:1000 class 02.00.00 rev 00 endpoint
ok: devices(1)
```

`lspci` — the same unmodified binary that saw 4 devices — now sees exactly one. Composed attenuation,
no kernel policy, no driver changes.

#### Demo 3c — the same attenuation under the *storage* stack: **it does not work** (bug found live)

The disk is at `0000:00:03.0` on this boot. Composing the filter (allowing exactly that address) in
front of the real driver stack:

```
eosh> pci.filtered --allow "[{segment: 0, bus: 0, device: 3, function: 0}]" $ disk.virtio $ fs.eofs $ cat /hello.txt
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
error: fs("FsError::Io(\"device error: device i/o failure\")")
```

The driver *probed successfully through the filter* (the probe line is there: enumerate, open, config
space, BARs, DMA allocation, feature negotiation, and the interrupt request all forwarded correctly) —
and then the first actual disk read failed with an opaque error. The unfiltered
`disk.virtio $ fs.eofs $ cat /hello.txt` works on the same boot.

And with the filter pointing at the *wrong* device (the allow-list names the NIC, so the disk is
invisible to the driver):

```
eosh> pci.filtered --allow "[{segment: 0, bus: 0, device: 1, function: 0}]" $ disk.virtio $ fs.eofs $ ls
error: fs("FsError::Io(\"invalid format options: device too small\")")
```

— a misleading error: the driver correctly reported "no virtio-blk function is visible through the
granted pci capability", but its `size()` contract reports 0 for an unprobeable device, `fs.eofs`
checks the size before the first read, and the real cause never reaches the user.

The facilitator also tried to show the refusal stub and could not:

```
eosh> pci.deny $ lspci
error: cannot resolve `pci.deny` (/bin/pci.deny.wasm): FsError::NotFound
```

`pci.deny` and `pci.none` are not in the kernel's baked store (only `pci.filtered` is).

#### Demo 3d — localizing the 3c failure (run live, after the participant asked what was wrong)

The facilitator ran a differential on a `pci net` boot (modern virtio-net attached; note the device
addresses shift — the NIC is at `00:02.0` on this boot config, virtio-rng at `00:01.0`):

The **polled** virtio-net driver through the same filter **works end-to-end**:

```
eosh> net.virtio $ l2check
net.virtio: virtio-net 52:54:00:12:34:56, queues rx/tx 16/16
l2check: interface virtio0 (52:54:00:12:34:56, mtu 1500)
l2check: 10.0.2.2 is at 52:55:0a:00:02:02
ok: resolved("52:55:0a:00:02:02")
eosh> pci.filtered --allow "[{segment: 0, bus: 0, device: 2, function: 0}]" $ net.virtio $ l2check
net.virtio: virtio-net 52:54:00:12:34:56, queues rx/tx 16/16
l2check: interface virtio0 (52:54:00:12:34:56, mtu 1500)
l2check: 10.0.2.2 is at 52:55:0a:00:02:02
ok: resolved("52:55:0a:00:02:02")
```

But a **double** filter (two attenuators stacked — 4 components, still fully polled, no interrupts
anywhere) fails, and this time the error message survives because the net error path carries strings:

```
eosh> pci.filtered --allow "[… rng …, … nic …]" $ pci.filtered --allow "[… nic …]" $ net.virtio $ l2check
error: net("L2Error::Io(\"net.virtio: enumerate: the pci provider suspended\")")
```

So the failure is about **forwarding depth in the eager-poll chain**, not interrupts: one
guest-to-guest forwarding hop below an eagerly-polling consumer works; two hops produce "the pci
provider suspended". The storage stack has `fs.eofs` eagerly polling `disk.virtio` above the filter
hop — that is its second hop, which is why 3c fails while the shallower net stack works. (This matches
a known caveat recorded in the project's gap list — "suspended-subtask path not yet exercised
end-to-end" — which this study turned from a caveat into a reproduced, user-visible failure of the
flagship attenuation pattern.)

When the filter mis-addressed the NIC (pointing at the rng), the driver's full typed message *was*
preserved by the net path:

```
error: net("L2Error::Io(\"net.virtio: no virtio-net function is visible through the granted pci
capability (expected vendor 0x1af4, device 0x1041)\")")
```

— which is exactly the message the disk path swallowed into "device i/o failure" in 3c.

#### Demo 3e — riscv64 parity: `cargo xtask qemu riscv64 pci disk`, same driver, same disk image

The same scratch disk image (already carrying `hello.txt` written by the aarch64 boots) attached to a
riscv64 QEMU machine, same wasm driver bytes from the riscv64 kernel's store:

```
eosh> lspci
0000:00:00.0 1b36:0008 class 06.00.00 rev 00 endpoint
0000:00:01.0 1af4:1005 class 00.ff.00 rev 00 endpoint
0000:00:02.0 1af4:1042 class 01.00.00 rev 01 endpoint
ok: devices(3)
eosh> disk.virtio $ fs.eofs $ ls
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
hello.txt
ok: listed(1)
eosh> disk.virtio $ fs.eofs $ cat /hello.txt
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
eo9-on-real-diskok: printed(16)
eosh> disk.virtio $ fs.eofs $ readwrite /rv64.txt written-on-riscv64
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
pci: INTx delivery on line 2 served an interrupt wait (the cpu halted instead of polling)
ok: round-tripped(18)
```

- The file written on aarch64 is read back on riscv64 — same driver binary (wasm), same on-disk
  filesystem, different ISA, no changes anywhere.
- Interrupt delivery is through the PLIC (line 2) instead of the GIC (line 3); the driver did not
  change — it asked the capability for an INTx vector and the per-arch routing is the provider's
  business.
- A later aarch64 boot listed both files (`hello.txt`, `rv64.txt`) — persistence is bidirectional
  across architectures.
- Note the disk sits at `00:02.0` here vs `00:03.0` on the aarch64 `disk` boot (riscv64 `virt` has no
  default NIC) — device addresses are a function of the machine configuration, which matters for
  address-based allow-lists.

#### Two more live runs answering Round 2 follow-ups

**Warm-path timing** — the identical composition `disk.virtio $ fs.eofs $ ls` three times in one
boot: **29.6 s, 18.2 s, 11.1 s**. No compile cache is in effect (a cache hit in this system is
milliseconds); the decline is QEMU TCG warming, not reuse — the composition is recompiled by
Cranelift every time. The persistent compile cache that does exist (`storedisk`) cannot be combined
with the `pci`/`disk` grants in the same boot (the kernel and the guest driver would race to claim
the same virtio-blk function). Net effect: **driver development sessions never get the compile
cache** — every driver iteration pays 10–40 s of recompilation per command under TCG.

**net.virtio teardown, from source** — the net driver sets bus-master at bring-up and has no teardown
of any kind: no Drop implementation, no reset on exit, no bus-master disable, ever (the disk driver
is the same). The steady state after any completed `net.virtio $ …` command is a live, bus-mastering
NIC with rx descriptors pointing into freed kernel heap, until the next driver instance's bring-up
reset quiesces it.

**Cross-arch byte identity** — the driver component is one artifact, `eo9-stub-disk-virtio.wasm`,
142,830 bytes, sha256 `7990bf82bc73b8e8…`; both kernels' stores carry those same wasm bytes (plus
per-ISA precompiled artifacts derived from them); compositions on either prompt are fused from the
same bytes and compiled on-target. The riscv64 INTx attestation is real (PLIC source 2), with the
honest detail that on the riscv64 session it printed on the fourth storage command, not the first —
the first three requests had completed before the driver's first wait, so no wait blocked; the
attestation only fires when a wait is actually served, which is exactly why it is trustworthy.

### Round 4 — the participant's verdict (their words, condensed)

**Real (what they'd tell a colleague exists today):** "A capability-gated PCI API with a semantically
correct virtio-blk driver behind it … genuinely interrupt-driven completion through the GIC on
aarch64 and the PLIC on riscv64 — attested by the kernel, not the driver, and the attestation is
trustworthy precisely because it sometimes *doesn't* fire. One set of wasm bytes, hash-verified, both
ISAs, real persistence on a real disk image, bidirectional across architectures. That vertical slice
is not nothing; most 'capability OS' papers never get this far." Also real: the grant model ("no
token → the device doesn't exist for the program"; `describe` showing a driver's complete authority
surface — "Nothing in the kernel-module world gives you that"), attenuation at depth one, the trap
surface, and "the project's honesty culture … it's why my findings list is precise instead of
speculative."

**Vapor / marketing (their words):**
- "**The word 'containment.'** What's contained is the component's memory access. The device it
  programs is unconstrained (no IOMMU), and the device's state after the driver dies is unmanaged (no
  teardown). 'Never a kernel panic' is true and conceals the worse outcome: silent kernel-heap
  corruption by device DMA, reachable by Ctrl-C or by a packet arriving between two shell commands."
- "**The composable-policy story as a system property.** Three policy worlds designed; two aren't in
  the store; the third breaks under the real driver stack. Policy composition is real for `lspci` and
  vapor for the thing the pitch describes."
- "**The async API.** Every signature is async; nothing in the system uses the asyncness. It's a
  synchronous system wearing async types, and every driver pays scaffolding cost for the costume."
- "**MSI-X, reset, hot-plug, machine-wide claiming:** signatures and TODOs."
- "**Performance:** unknown, unmeasurable, all numbers are TCG artifacts. Not 'slower than X' —
  unmeasured."

**Top blockers (their priority order):**
1. **Teardown** — "Today's steady state … is a kernel-corruption bug in normal operation. It was on
   nobody's list until this study. Nothing else on this list matters until this is fixed."
2. **IOMMU-backed DMA**, provable in emulation today — "Until it lands, the PCI capability is root
   with extra steps."
3. **The forwarding-depth bug** — "The flagship attenuation pattern fails under the flagship driver
   stack. Demo 3c is the regression test; it currently fails."
4. **Error fidelity across boundaries** — "the one successful debugging session in this entire study
   (3d) was possible *only* because the net path happens to preserve messages — that's luck, not
   design."
5. **The dev loop** — "No compile cache on any boot that grants PCI means 10–40 s per driver
   iteration. Driver development on this system is not currently viable as a daily activity."
   Then: queue depth > 1, MSI-X, a long-running driver concept.

**Over-engineered:** "the async type surface (cost paid by every driver, benefit delivered to
nobody); the policy-world catalog (three worlds documented, half of one working end-to-end)."
**Under-engineered:** "~115 careful lines of bring-up, **zero** lines of teardown — that ratio is the
project's biggest defect expressed as a number"; cross-boundary error fidelity; the unstated DMA
contract (contiguity/alignment/caps); debugging beyond the trap surface; "compiling a composition for
29.5 s and *then* refusing it at instantiation is backwards — authority is cheap to check; check it
first"; device identity ("allow-lists keyed on bus addresses that shifted between this study's own
boot configs … real filters need stable identity — vendor/device/serial or topological path").

**The containment story vs. what they know:**
- vs. kernel modules: "For the bug classes that dominate real driver work — logic errors, bounds
  errors, panics — Eo9 is genuinely better … For DMA programming errors, Eo9 today is genuinely
  *worse*: Linux has IOMMU support on every serious platform, DMA-API debugging, and at minimum an
  attributable crash; Eo9 has silent, delayed, unattributable corruption."
- vs. VFIO: "the damning comparison, because VFIO got the ordering right. The IOMMU came first …
  Eo9 today is vfio-noiommu with nicer policy ergonomics and a more expensive access path. The
  capability/attenuation model — when it works — is genuinely cleaner than VFIO's
  group/container/ioctl plumbing, and I'd say that without reservation. But VFIO's security claim is
  true and Eo9's is not yet."
- vs. DPDK/SPDK: "not comparable by design or by state … That's a defensible trade *if* the
  containment purchased is real. Today the price is paid and the goods aren't delivered."

**First three things they would build/fix:** (1) teardown ordering in the kernel provider
(bus-master clear + reset on drop and clean exit, before freeing DMA buffers; machine-global claiming
in the same change — "days of work and removes the disqualifying bug"); (2) SMMUv3 in the aarch64
QEMU target + per-device IOMMU domains in `alloc-dma`, with the DMA escape test in CI asserting it
faults — "converts the pitch's central claim from false to true, entirely in emulation"; (3) the
suspended-subtask fix with demo 3c as the regression test, plus string-carrying disk errors in the
same milestone. Fourth/fifth: compile-cache coexistence with PCI grants; a second, structurally
different driver (NVMe) "to put pressure on the parts of the API that virtio-blk never touches."

**Net:** "No, I would not write a driver for this system today, for any purpose beyond studying the
system itself: it cannot keep its own kernel memory safe from the devices it grants, its flagship
policy pattern fails under its flagship driver, and the development loop costs tens of seconds per
iteration. But this is the first capability-OS driver story I've evaluated where the gap between
claim and reality is fully enumerated, the top fixes are small and concrete, and the working parts —
interrupt-driven cross-ISA driver from one binary, grant/refusal with named remediation, the trap
surface — are real rather than staged. … the attenuation model is something I would genuinely want —
handing exactly one PCI function to exactly one tenant, enforced by composition rather than by trust,
is a thing VFIO makes painful and this makes one shell token. … Re-run this study when items 1–3
land; my answer changes that day, not before."

## Findings

### Bugs found live (reproduced, real output in the transcript)

1. **The filtered storage stack fails** (demo 3c): `pci.filtered --allow "[{…the disk's address…}]" $
   disk.virtio $ fs.eofs $ cat` probes successfully through the filter, then fails at the first I/O
   with `fs("FsError::Io("device error: device i/o failure"))`. The unfiltered stack works on the
   same boot. Localized by differential (demo 3d): one guest-to-guest forwarding hop below an
   eagerly-polling consumer works (`pci.filtered $ net.virtio $ l2check` passes); two hops fail with
   "the pci provider suspended" (double-filter polled net stack). The flagship "exactly this one
   device" pattern does not work for the storage stack. This is the GAPS "suspended-subtask path not
   yet exercised end-to-end" caveat turned into a reproduced, user-visible failure.
2. **Driver error messages are swallowed at the disk→fs boundary** (demos 2e, 3c): `eofs-core`'s
   `DeviceError::Io` is a message-less unit variant, so the driver's labelled, typed errors
   ("the device reported request status 1", "no virtio-blk function is visible…") all collapse to
   `device i/o failure`. A real hardware error, a hung device, and the composition bug above are
   indistinguishable at the console. The net path (`L2Error::Io(string)`) preserves messages — the
   inconsistency is what made root-causing finding 1 possible at all.
3. **Misleading error when the filter hides the device** (demo 3c): driver `size()` reports 0 for an
   unprobeable device by design; `fs.eofs` checks size before the first read and reports
   "invalid format options: device too small", burying the real cause (no device visible through the
   granted capability).
4. **`pci.deny` and `pci.none` are not in the kernel store** (demo 3c): the refusal stub cannot be
   used or demonstrated on metal; only `pci.filtered` is baked.
5. **Missing-capability refusal happens after on-target compilation** (demo 3a): a composition whose
   residual imports include ungranted `eo9:pci` is compiled for ~30 s and then refused at
   instantiation. The same refusal for a precompiled program is instant.

### Holes confirmed from source during the study (not previously tracked anywhere)

6. **No device quiesce on teardown** (`close_device`/`close_buffer` in the kernel provider; both
   drivers have no exit teardown): a killed or completed driver leaves its device bus-master enabled
   with descriptors pointing at freed kernel heap. For virtio-net this means continuous rx DMA into
   freed memory between commands — silent kernel-heap corruption in normal operation. The
   participant's top blocker; the minimum fix is ordering (bus-master clear + reset before freeing
   DMA buffers).
7. **No IOMMU** (provider docs concede this): a driver can program its device to DMA anywhere.
   Containment today = "the wasm component cannot touch memory" + "the capability is never granted by
   default", not "the device is constrained". QEMU virt supports smmuv3/virtio-iommu, so the real fix
   is provable in emulation.
8. **Exclusive device claiming is per-task only**: machine-wide exclusivity (two tasks claiming the
   same function) is not enforced; acknowledged in source as a follow-up.
9. **The DMA contract is unstated**: allocations are contiguous, page-aligned, ≤ 4 MiB, ≤ 64 per
   task — none of it in the WIT; drivers discover the caps by hitting `exhausted`.

### Confusions

1. **"Containment" reads as device containment** until the IOMMU/teardown questions are asked; the
   accurate claim today is fault isolation for non-DMA bugs plus grant-gated device visibility.
2. **Device addresses are not stable across boot configurations** — the disk is `00:03.0` on the
   aarch64 `disk` boot, `00:02.0` on riscv64; adding the `net` flag moves the NIC. Address-keyed
   allow-lists silently filter the wrong device when the machine config changes (this bit the
   facilitator live during demo prep: a filter written for one boot's NIC address pointed at the rng
   on the next).
3. **The driver's per-command lifetime** (fresh instance, full re-probe, no teardown per composition
   run) surprised the participant: "where does a long-running driver live in this model?" is an
   unanswered model-level question, not an implementation gap.
4. **The async API surface vs the all-eager reality**: every signature is async; every consumer polls
   eagerly; the kernel's `wait` blocks host-side specifically to accommodate that. A driver developer
   reading the WIT would design for concurrency the system cannot express today.

### Pain points

1. **The driver development loop**: every composition recompiles (10–40 s under TCG), and the compile
   cache (`storedisk`) is mutually exclusive with the PCI/disk grants — driver work never gets it.
2. **Debugging is the typed-error vocabulary or nothing** — and finding 2 means the vocabulary's best
   property (labelled, step-naming messages) does not survive the storage stack. No ring inspection,
   no host-call trace, no way to look at a live driver.
3. **No fault-injection or device-misbehavior test surface**: the facilitator had to reach for QEMU
   blkdebug (and discovered raw images silently never trigger it — qcow2 required) to demonstrate a
   device error at all.
4. **Probe failures surface at first I/O** (lazy bring-up) rather than at composition/spawn time.

### Missing capabilities (in the participant's priority order)

1. Device teardown/quiesce on resource drop and task exit (finding 6).
2. IOMMU-backed DMA (finding 7), provable under QEMU smmuv3/virtio-iommu.
3. The nested-forwarding fix (finding 1) — pci.filtered under the storage stack as the regression test.
4. String-carrying disk errors (finding 2).
5. Compile-cache coexistence with PCI grants / machine-global device claiming.
6. Queue depth > 1 (needs a consumer/executor that drives async imports for real), MSI/MSI-X,
   function-level reset, hot-plug/surprise removal, AER.
7. A long-running driver concept (a driver that outlives one command).
8. A second, structurally different driver (NVMe) to validate the API beyond virtio.

### Performance / footprint reactions

- All numbers are TCG-emulated and were presented as such; the participant's conclusion was
  "unmeasured and unmeasurable" rather than "slow" — no real-hardware target, no perf API.
- Boot-to-prompt ~12–19 s (includes kernel image load under TCG); precompiled programs ~1 s
  prompt-to-prompt; composed driver stacks 10–40 s each, ~95 % on-target Cranelift compilation.
- The interrupt-vs-polled distinction is invisible at session granularity under TCG (compilation
  dominates); its value is structural (CPU halts instead of burning host calls).
- The cost model (a host call per register access / ring peek; copies at `dma-read`/`dma-write` and
  at the disk export boundary) was accepted as the honest price of containment for block storage at
  v0, and rejected as a basis for NICs at line rate or NVMe at depth.

### What landed well

- **The kernel's INTx attestation line** ("served an interrupt wait — the cpu halted instead of
  polling"): the right party (the kernel, not the driver) attesting the load-bearing property, and it
  only prints when true.
- **The grant/refusal model**: no `pci` token → instant refusal naming the missing capability and the
  exact remediation; `describe` showing a driver's full authority surface before running it.
- **Single-device attenuation for enumeration**: unmodified `lspci` seeing 4 devices or exactly 1
  depending on what is composed in front of it.
- **Cross-ISA parity from one binary**: the same wasm bytes driving the same disk image on aarch64
  (GIC) and riscv64 (PLIC), with files written on one architecture read on the other.
- **The virtio implementation quality** (the participant's own assessment, point 6 of round 1).
- **The trap surface**: panic message + source location + symbolized backtrace + surviving shell.
- **The honesty norm**: provider doc-comments that name their own holes; the facilitator's concessions
  were quoted from the code rather than extracted by interrogation.

### Criticisms (the participant's, preserved)

1. The no-IOMMU + no-teardown combination is disqualifying as shipped, and its failure mode (silent
   delayed heap corruption) is strictly worse than the kernel panic the pitch positions itself
   against.
2. The policy/attenuation story is "real for `lspci` and vapor for the thing the pitch describes."
3. The API has "exactly one driver's worth of validation."
4. Bring-up:teardown effort ratio (~115 lines : 0) "is the project's biggest defect expressed as a
   number."
5. Authority checking ordered after compilation is backwards.
6. vs VFIO: "Eo9 today is vfio-noiommu with nicer policy ergonomics and a more expensive access
   path."

## Triage table

Per the project's no-drop rule, every finding above is dispositioned: fix-now (an obvious, contained
fix an area agent can land), tracked (needs a GAPS.md entry + a planned pass), or needs-owner-decision.

| # | Finding | Disposition | Concrete next step |
|---|---|---|---|
| 6 | No device quiesce on teardown (devices left bus-mastering into freed heap) | **Fix-now** (highest severity) | Kernel provider: on `device` drop and on task exit, clear the command-register bus-master bit (and attempt reset) *before* freeing that task's DMA buffers; mirror a best-effort bus-master-disable at clean exit in disk.virtio/net.virtio |
| 1 | `pci.filtered` under the storage stack fails (nested forwarding suspends) | **Tracked** | GAPS: promote the "suspended-subtask path" caveat to a reproduced bug; demo 3c is the regression test; needs the binder/executor pass that drives nested cross-component async calls |
| 2 | Disk→fs boundary swallows driver error messages | **Fix-now** | Give `eofs-core::DeviceError::Io` a message payload (or add `Io(String)`-style variant) and thread the disk error string through `fs-eofs`'s `device_error()` |
| 3 | "Device too small" masks "no device visible" | **Fix-now** | `fs.eofs`: when `disk.size()` is 0, attempt one read and surface its typed error instead of the format-options message |
| 4 | `pci.deny`/`pci.none` not in the kernel store | **Fix-now** | Add both to `KERNEL_STORE_COMPONENTS` (store 22 → 24 entries) |
| 5 | Missing-grant refusal after a 30 s compile | **Tracked** | GAPS: shellexec should check residual imports against the boot's grants at compose time, before invoking Cranelift |
| 7 | No IOMMU — device DMA unconstrained | **Needs-owner-decision** | Adopt the participant's plan (QEMU `virt,iommu=smmuv3` + per-device domains in `alloc-dma` + a CI escape test) vs. defer to real-board work; until then, soften the containment wording in SPEC/README |
| 8 | Device claiming is per-task, not machine-wide | **Tracked** | Already acknowledged in provider source; goes to GAPS with the storedisk-coexistence item (same root) |
| 9 | DMA contract (contiguity/alignment/caps) unstated in WIT | **Fix-now** | Document in `pci.wit` doc-comments: contiguous, page-aligned, per-alloc and per-task caps, and that `exhausted` is the cap signal |
| C2 | Address-keyed allow-lists are fragile across boot configs | **Needs-owner-decision** | Whether `pci.filtered` should also/instead match on vendor:device (and how to express "the Nth match") — an API design question |
| C3 | No long-running driver concept | **Needs-owner-decision** | Model-level: where does a daemon-like driver live? Tied to the Message API / supervision design |
| C4 | Async API vs all-eager reality (queue depth 1 structural) | **Tracked** | GAPS already carries the async disk/net bridge follow-up; add the driver-API consequence (concurrency promised by signatures is not realizable) |
| P1 | Compile cache unusable with PCI grants | **Tracked** | Same fix as machine-global claiming (finding 8); record in GAPS as a driver-DX blocker |
| P3 | No fault-injection surface for drivers | **Tracked** | GAPS: a `pci.fault` middleware stub (inject errors/delays/bad status between driver and root) would make device-misbehavior testable in pure wasm |
| 5 (R1) | MSI/MSI-X, FLR, hot-plug, AER all `unsupported` | **Tracked** | Already partially in plan/12 D59 "Remaining"; add hot-plug/AER explicitly so the API's unimplemented surface is enumerated in one place |

## Facilitator observations (independent of the participant)

- **The system's own test/verification history created the blind spot this study walked into.** The
  filtered-driver path (finding 1) is untestable in usermode by design (no pci provider exists
  there), and the metal verification of `pci.filtered` only ever ran `lspci` through it. The first
  time anyone composed the filter under the real storage stack was this study. Anything that can only
  be tested on metal, interactively, effectively has no tests.
- **QEMU blkdebug silently does nothing on raw images** — error injection requires a format driver
  (qcow2) above it. Cost the facilitator one wasted run; worth knowing for future fault-injection
  work.
- **The scripted-console conventions held**: waiting for the prompt before sending, pacing input
  character-by-character, and per-command timestamps were enough to drive 9 QEMU sessions (2
  architectures, 5 boot configurations) without a single dropped or mangled command.
- **The participant's two hardest questions were answered by reading provider source, not by running
  anything** (teardown, per-task claiming). The provider's honest doc-comments made those answers
  fast to find — the project's documentation honesty is a real asset for exactly this kind of
  scrutiny.
- **The shared scratch disk between architectures** (one `eo9-scratch-disk.raw` used by both the
  aarch64 and riscv64 QEMU configs) turned out to be the study's best demo for free: cross-ISA
  persistence was discovered, not planned.
- The facilitator had to decline 4 of the participant's 10 asks (host-call ns, Linux comparison, hung
  device, surprise removal) as unmeasurable/uninjectable with what exists today — each declination is
  itself a finding about missing instrumentation and test surface.
