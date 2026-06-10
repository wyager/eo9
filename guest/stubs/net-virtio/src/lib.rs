//! `net.virtio` — a virtio-net driver as an ordinary wasm component.
//!
//! Targets the crate-local `eo9:net-virtio/virtio-net` world: imports the PCI
//! capability (`eo9:pci/pci`) plus `eo9:text/text` for one diagnostic line, and exports
//! `eo9:net/l2` (interfaces, MAC addresses, whole Ethernet frames) backed by a modern
//! (virtio 1.0, `disable-legacy=on`) virtio-net PCI function. The driver holds no policy
//! of its own: which functions it can see (and therefore claim) is entirely the PCI
//! provider's business — the kernel root only when the boot granted `pci`, an
//! attenuating `pci.filtered` for "exactly this one device" grants, `pci.deny` to
//! refuse.
//!
//! Shape of the device conversation (virtio 1.0 over PCI, the same probe/bring-up as
//! `disk.virtio`, plan/12 Decision 50):
//!
//! * **Probe.** Enumerate the capability's view of the bus, claim the first virtio-net
//!   function (vendor 0x1af4, modern device id 0x1041; the transitional id 0x1000 is
//!   accepted when it carries the modern capabilities), and walk its vendor-specific
//!   PCI capabilities to find the common / notify / device-config register windows.
//! * **Bring-up.** Reset, ACKNOWLEDGE → DRIVER, negotiate `VIRTIO_F_VERSION_1` plus
//!   `VIRTIO_NET_F_MAC`, FEATURES_OK (verified by reading it back), enable bus
//!   mastering, build the receive and transmit virtqueues (16 entries each) in DMA
//!   buffers obtained from `alloc-dma`, DRIVER_OK, read the MAC address from the device
//!   config window, and pre-post the receive buffers.
//! * **I/O.** Every frame crosses the rings with the 12-byte virtio-net header in
//!   front of it (zeroed on transmit — no offloads are negotiated — and stripped on
//!   receive). Transmit publishes one descriptor and polls the used ring for
//!   completion; receive polls the used ring for the next delivered buffer, copies the
//!   frame out, and immediately re-posts the buffer. The used-ring polling bounds
//!   stay: every await here resolves within the call — pci operations are plain
//!   MMIO/memory work in the provider — so the bounds are what keep a wedged device a
//!   typed error instead of a hang, per the SPEC's awaits-are-bounded rule.
//! * **The receive event (plan/12 D59, the timer-crutch audit's A2).** Where the PCI
//!   provider routes INTx (QEMU aarch64-virt and riscv64 today), bring-up asks for one
//!   vector and leaves used-buffer interrupts enabled on the receive queue, and the
//!   exported `wait-recv` parks the calling task on the device's RX interrupt (bounded
//!   by the caller's `max-wait-ns`, clamped by the provider) — so a consumer waiting
//!   for traffic costs the machine nothing instead of busy-pumping `recv-frame`. Where
//!   interrupts are not routed (`enable-interrupts` answers `unsupported`: x86_64, the
//!   v1 platform provider), `wait-recv` returns immediately and the consumer degrades
//!   to exactly the poll loop it always ran.
//!
//! The exported `eo9:net/l2` surface is the single interface `virtio0`: `recv-frame`
//! that finds nothing within its short poll window reports **an empty result**
//! (`bytes-received: 0`, the WIT's "nothing waiting right now") so the consumer owns
//! the wait policy — a TCP/IP middleware pumps again on its own deadline, a one-shot
//! prober retries a few times — instead of every consumer paying a multi-second spin
//! (user study 08, finding F2). A frame larger than the transmit buffer fails with
//! `frame-too-large`, and device weirdness is always a typed error, never a trap.
//!
//! The driver's `eo9:pci` import calls are genuine awaits (the SPEC's "boundaries are
//! honestly async" rule): each one is plain MMIO / memory work in the provider and
//! resolves within the call today, but nothing in this driver depends on that — a
//! provider that suspends just parks the operation, and the consumer above absorbs the
//! suspension by awaiting its own l2 calls. The documented default state (no configure
//! interface) is "claim the first virtio-net function on first use"; first use also
//! prints one `net.virtio: …` diagnostic line so a metal session shows what was probed.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "virtio-net",
    path: "wit",
    // Pull in bindings for eo9:pci/types and eo9:io/buffers, which the imported and
    // exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::pci::pci;
use eo9::text::text;
use exports::eo9::net::l2::{self, Buffer, InterfaceInfo, L2Error, RecvResult, SendResult};

// ------------------------------------------------------------------------------------------
// Constants: PCI configuration space, virtio-pci capabilities, the common config window,
// the split virtqueues, and the virtio-net header. All multi-byte device fields are
// little-endian (virtio 1.0 §4.1; both supported kernels are little-endian).
// ------------------------------------------------------------------------------------------

/// virtio vendor id.
const VIRTIO_VENDOR: u16 = 0x1af4;
/// Modern (virtio 1.0+) virtio-net device id (0x1040 + device type 1).
const VIRTIO_NET_MODERN: u16 = 0x1041;
/// Transitional virtio-net device id; accepted only if it carries the modern capabilities.
const VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;

/// Configuration-space offset of the capabilities pointer.
const PCI_CAP_POINTER: u32 = 0x34;
/// Vendor-specific capability id (virtio structures).
const PCI_CAP_ID_VENDOR: u64 = 0x09;
/// Upper bound on capability-list traversal (the list is at most 48 entries by layout).
const PCI_CAP_WALK_LIMIT: usize = 48;

/// virtio_pci_cap.cfg_type values.
const VIRTIO_PCI_CAP_COMMON: u64 = 1;
const VIRTIO_PCI_CAP_NOTIFY: u64 = 2;
const VIRTIO_PCI_CAP_ISR: u64 = 3;
const VIRTIO_PCI_CAP_DEVICE: u64 = 4;

