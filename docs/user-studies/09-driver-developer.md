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
- **Session shape:** ~4 rounds: pitch + the `eo9:pci` API surface + the `disk.virtio` driver source →
  metal demos (enumeration, the interrupt-driven storage stack, persistence across power cycles,
  single-device attenuation, refusal without the grant) → containment story + riscv64 parity +
  follow-up answers → verdict and recommendations.

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

*(Demo material complete and committed. The participant phase — reactions, questions, verdict — and
the findings/triage tables follow.)*