/// Offsets within the common configuration window (virtio 1.0 §4.1.4.3).
const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00;
const COMMON_DEVICE_FEATURE: u64 = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: u64 = 0x08;
const COMMON_DRIVER_FEATURE: u64 = 0x0c;
const COMMON_NUM_QUEUES: u64 = 0x12;
const COMMON_DEVICE_STATUS: u64 = 0x14;
const COMMON_QUEUE_SELECT: u64 = 0x16;
const COMMON_QUEUE_SIZE: u64 = 0x18;
const COMMON_QUEUE_ENABLE: u64 = 0x1c;
const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1e;
const COMMON_QUEUE_DESC: u64 = 0x20;
const COMMON_QUEUE_DRIVER: u64 = 0x28;
const COMMON_QUEUE_DEVICE: u64 = 0x30;

/// Device status bits.
const STATUS_ACKNOWLEDGE: u64 = 1;
const STATUS_DRIVER: u64 = 2;
const STATUS_DRIVER_OK: u64 = 4;
const STATUS_FEATURES_OK: u64 = 8;

/// `VIRTIO_F_VERSION_1` is feature bit 32: bit 0 of the high feature word.
const FEATURE_VERSION_1_HIGH: u64 = 1;
/// `VIRTIO_NET_F_MAC` is feature bit 5 of the low feature word: the device config
/// window carries a stable MAC address.
const FEATURE_MAC_LOW: u64 = 1 << 5;

/// Split-virtqueue descriptor flags.
const DESC_F_WRITE: u16 = 2;

/// `VIRTQ_AVAIL_F_NO_INTERRUPT` (virtio 1.0 §2.6.7): tell the device this driver does
/// not want used-buffer interrupts on a queue. Both rings start suppressed at setup;
/// when bring-up is granted an INTx vector (plan/12 D59 landed: receive is
/// interrupt-capable through `wait-recv`), the **receive** ring's flag is cleared again
/// and the driver acks its ISR like the interrupt-mode siblings (disk.virtio,
/// gpu.virtio). The **transmit** ring stays suppressed always: transmit completion is
/// consumed by an inline poll within the same `send`, so a tx interrupt would assert
/// the level-triggered line with nobody waiting on it — exactly the stale-delivery
/// wedge the suppression exists to prevent. Where no vector is granted (interrupts
/// unrouted) the receive ring stays suppressed too, and the driver is purely polled as
/// before. The flag is the spec's hint, not a guarantee, which is exactly right: a
/// device that interrupts anyway costs nothing (the line is masked at the controller
/// outside waits), suppression just stops the routine wedging.
const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Queue size the driver uses for both queues (the device's maximum is reduced to this).
const QUEUE_SIZE: u16 = 16;
/// Per-queue ring layout inside the shared ring DMA page: the receive queue's rings live
/// at offset 0, the transmit queue's at 2048; within each region the descriptor table,
/// avail ring, and used ring sit at the same offsets `disk.virtio` uses (alignments
/// 16 / 2 / 4 are all satisfied).
const RING_REGION: u64 = 2048;
const RX_RING_BASE: u64 = 0;
const TX_RING_BASE: u64 = RING_REGION;
const RING_BYTES: u64 = 4096;
const DESC_OFFSET: u64 = 0; // 16 bytes * 16 entries = 256
const AVAIL_OFFSET: u64 = 256; // 6 + 2 * 16 = 38
const USED_OFFSET: u64 = 512; // 6 + 8 * 16 = 134

/// The 12-byte virtio-net header (virtio 1.0 §5.1.6) that precedes every frame on the
/// rings once VERSION_1 is negotiated. No offload features are negotiated, so it is
/// all-zeroes on transmit and ignored (stripped) on receive.
const VNET_HEADER: u64 = 12;

/// Receive buffers: 8 slots of 2 KiB each (an Ethernet frame is at most 1514 bytes plus
/// the 12-byte header), pre-posted to the device and re-posted as they are consumed.
const RX_SLOTS: u16 = 8;
const RX_SLOT_BYTES: u64 = 2048;
const RX_DATA_BYTES: u64 = RX_SLOT_BYTES * RX_SLOTS as u64;
/// Transmit bounce buffer: one frame at a time (header + frame).
const TX_DATA_BYTES: u64 = 2048;
/// Largest frame `send-frame` accepts (the transmit buffer minus the virtio-net header).
const MAX_FRAME: u64 = TX_DATA_BYTES - VNET_HEADER;
/// The MTU reported for the interface (classic Ethernet payload size).
const MTU: u32 = 1500;
/// The single interface name this driver exposes.
const INTERFACE_NAME: &str = "virtio0";

/// Transmit-completion polling bound (each iteration is a host call); the device
/// consumes a transmit descriptor in microseconds, so hitting this means it is wedged.
const TX_POLL_LIMIT: u64 = 50_000_000;
/// Receive polling bound: how many host calls `recv-frame` spends checking for a frame
/// before reporting "nothing waiting" (an empty result, not an error). Calibrated to a
/// couple of milliseconds of host calls — long enough to catch a reply already in
/// flight (QEMU user-net answers ARP/DNS in tens of microseconds), short enough that a
/// consumer polling a quiet link pays milliseconds per poll, not seconds. The consumer
/// owns the wait policy: the TCP/IP middleware pumps repeatedly under its own
/// deadlines, and one-shot probes like l2check retry a bounded number of times.
/// (User study 08 finding F2: this was 2,000,000 — ~1.7 s per empty poll — which
/// stacked up to ~6.7 s ARP stalls through the middleware's pump loop.)
const RX_POLL_LIMIT: u64 = 2_000;

/// Rate limit for the missed-RX-interrupt liveness finding (the detector discipline:
/// a bounded wait that expires with receive work already in the ring means the event
/// path failed to deliver — report it loudly, first occurrence and every 16th after).
const LIVENESS_REPORT_EVERY: u32 = 16;

// ------------------------------------------------------------------------------------------
// Awaited driving of the async pci imports.
// ------------------------------------------------------------------------------------------

/// Run one PCI operation to completion and flatten its result, labelling failures with
/// `what`. The await is genuine: a provider that completes within the call (the kernel
/// root, `pci.deny`) resolves on the spot; one that suspends (an interposed guest
/// middleware) parks this driver's activation, and the consumer above absorbs that by
/// awaiting its own l2 call.
async fn pci_call<T>(
    what: &str,
    future: impl Future<Output = Result<T, pci::PciError>>,
) -> Result<T, String> {
    match future.await {
        Ok(value) => Ok(value),
        Err(error) => Err(format!("{what}: {error:?}")),
    }
}

// ------------------------------------------------------------------------------------------
// Driver state
// ------------------------------------------------------------------------------------------

/// One register window discovered from a virtio PCI capability: which BAR it lives in
/// and at which offset within that BAR.
struct Region {
    bar: u8,
    offset: u64,
}

/// One split virtqueue: where its doorbell is and the driver-side ring indices (which
/// ring block it uses inside the shared ring page is fixed by `RX_RING_BASE` /
/// `TX_RING_BASE`).
struct Queue {
    /// Absolute offset of this queue's doorbell within the notify BAR.
    notify_offset: u64,
    size: u16,
    /// Next avail-ring index (free-running, wraps at 65536 like the device's view).
    avail_index: u16,
    /// Used-ring entries consumed so far (free-running).
    used_index: u16,
}

/// The brought-up device: claimed function, opened BARs, the two virtqueues, and the
/// DMA buffers every frame reuses.
struct Driver {
    /// Keeps the exclusive claim on the function alive for the component's lifetime.
    _device: pci::Device,
    /// Opened BARs, one handle per distinct BAR index the capabilities referenced.
    bars: Vec<(u8, pci::Bar)>,
    common: Region,
    device_config: Region,
    notify: Region,
    /// The ISR status window (read-to-clear; deasserts INTx). `None` when the device
    /// exposes no ISR capability — interrupt mode is then never entered.
    isr: Option<Region>,
    /// The INTx vector the provider granted, or `None` when interrupts are not routed
    /// on this platform/provider (`wait-recv` then returns immediately and the
    /// consumer stays on its poll loop).
    interrupt: Option<pci::Interrupt>,
    /// Missed-interrupt liveness findings so far (rate-limits the loud line).
    missed_intx: u32,
    rings: pci::DmaBuffer,
    rx_data: pci::DmaBuffer,
    tx_data: pci::DmaBuffer,
    rx: Queue,
    tx: Queue,
    mac: [u8; 6],
}

/// Failures of the link-layer operations, mapped to the WIT error variants by the
/// export glue.
enum L2Fail {
    FrameTooLarge,
    Io(String),
}

impl From<L2Fail> for L2Error {
    fn from(fail: L2Fail) -> L2Error {
        match fail {
            L2Fail::FrameTooLarge => L2Error::FrameTooLarge,
            L2Fail::Io(message) => L2Error::Io(message),
        }
    }
}

/// The driver's home between operations. An operation takes the driver *out* of the
/// slot for its duration (the same discipline as `net.l4.over-l2`'s link slot): a
/// `ProviderState` borrow must never be held across an await, so the state cannot be
/// borrowed in place while pci calls run.
struct DriverSlot {
    driver: Option<Driver>,
    /// Whether bring-up has been claimed (set before the first `bring_up().await`, so a
    /// concurrent first use cannot start a second probe; cleared again if bring-up
    /// fails, so the next use retries).
    brought_up: bool,
}

static STATE: ProviderState<DriverSlot> = ProviderState::new();

/// Puts the driver back in its slot when the operation that took it finishes —
/// including by *cancellation* (the operation's future dropped mid-await), so a
/// cancelled operation can never leave the slot empty. A transmit the cancelled
/// operation left published is settled by the next `send`'s [`Driver::drain_tx`]
/// before any shared state is reused; the guard itself stays synchronous and free of
/// device access (`Drop` cannot await).
struct DriverGuard(Option<Driver>);

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if let Some(driver) = self.0.take() {
            STATE.with(|slot| slot.driver = Some(driver));
        }
    }
}

impl core::ops::Deref for DriverGuard {
    type Target = Driver;
    fn deref(&self) -> &Driver {
        self.0
            .as_ref()
            .expect("the driver is held for the guard's lifetime")
    }
}

impl core::ops::DerefMut for DriverGuard {
    fn deref_mut(&mut self) -> &mut Driver {
        self.0
            .as_mut()
            .expect("the driver is held for the guard's lifetime")
    }
}

/// What `acquire_driver` found in the slot.
enum SlotView {
    Ready(Driver),
    Busy,
    NeedBringUp,
}

/// Take the driver for one operation, probing and initializing the device on first use
/// (the documented default state — there is no configure interface). A second
/// activation arriving while one is parked mid-operation gets a typed error, never a
/// re-entrant borrow trap.
async fn acquire_driver() -> Result<DriverGuard, L2Fail> {
    if !STATE.is_set() {
        STATE.set(DriverSlot {
            driver: None,
            brought_up: false,
        });
    }
    let view = STATE.with(|slot| {
        if let Some(driver) = slot.driver.take() {
            SlotView::Ready(driver)
        } else if slot.brought_up {
            SlotView::Busy
        } else {
            slot.brought_up = true;
            SlotView::NeedBringUp
        }
    });
    match view {
        SlotView::Ready(driver) => Ok(DriverGuard(Some(driver))),
        SlotView::Busy => Err(L2Fail::Io(String::from(
            "net.virtio: another operation on this device is in progress",
        ))),
        SlotView::NeedBringUp => {
            // `brought_up` is set from the `with` above: arm the restore before
            // the first await of bring-up, so an error return *or a future dropped
            // mid-bring-up* clears the claim and the next use retries (instead of
            // wedging the instance behind the typed busy answer).
            let claim = BringUpClaim { armed: true };
            let driver = Driver::bring_up().await.map_err(L2Fail::Io)?;
            claim.defuse();
            Ok(DriverGuard(Some(driver)))
        }
    }
}

/// Releases the bring-up claim (`brought_up`) if bring-up never completes; armed from
/// the instant the claim exists, defused on success when the [`DriverGuard`] takes
/// over (a successful bring-up keeps `brought_up = true` for the instance's lifetime).
struct BringUpClaim {
    armed: bool,
}

impl BringUpClaim {
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for BringUpClaim {
    fn drop(&mut self) {
        if self.armed {
            STATE.with(|slot| slot.brought_up = false);
        }
    }
}

impl Driver {
    /// Find, claim, and bring up the first virtio-net function visible through the
    /// granted PCI capability. Every step reports a typed, labelled error — device
    /// weirdness is an `io` failure of the l2 operation, never a trap.
    async fn bring_up() -> Result<Driver, String> {
        let root = pci::default();
        let devices = pci_call("net.virtio: enumerate", pci::enumerate(&root)).await?;
        let target = devices
            .iter()
            .find(|d| d.vendor_id == VIRTIO_VENDOR && d.device_id == VIRTIO_NET_MODERN)
            .or_else(|| {
                devices.iter().find(|d| {
                    d.vendor_id == VIRTIO_VENDOR && d.device_id == VIRTIO_NET_TRANSITIONAL
                })
            })
            .ok_or_else(|| {
                String::from(
                    "net.virtio: no virtio-net function is visible through the granted \
                     pci capability (expected vendor 0x1af4, device 0x1041)",
                )
            })?;
        let address = target.address;
        let device = pci_call("net.virtio: open", pci::open(&root, address)).await?;

        // Walk the vendor-specific capabilities to find the virtio register windows.
        let (common, notify_base, notify_multiplier, device_config, isr) =
            find_windows(&device).await?;

        // Open each BAR the windows live in exactly once.
        let mut bar_indices: Vec<u8> = Vec::new();
        for index in [common.bar, notify_base.bar, device_config.bar] {
            if !bar_indices.contains(&index) {
                bar_indices.push(index);
            }
        }
        if let Some(isr) = &isr
            && !bar_indices.contains(&isr.bar)
        {
            bar_indices.push(isr.bar);
        }
        let mut bars: Vec<(u8, pci::Bar)> = Vec::new();
        for index in bar_indices {
            let bar = pci_call("net.virtio: open-bar", pci::open_bar(&device, index)).await?;
            bars.push((index, bar));
        }

        // DMA buffers: one page holding both queues' rings, the receive slots, and the
        // transmit bounce buffer. CPU address == device address under the kernel's
        // identity map; the provider hands back the device-visible address via
        // `dma-address`.
        let rings = pci_call(
            "net.virtio: alloc-dma (rings)",
            pci::alloc_dma(&device, RING_BYTES),
        )
        .await?;
        let rx_data = pci_call(
            "net.virtio: alloc-dma (receive buffers)",
            pci::alloc_dma(&device, RX_DATA_BYTES),
        )
        .await?;
        let tx_data = pci_call(
            "net.virtio: alloc-dma (transmit buffer)",
            pci::alloc_dma(&device, TX_DATA_BYTES),
        )
        .await?;

        let mut driver = Driver {
            _device: device,
            bars,
            common,
            device_config,
            notify: notify_base,
            isr,
            interrupt: None,
            missed_intx: 0,
            rings,
            rx_data,
            tx_data,
            rx: Queue {
                notify_offset: 0,
                size: 0,
                avail_index: 0,
                used_index: 0,
            },
            tx: Queue {
                notify_offset: 0,
                size: 0,
                avail_index: 0,
                used_index: 0,
            },
            mac: [0; 6],
        };
        driver.start(notify_multiplier).await?;
        Ok(driver)
    }

    /// Negotiate features, build both virtqueues, read the MAC, and pre-post the
    /// receive buffers — the device side of bring-up, once the function is claimed and
    /// the DMA buffers exist.
    async fn start(&mut self, notify_multiplier: u32) -> Result<(), String> {
        // Reset, then ACKNOWLEDGE and DRIVER.
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, 0)
            .await?;
        let mut spins = 0u32;
        while self
            .common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)
            .await?
            != 0
        {
            spins += 1;
            if spins > 1000 {
                return Err(String::from("net.virtio: device did not reset"));
            }
        }
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            STATUS_ACKNOWLEDGE,
        )
        .await?;
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER,
        )
        .await?;

        // Feature negotiation: VIRTIO_F_VERSION_1 is required (it is what makes the
        // modern register layout valid at all); VIRTIO_NET_F_MAC is required so the
        // device-config window carries a stable MAC address (QEMU always offers it).
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 0)
            .await?;
        let low_features = self
            .common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)
            .await?;
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 1)
            .await?;
        let high_features = self
            .common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)
            .await?;
        if high_features & FEATURE_VERSION_1_HIGH == 0 {
            return Err(String::from(
                "net.virtio: the device does not offer VIRTIO_F_VERSION_1 \
                 (is it a legacy-only function?)",
            ));
        }
        if low_features & FEATURE_MAC_LOW == 0 {
            return Err(String::from(
                "net.virtio: the device does not offer VIRTIO_NET_F_MAC",
            ));
        }
        self.common_write(COMMON_DRIVER_FEATURE_SELECT, pci::AccessWidth::Dword, 0)
            .await?;
        self.common_write(
            COMMON_DRIVER_FEATURE,
            pci::AccessWidth::Dword,
            FEATURE_MAC_LOW,
        )
        .await?;
        self.common_write(COMMON_DRIVER_FEATURE_SELECT, pci::AccessWidth::Dword, 1)
            .await?;
        self.common_write(
            COMMON_DRIVER_FEATURE,
            pci::AccessWidth::Dword,
            FEATURE_VERSION_1_HIGH,
        )
        .await?;
        let with_features_ok = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            with_features_ok,
        )
        .await?;
        let status = self
            .common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)
            .await?;
        if status & STATUS_FEATURES_OK == 0 {
            return Err(String::from(
                "net.virtio: the device rejected the negotiated feature set",
            ));
        }

        // The device DMAs into the rings and the receive buffers, so bus mastering must
        // be on before the first buffer is posted.
        pci_call(
            "net.virtio: set-bus-master",
            pci::set_bus_master(&self._device, true),
        )
        .await?;

        // Queues 0 (receive) and 1 (transmit).
        let queues = self
            .common_read(COMMON_NUM_QUEUES, pci::AccessWidth::Word)
            .await?;
        if queues < 2 {
            return Err(format!(
                "net.virtio: the device exposes {queues} virtqueue(s); a net device needs \
                 a receive and a transmit queue"
            ));
        }
        self.rx = self.setup_queue(0, RX_RING_BASE, notify_multiplier).await?;
        self.tx = self.setup_queue(1, TX_RING_BASE, notify_multiplier).await?;

        // Everything is in place: tell the device the driver is live.
        let live = with_features_ok | STATUS_DRIVER_OK;
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, live)
            .await?;

        // The MAC address from the device configuration window (valid because
        // VIRTIO_NET_F_MAC was negotiated).
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = self
                .device_read(index as u64, pci::AccessWidth::Byte)
                .await? as u8;
        }
        self.mac = mac;

        // Receive-event delivery (plan/12 D59): ask the provider for one INTx vector.
        // `unsupported` (or any other failure) means this platform/provider does not
        // route PCI interrupts — `wait-recv` then returns immediately and consumers
        // keep their poll loops, which work everywhere. Interrupt mode also needs the
        // ISR window (reading it clears the device-side cause), so without one the
        // vector is not requested at all.
        if self.isr.is_some() {
            self.interrupt =
                match pci::enable_interrupts(&self._device, pci::InterruptKind::Intx, 1).await {
                    Ok(mut vectors) if !vectors.is_empty() => Some(vectors.remove(0)),
                    _ => None,
                };
        }
        if self.interrupt.is_some() {
            // Un-suppress used-buffer interrupts on the receive ring (setup_queue wrote
            // the suppression hint into both rings): RX completions may now assert the
            // level-triggered INTx, which stays masked at the controller except while a
            // `wait-recv` is parked on it, and is deasserted by the ISR read the
            // consuming paths perform. The transmit ring stays suppressed — its
            // completions are consumed by `send`'s inline poll.
            pci::dma_write(&self.rings, RX_RING_BASE + AVAIL_OFFSET, &0u16.to_le_bytes());
        }

        // Hand the device its receive buffers and open the doorbell once.
        self.post_initial_receive_buffers().await?;

        // One best-effort diagnostic line so a metal session shows what was probed and
        // how receive traffic is observed.
        let handle = text::default();
        let line = format!(
            "net.virtio: virtio-net {}, queues rx/tx {}/{}, rx wait: {}",
            format_mac(&self.mac),
            self.rx.size,
            self.tx.size,
            if self.interrupt.is_some() {
                "INTx interrupt"
            } else {
                "polled"
            },
        );
        let _ = text::write(&handle, text::OutputStream::Out, &line);
        let _ = text::write(&handle, text::OutputStream::Out, "\n");
        Ok(())
    }

    /// Select queue `index`, size it, point its rings at `ring_base` within the ring
    /// page, and enable it.
    async fn setup_queue(
        &mut self,
        index: u16,
        ring_base: u64,
        notify_multiplier: u32,
    ) -> Result<Queue, String> {
        self.common_write(
            COMMON_QUEUE_SELECT,
            pci::AccessWidth::Word,
            u64::from(index),
        )
        .await?;
        let max_size = self
            .common_read(COMMON_QUEUE_SIZE, pci::AccessWidth::Word)
            .await?;
        if max_size == 0 {
            return Err(format!("net.virtio: virtqueue {index} is not available"));
        }
        let size = core::cmp::min(max_size, u64::from(QUEUE_SIZE)) as u16;
        self.common_write(COMMON_QUEUE_SIZE, pci::AccessWidth::Word, u64::from(size))
            .await?;
        // The driver owns ring initialization (virtio 1.0 §3.1.1): zero the descriptor
        // table and both rings so the device's first used-index write is the first
        // non-zero value the polling loops ever observe.
        pci::dma_write(&self.rings, ring_base, &[0u8; 1024]);
        // Suppress used-buffer interrupts on this queue at setup. If bring-up is later
        // granted an INTx vector, the receive ring's flag is cleared again (interrupt
        // mode); the transmit ring stays suppressed (see AVAIL_F_NO_INTERRUPT).
        pci::dma_write(
            &self.rings,
            ring_base + AVAIL_OFFSET,
            &AVAIL_F_NO_INTERRUPT.to_le_bytes(),
        );
        let ring_address = pci::dma_address(&self.rings) + ring_base;
        self.write_address(COMMON_QUEUE_DESC, ring_address + DESC_OFFSET)
            .await?;
        self.write_address(COMMON_QUEUE_DRIVER, ring_address + AVAIL_OFFSET)
            .await?;
        self.write_address(COMMON_QUEUE_DEVICE, ring_address + USED_OFFSET)
            .await?;
        let queue_notify_off = self
            .common_read(COMMON_QUEUE_NOTIFY_OFF, pci::AccessWidth::Word)
            .await?;
        let notify_offset = self.notify.offset + queue_notify_off * u64::from(notify_multiplier);
        self.common_write(COMMON_QUEUE_ENABLE, pci::AccessWidth::Word, 1)
            .await?;
        Ok(Queue {
            notify_offset,
            size,
            avail_index: 0,
            used_index: 0,
        })
    }

    /// Post every receive slot to the device: descriptor `i` covers slot `i`, the avail
    /// ring publishes them all, and one kick tells the device its buffers are there.
    async fn post_initial_receive_buffers(&mut self) -> Result<(), String> {
        let rx_address = pci::dma_address(&self.rx_data);
        for slot in 0..RX_SLOTS {
            self.write_descriptor(
                RX_RING_BASE,
                u64::from(slot),
                rx_address + u64::from(slot) * RX_SLOT_BYTES,
                RX_SLOT_BYTES as u32,
                DESC_F_WRITE,
                0,
            );
            let avail_slot = u64::from(self.rx.avail_index % self.rx.size);
            pci::dma_write(
                &self.rings,
                RX_RING_BASE + AVAIL_OFFSET + 4 + 2 * avail_slot,
                &slot.to_le_bytes(),
            );
            self.rx.avail_index = self.rx.avail_index.wrapping_add(1);
        }
        pci::dma_write(
            &self.rings,
            RX_RING_BASE + AVAIL_OFFSET + 2,
            &self.rx.avail_index.to_le_bytes(),
        );
        self.notify_queue(0, self.rx.notify_offset).await
    }

    // --- register access helpers ----------------------------------------------------------

    fn bar(&self, index: u8) -> Result<&pci::Bar, String> {
        self.bars
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, bar)| bar)
            .ok_or_else(|| String::from("net.virtio: internal error: BAR not opened"))
    }

    async fn common_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.common.bar)?;
        pci_call(
            "net.virtio: common config read",
            pci::bar_read(bar, self.common.offset + register, width),
        )
        .await
    }

    async fn common_write(
        &self,
        register: u64,
        width: pci::AccessWidth,
        value: u64,
    ) -> Result<(), String> {
        let bar = self.bar(self.common.bar)?;
        pci_call(
            "net.virtio: common config write",
            pci::bar_write(bar, self.common.offset + register, width, value),
        )
        .await
    }

    async fn device_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.device_config.bar)?;
        pci_call(
            "net.virtio: device config read",
            pci::bar_read(bar, self.device_config.offset + register, width),
        )
        .await
    }

    /// Write a 64-bit ring address as the two dword halves the common config expects.
    async fn write_address(&self, register: u64, address: u64) -> Result<(), String> {
        self.common_write(register, pci::AccessWidth::Dword, address & 0xffff_ffff)
            .await?;
        self.common_write(register + 4, pci::AccessWidth::Dword, address >> 32)
            .await
    }

    /// Ring the doorbell for queue `index` at its precomputed notify offset.
    async fn notify_queue(&self, index: u16, notify_offset: u64) -> Result<(), String> {
        let bar = self.bar(self.notify.bar)?;
        pci_call(
            "net.virtio: queue notify",
            pci::bar_write(bar, notify_offset, pci::AccessWidth::Word, u64::from(index)),
        )
        .await
    }

    /// Write one 16-byte split-virtqueue descriptor for the queue whose rings start at
    /// `ring_base`.
    fn write_descriptor(
        &self,
        ring_base: u64,
        index: u64,
        address: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let mut descriptor = [0u8; 16];
        descriptor[0..8].copy_from_slice(&address.to_le_bytes());
        descriptor[8..12].copy_from_slice(&len.to_le_bytes());
        descriptor[12..14].copy_from_slice(&flags.to_le_bytes());
        descriptor[14..16].copy_from_slice(&next.to_le_bytes());
        pci::dma_write(
            &self.rings,
            ring_base + DESC_OFFSET + index * 16,
            &descriptor,
        );
    }

    /// The device's current used index for the queue whose rings start at `ring_base`.
    fn used_index(&self, ring_base: u64) -> u16 {
        let raw = pci::dma_read(&self.rings, ring_base + USED_OFFSET + 2, 2);
        u16::from_le_bytes([raw[0], raw[1]])
    }

    // --- frames ------------------------------------------------------------------------------

    /// The single interface this driver exposes.
    fn interface_info(&self) -> InterfaceInfo {
        InterfaceInfo {
            name: String::from(INTERFACE_NAME),
            mac: (
                self.mac[0],
                self.mac[1],
                self.mac[2],
                self.mac[3],
                self.mac[4],
                self.mac[5],
            ),
            mtu: MTU,
            up: true,
        }
    }

    /// Settle a transmit a *cancelled* `send-frame` left published before the shared
    /// bounce buffer and descriptor slot are reused. A cancellation (the operation's
    /// future dropped mid-await — reachable through any pci provider that defers, e.g.
    /// an interposed `pci.filtered`) can land in `send`'s notify await: at that point
    /// the descriptor is published — possibly unkicked — and the device may transmit it
    /// at the next doorbell, reading whatever the bounce buffer holds *then*. Without
    /// this drain, the next `send` would overwrite the bounce buffer (putting a
    /// corrupted copy of its own frame on the wire under the cancelled transmit's
    /// descriptor) and consume the stale completion as its own, leaving the consumed
    /// cursor permanently one behind the device. The cursor pair makes it visible:
    /// `tx.avail_index` counts published transmits, `tx.used_index` consumed
    /// completions, level between healthy operations. On divergence: kick (idempotent),
    /// poll the leftover completion out with the normal transmit bound, discard it.
    ///
    /// The receive queue needs no analogue: its consumption path is await-free (the
    /// used element is read, the cursor advanced, and the slot re-published entirely
    /// with synchronous DMA accesses before the only await, the re-post doorbell), so a
    /// cancellation there can only lose a kick — and the published re-post stays in the
    /// avail ring, where the next doorbell re-delivers it.
    async fn drain_tx(&mut self) -> Result<(), L2Fail> {
        let mut spins: u64 = 0;
        while self.tx.avail_index != self.tx.used_index {
            self.notify_queue(1, self.tx.notify_offset)
                .await
                .map_err(L2Fail::Io)?;
            while self.used_index(TX_RING_BASE) == self.tx.used_index {
                spins += 1;
                if spins > TX_POLL_LIMIT {
                    return Err(L2Fail::Io(String::from(
                        "net.virtio: the device did not complete a cancelled transmit \
                         (poll limit)",
                    )));
                }
            }
            self.tx.used_index = self.tx.used_index.wrapping_add(1);
        }
        Ok(())
    }

    /// Transmit one Ethernet frame: virtio-net header (zeroed — no offloads) + frame,
    /// one descriptor, kick, poll the used ring for the device to consume it.
    async fn send(&mut self, frame: &[u8]) -> Result<u64, L2Fail> {
        let frame_len = frame.len() as u64;
        if frame_len > MAX_FRAME {
            return Err(L2Fail::FrameTooLarge);
        }
        self.drain_tx().await?;
        let mut packet = vec![0u8; VNET_HEADER as usize];
        packet.extend_from_slice(frame);
        pci::dma_write(&self.tx_data, 0, &packet);

        let descriptor_index = u64::from(self.tx.avail_index % self.tx.size);
        self.write_descriptor(
            TX_RING_BASE,
            descriptor_index,
            pci::dma_address(&self.tx_data),
            packet.len() as u32,
            0,
            0,
        );
        let avail_slot = u64::from(self.tx.avail_index % self.tx.size);
        pci::dma_write(
            &self.rings,
            TX_RING_BASE + AVAIL_OFFSET + 4 + 2 * avail_slot,
            &(descriptor_index as u16).to_le_bytes(),
        );
        self.tx.avail_index = self.tx.avail_index.wrapping_add(1);
        pci::dma_write(
            &self.rings,
            TX_RING_BASE + AVAIL_OFFSET + 2,
            &self.tx.avail_index.to_le_bytes(),
        );
        self.notify_queue(1, self.tx.notify_offset)
            .await
            .map_err(L2Fail::Io)?;

        let mut spins: u64 = 0;
        while self.used_index(TX_RING_BASE) == self.tx.used_index {
            spins += 1;
            if spins > TX_POLL_LIMIT {
                return Err(L2Fail::Io(String::from(
                    "net.virtio: the device did not consume the transmitted frame (poll limit)",
                )));
            }
        }
        self.tx.used_index = self.tx.used_index.wrapping_add(1);
        Ok(frame_len)
    }

    /// Receive the next delivered frame (header stripped), truncated to `max_len`
    /// bytes, re-posting the receive buffer afterwards. A short poll that finds nothing
    /// returns an empty frame ("nothing waiting right now") so the consumer decides how
    /// long to keep waiting; runts and unusable completions also come back empty (they
    /// are wire noise, not driver failures).
    async fn recv(&mut self, max_len: u64) -> Result<Vec<u8>, L2Fail> {
        let mut spins: u64 = 0;
        while self.used_index(RX_RING_BASE) == self.rx.used_index {
            spins += 1;
            if spins > RX_POLL_LIMIT {
                return Ok(Vec::new());
            }
        }

        // Read the used element this completion corresponds to: which descriptor (and
        // therefore which receive slot) and how many bytes the device wrote.
        let used_slot = u64::from(self.rx.used_index % self.rx.size);
        let element = pci::dma_read(
            &self.rings,
            RX_RING_BASE + USED_OFFSET + 4 + 8 * used_slot,
            8,
        );
        let id = u32::from_le_bytes([element[0], element[1], element[2], element[3]]);
        let written = u64::from(u32::from_le_bytes([
            element[4], element[5], element[6], element[7],
        ]));
        self.rx.used_index = self.rx.used_index.wrapping_add(1);

        let bytes = if id as u64 >= u64::from(RX_SLOTS) || written <= VNET_HEADER {
            // A slot we never posted, or a runt the header alone fills: drop it.
            Vec::new()
        } else {
            let frame_len = core::cmp::min(written - VNET_HEADER, max_len);
            let frame_len = core::cmp::min(frame_len, RX_SLOT_BYTES - VNET_HEADER);
            pci::dma_read(
                &self.rx_data,
                u64::from(id) * RX_SLOT_BYTES + VNET_HEADER,
                frame_len,
            )
        };

        // Hand the slot straight back to the device.
        if (id as u64) < u64::from(RX_SLOTS) {
            let avail_slot = u64::from(self.rx.avail_index % self.rx.size);
            pci::dma_write(
                &self.rings,
                RX_RING_BASE + AVAIL_OFFSET + 4 + 2 * avail_slot,
                &(id as u16).to_le_bytes(),
            );
            self.rx.avail_index = self.rx.avail_index.wrapping_add(1);
            pci::dma_write(
                &self.rings,
                RX_RING_BASE + AVAIL_OFFSET + 2,
                &self.rx.avail_index.to_le_bytes(),
            );
            self.notify_queue(0, self.rx.notify_offset)
                .await
                .map_err(L2Fail::Io)?;
        }

        // Interrupt mode: the delivery just consumed set the device's ISR bit (and may
        // hold the level-triggered line asserted, masked at the controller). Read it
        // away now so the next `wait-recv` arms on a clean line instead of taking one
        // spurious wake. Safe against losing events: a frame that lands between this
        // consume and the ISR read re-raises the ISR *and* sits in the used ring, and
        // `wait_rx` checks the ring before ever parking.
        if self.interrupt.is_some() {
            self.acknowledge_isr().await;
        }

        // An unusable completion (a slot we never posted, or a runt the virtio-net
        // header alone fills) is wire noise: report it the same way as "nothing
        // waiting" and let the consumer poll again.
        Ok(bytes)
    }

    /// Park until the device's RX interrupt reports receive work, the caller's bound
    /// (clamped by the provider) expires, or — with no vector granted — immediately:
    /// the `wait-recv` arm of the l2 surface (plan/12 D59). Never parks over work
    /// already delivered: the used ring is checked first.
    ///
    /// Liveness discipline: the bound is a backstop, not the delivery path. If it
    /// expires and the used ring advanced *during* the wait, the event path missed an
    /// edge — that is reported loudly (a `liveness:` line the check gates assert
    /// against), never silently rescued by the re-poll. (A frame can land in the
    /// microseconds between the provider masking the line at expiry and the ring check
    /// here; that benign race is why the line is rate-limited rather than fatal.)
    async fn wait_rx(&mut self, max_ns: u64) -> Result<(), L2Fail> {
        if self.used_index(RX_RING_BASE) != self.rx.used_index {
            return Ok(());
        }
        let Some(vector) = self.interrupt.take() else {
            // No interrupt routing on this platform/provider: the documented poll
            // fallback — return immediately, the consumer keeps polling.
            return Ok(());
        };
        let outcome = pci::wait(&vector, max_ns).await;
        self.interrupt = Some(vector);
        match outcome {
            // A delivery (possibly coalesced, possibly a shared-line sibling's): clear
            // the ISR so the line deasserts, and let the caller re-poll the ring.
            Ok(_deliveries) => self.acknowledge_isr().await,
            // Bound expiry (or any wait failure): nothing to do — unless receive work
            // arrived without a delivery, which is a missed event, reported loudly.
            Err(_) => {
                if self.used_index(RX_RING_BASE) != self.rx.used_index {
                    self.missed_intx = self.missed_intx.wrapping_add(1);
                    if self.missed_intx == 1 || self.missed_intx % LIVENESS_REPORT_EVERY == 0 {
                        let handle = text::default();
                        let line = format!(
                            "liveness: net.virtio rx frame present after a bounded \
                             interrupt wait expired (missed INTx; occurrence {})",
                            self.missed_intx
                        );
                        let _ = text::write(&handle, text::OutputStream::Out, &line);
                        let _ = text::write(&handle, text::OutputStream::Out, "\n");
                    }
                }
            }
        }
        Ok(())
    }

    /// Read (and thereby clear) the device's ISR status register, deasserting its INTx
    /// line. Best-effort: a failure here only risks one spurious re-delivery.
    async fn acknowledge_isr(&self) {
        if let Some(isr) = &self.isr
            && let Ok(bar) = self.bar(isr.bar)
        {
            let _ = pci::bar_read(bar, isr.offset, pci::AccessWidth::Byte).await;
        }
    }
}

/// `aa:bb:cc:dd:ee:ff`.
fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

// ------------------------------------------------------------------------------------------
// Capability-window discovery (the vendor-specific PCI capabilities).
// ------------------------------------------------------------------------------------------

/// One capability-list configuration-space read.
async fn config_read(
    device: &pci::Device,
    offset: u32,
    width: pci::AccessWidth,
) -> Result<u64, String> {
    pci_call(
        "net.virtio: config read",
        pci::config_read(device, offset, width),
    )
    .await
}

/// Walk the configuration-space capability list and return the common, notify (plus its
/// multiplier), and device-config windows, plus the ISR window when the device has one
/// (it always does on QEMU; interrupt mode needs it to deassert INTx).
async fn find_windows(
    device: &pci::Device,
) -> Result<(Region, Region, u32, Region, Option<Region>), String> {
    let mut common: Option<Region> = None;
    let mut notify: Option<(Region, u32)> = None;
    let mut device_config: Option<Region> = None;
    let mut isr: Option<Region> = None;

    let mut pointer =
        (config_read(device, PCI_CAP_POINTER, pci::AccessWidth::Byte).await? & 0xfc) as u32;
    let mut steps = 0;
    while pointer != 0 && steps < PCI_CAP_WALK_LIMIT {
        steps += 1;
        let id = config_read(device, pointer, pci::AccessWidth::Byte).await?;
        let next = (config_read(device, pointer + 1, pci::AccessWidth::Byte).await? & 0xfc) as u32;
        if id == PCI_CAP_ID_VENDOR {
            let cfg_type = config_read(device, pointer + 3, pci::AccessWidth::Byte).await?;
            let bar = config_read(device, pointer + 4, pci::AccessWidth::Byte).await? as u8;
            let offset = config_read(device, pointer + 8, pci::AccessWidth::Dword).await?;
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON if common.is_none() => {
                    common = Some(Region { bar, offset });
                }
                VIRTIO_PCI_CAP_NOTIFY if notify.is_none() => {
                    let multiplier =
                        config_read(device, pointer + 16, pci::AccessWidth::Dword).await? as u32;
                    notify = Some((Region { bar, offset }, multiplier));
                }
                VIRTIO_PCI_CAP_ISR if isr.is_none() => {
                    isr = Some(Region { bar, offset });
                }
                VIRTIO_PCI_CAP_DEVICE if device_config.is_none() => {
                    device_config = Some(Region { bar, offset });
                }
                _ => {}
            }
        }
        pointer = next;
    }

    let common = common.ok_or_else(|| {
        String::from("net.virtio: the function has no virtio common-config capability")
    })?;
    let (notify, multiplier) = notify
        .ok_or_else(|| String::from("net.virtio: the function has no virtio notify capability"))?;
    let device_config = device_config.ok_or_else(|| {
        String::from("net.virtio: the function has no virtio device-config capability")
    })?;
    Ok((common, notify, multiplier, device_config, isr))
}

// ------------------------------------------------------------------------------------------
// The exported eo9:net/l2 provider
// ------------------------------------------------------------------------------------------

/// The `net.virtio` provider.
struct Stub;

/// The root-handle resource: a token referring to the claimed and brought-up device.
struct VirtioL2;

/// The opened-interface resource: a token — the device state lives in [`STATE`].
struct VirtioInterface;

impl l2::GuestL2Impl for VirtioL2 {}
impl l2::GuestL2Interface for VirtioInterface {}

impl l2::Guest for Stub {
    type L2Impl = VirtioL2;
    type L2Interface = VirtioInterface;

    fn default() -> l2::L2Impl {
        l2::L2Impl::new(VirtioL2)
    }

    async fn list_interfaces(_l2: l2::L2ImplBorrow<'_>) -> Result<Vec<InterfaceInfo>, L2Error> {
        match acquire_driver().await {
            Ok(driver) => Ok(alloc::vec![driver.interface_info()]),
            Err(fail) => Err(L2Error::from(fail)),
        }
    }

    async fn open_interface(
        _l2: l2::L2ImplBorrow<'_>,
        name: String,
    ) -> Result<l2::L2Interface, L2Error> {
        let _driver = acquire_driver().await.map_err(L2Error::from)?;
        if name.is_empty() || name == INTERFACE_NAME {
            Ok(l2::L2Interface::new(VirtioInterface))
        } else {
            Err(L2Error::NoSuchInterface)
        }
    }

    fn info(_iface: l2::L2InterfaceBorrow<'_>) -> InterfaceInfo {
        // An opened interface implies the driver is up (open-interface brought it up);
        // `info` is a sync WIT function, so it reads the resting driver from its slot
        // and reports the link down rather than trapping if the state is unavailable
        // (mid-operation, or something went sideways since).
        let resting = if STATE.is_set() {
            STATE.with(|slot| slot.driver.as_ref().map(Driver::interface_info))
        } else {
            None
        };
        resting.unwrap_or(InterfaceInfo {
            name: String::from(INTERFACE_NAME),
            mac: (0, 0, 0, 0, 0, 0),
            mtu: 0,
            up: false,
        })
    }

    async fn send_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        frame: Buffer,
    ) -> (Buffer, Result<SendResult, L2Error>) {
        let len = frame.len();
        // Copy out of the buffer before driving the device so no buffer call interleaves
        // with the request (same discipline as disk.virtio).
        let bytes = if len == 0 {
            Vec::new()
        } else {
            frame.read(0, len)
        };
        let mut driver = match acquire_driver().await {
            Ok(driver) => driver,
            Err(fail) => return (frame, Err(L2Error::from(fail))),
        };
        match driver.send(&bytes).await {
            Ok(bytes_sent) => (frame, Ok(SendResult { bytes_sent })),
            Err(fail) => (frame, Err(L2Error::from(fail))),
        }
    }

    async fn wait_recv(
        _iface: l2::L2InterfaceBorrow<'_>,
        max_wait_ns: u64,
    ) -> Result<(), L2Error> {
        let mut driver = acquire_driver().await.map_err(L2Error::from)?;
        driver.wait_rx(max_wait_ns).await.map_err(L2Error::from)
    }

    async fn recv_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L2Error>) {
        let capacity = dst.len();
        let mut driver = match acquire_driver().await {
            Ok(driver) => driver,
            Err(fail) => return (dst, Err(L2Error::from(fail))),
        };
        match driver.recv(capacity).await {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    dst.write(0, &bytes);
                }
                (
                    dst,
                    Ok(RecvResult {
                        bytes_received: bytes.len() as u64,
                    }),
                )
            }
            Err(fail) => (dst, Err(L2Error::from(fail))),
        }
    }
}

export!(Stub);
