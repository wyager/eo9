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
//!   frame out, and immediately re-posts the buffer. The kernel's PCI provider does not
//!   deliver interrupts yet — and virtio is fine with polling.
//!
//! The exported `eo9:net/l2` surface is the single interface `virtio0`: `recv-frame`
//! that finds nothing within its (bounded) poll reports a typed `io` error rather than
//! blocking forever, a frame larger than the transmit buffer fails with
//! `frame-too-large`, and device weirdness is always a typed error, never a trap.
//!
//! Like `disk.virtio`, the driver drives its `eo9:pci` imports eagerly (they are plain
//! MMIO / memory operations in the provider), so the exported operations complete in a
//! single poll and the driver composes under consumers that poll their l2 import
//! eagerly. The documented default state (no configure interface) is "claim the first
//! virtio-net function on first use"; first use also prints one `net.virtio: …`
//! diagnostic line so a metal session shows what was probed.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

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
/// Receive polling bound: how long `recv-frame` waits for the next frame before
/// reporting a typed `io` error (a few seconds of host calls — long enough for the
/// reply-to-our-request flows the l2 surface is used for, short enough never to look
/// like a hang).
const RX_POLL_LIMIT: u64 = 2_000_000;

// ------------------------------------------------------------------------------------------
// Eager driving of the async pci imports (same pattern and reasoning as disk.virtio).
// ------------------------------------------------------------------------------------------

/// Drive an async import call that completes without suspending. Every `eo9:pci`
/// operation the driver uses is plain MMIO / memory work in the provider, so a single
/// poll completes it; a provider that genuinely suspends makes the operation fail with
/// an `io` error rather than blocking the consumer's eager poll of *us*.
fn poll_eager<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// Run one PCI operation eagerly and flatten its result, labelling failures with `what`.
fn pci_call<T>(
    what: &str,
    future: impl Future<Output = Result<T, pci::PciError>>,
) -> Result<T, String> {
    match poll_eager(future) {
        None => Err(format!("{what}: the pci provider suspended")),
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(format!("{what}: {error:?}")),
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
    NoSuchInterface,
    FrameTooLarge,
    Io(String),
}

impl From<L2Fail> for L2Error {
    fn from(fail: L2Fail) -> L2Error {
        match fail {
            L2Fail::NoSuchInterface => L2Error::NoSuchInterface,
            L2Fail::FrameTooLarge => L2Error::FrameTooLarge,
            L2Fail::Io(message) => L2Error::Io(message),
        }
    }
}

static STATE: ProviderState<Driver> = ProviderState::new();

/// Run `f` over the brought-up driver, probing and initializing the device on first use
/// (the documented default state — there is no configure interface).
fn with_driver<R>(f: impl FnOnce(&mut Driver) -> Result<R, L2Fail>) -> Result<R, L2Fail> {
    if !STATE.is_set() {
        match Driver::bring_up() {
            Ok(driver) => STATE.set(driver),
            Err(message) => return Err(L2Fail::Io(message)),
        }
    }
    STATE.with(f)
}

impl Driver {
    /// Find, claim, and bring up the first virtio-net function visible through the
    /// granted PCI capability. Every step reports a typed, labelled error — device
    /// weirdness is an `io` failure of the l2 operation, never a trap.
    fn bring_up() -> Result<Driver, String> {
        let root = pci::default();
        let devices = pci_call("net.virtio: enumerate", pci::enumerate(&root))?;
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
        let device = pci_call("net.virtio: open", pci::open(&root, address))?;

        // Walk the vendor-specific capabilities to find the virtio register windows.
        let (common, notify_base, notify_multiplier, device_config) = find_windows(&device)?;

        // Open each BAR the windows live in exactly once.
        let mut bar_indices: Vec<u8> = Vec::new();
        for index in [common.bar, notify_base.bar, device_config.bar] {
            if !bar_indices.contains(&index) {
                bar_indices.push(index);
            }
        }
        let mut bars: Vec<(u8, pci::Bar)> = Vec::new();
        for index in bar_indices {
            let bar = pci_call("net.virtio: open-bar", pci::open_bar(&device, index))?;
            bars.push((index, bar));
        }

        // DMA buffers: one page holding both queues' rings, the receive slots, and the
        // transmit bounce buffer. CPU address == device address under the kernel's
        // identity map; the provider hands back the device-visible address via
        // `dma-address`.
        let rings = pci_call(
            "net.virtio: alloc-dma (rings)",
            pci::alloc_dma(&device, RING_BYTES),
        )?;
        let rx_data = pci_call(
            "net.virtio: alloc-dma (receive buffers)",
            pci::alloc_dma(&device, RX_DATA_BYTES),
        )?;
        let tx_data = pci_call(
            "net.virtio: alloc-dma (transmit buffer)",
            pci::alloc_dma(&device, TX_DATA_BYTES),
        )?;

        let mut driver = Driver {
            _device: device,
            bars,
            common,
            device_config,
            notify: notify_base,
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
        driver.start(notify_multiplier)?;
        Ok(driver)
    }

    /// Negotiate features, build both virtqueues, read the MAC, and pre-post the
    /// receive buffers — the device side of bring-up, once the function is claimed and
    /// the DMA buffers exist.
    fn start(&mut self, notify_multiplier: u32) -> Result<(), String> {
        // Reset, then ACKNOWLEDGE and DRIVER.
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, 0)?;
        let mut spins = 0u32;
        while self.common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)? != 0 {
            spins += 1;
            if spins > 1000 {
                return Err(String::from("net.virtio: device did not reset"));
            }
        }
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            STATUS_ACKNOWLEDGE,
        )?;
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER,
        )?;

        // Feature negotiation: VIRTIO_F_VERSION_1 is required (it is what makes the
        // modern register layout valid at all); VIRTIO_NET_F_MAC is required so the
        // device-config window carries a stable MAC address (QEMU always offers it).
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 0)?;
        let low_features = self.common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)?;
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 1)?;
        let high_features = self.common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)?;
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
        self.common_write(COMMON_DRIVER_FEATURE_SELECT, pci::AccessWidth::Dword, 0)?;
        self.common_write(
            COMMON_DRIVER_FEATURE,
            pci::AccessWidth::Dword,
            FEATURE_MAC_LOW,
        )?;
        self.common_write(COMMON_DRIVER_FEATURE_SELECT, pci::AccessWidth::Dword, 1)?;
        self.common_write(
            COMMON_DRIVER_FEATURE,
            pci::AccessWidth::Dword,
            FEATURE_VERSION_1_HIGH,
        )?;
        let with_features_ok = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            with_features_ok,
        )?;
        let status = self.common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)?;
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
        )?;

        // Queues 0 (receive) and 1 (transmit).
        let queues = self.common_read(COMMON_NUM_QUEUES, pci::AccessWidth::Word)?;
        if queues < 2 {
            return Err(format!(
                "net.virtio: the device exposes {queues} virtqueue(s); a net device needs \
                 a receive and a transmit queue"
            ));
        }
        self.rx = self.setup_queue(0, RX_RING_BASE, notify_multiplier)?;
        self.tx = self.setup_queue(1, TX_RING_BASE, notify_multiplier)?;

        // Everything is in place: tell the device the driver is live.
        let live = with_features_ok | STATUS_DRIVER_OK;
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, live)?;

        // The MAC address from the device configuration window (valid because
        // VIRTIO_NET_F_MAC was negotiated).
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = self.device_read(index as u64, pci::AccessWidth::Byte)? as u8;
        }
        self.mac = mac;

        // Hand the device its receive buffers and open the doorbell once.
        self.post_initial_receive_buffers()?;

        // One best-effort diagnostic line so a metal session shows what was probed.
        let handle = text::default();
        let line = format!(
            "net.virtio: virtio-net {}, queues rx/tx {}/{}",
            format_mac(&self.mac),
            self.rx.size,
            self.tx.size,
        );
        let _ = text::write(&handle, text::OutputStream::Out, &line);
        let _ = text::write(&handle, text::OutputStream::Out, "\n");
        Ok(())
    }

    /// Select queue `index`, size it, point its rings at `ring_base` within the ring
    /// page, and enable it.
    fn setup_queue(
        &mut self,
        index: u16,
        ring_base: u64,
        notify_multiplier: u32,
    ) -> Result<Queue, String> {
        self.common_write(
            COMMON_QUEUE_SELECT,
            pci::AccessWidth::Word,
            u64::from(index),
        )?;
        let max_size = self.common_read(COMMON_QUEUE_SIZE, pci::AccessWidth::Word)?;
        if max_size == 0 {
            return Err(format!("net.virtio: virtqueue {index} is not available"));
        }
        let size = core::cmp::min(max_size, u64::from(QUEUE_SIZE)) as u16;
        self.common_write(COMMON_QUEUE_SIZE, pci::AccessWidth::Word, u64::from(size))?;
        // The driver owns ring initialization (virtio 1.0 §3.1.1): zero the descriptor
        // table and both rings so the device's first used-index write is the first
        // non-zero value the polling loops ever observe.
        pci::dma_write(&self.rings, ring_base, &[0u8; 1024]);
        let ring_address = pci::dma_address(&self.rings) + ring_base;
        self.write_address(COMMON_QUEUE_DESC, ring_address + DESC_OFFSET)?;
        self.write_address(COMMON_QUEUE_DRIVER, ring_address + AVAIL_OFFSET)?;
        self.write_address(COMMON_QUEUE_DEVICE, ring_address + USED_OFFSET)?;
        let queue_notify_off = self.common_read(COMMON_QUEUE_NOTIFY_OFF, pci::AccessWidth::Word)?;
        let notify_offset = self.notify.offset + queue_notify_off * u64::from(notify_multiplier);
        self.common_write(COMMON_QUEUE_ENABLE, pci::AccessWidth::Word, 1)?;
        Ok(Queue {
            notify_offset,
            size,
            avail_index: 0,
            used_index: 0,
        })
    }

    /// Post every receive slot to the device: descriptor `i` covers slot `i`, the avail
    /// ring publishes them all, and one kick tells the device its buffers are there.
    fn post_initial_receive_buffers(&mut self) -> Result<(), String> {
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
        self.notify_queue(0, self.rx.notify_offset)
    }

    // --- register access helpers ----------------------------------------------------------

    fn bar(&self, index: u8) -> Result<&pci::Bar, String> {
        self.bars
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, bar)| bar)
            .ok_or_else(|| String::from("net.virtio: internal error: BAR not opened"))
    }

    fn common_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.common.bar)?;
        pci_call(
            "net.virtio: common config read",
            pci::bar_read(bar, self.common.offset + register, width),
        )
    }

    fn common_write(
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
    }

    fn device_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.device_config.bar)?;
        pci_call(
            "net.virtio: device config read",
            pci::bar_read(bar, self.device_config.offset + register, width),
        )
    }

    /// Write a 64-bit ring address as the two dword halves the common config expects.
    fn write_address(&self, register: u64, address: u64) -> Result<(), String> {
        self.common_write(register, pci::AccessWidth::Dword, address & 0xffff_ffff)?;
        self.common_write(register + 4, pci::AccessWidth::Dword, address >> 32)
    }

    /// Ring the doorbell for queue `index` at its precomputed notify offset.
    fn notify_queue(&self, index: u16, notify_offset: u64) -> Result<(), String> {
        let bar = self.bar(self.notify.bar)?;
        pci_call(
            "net.virtio: queue notify",
            pci::bar_write(bar, notify_offset, pci::AccessWidth::Word, u64::from(index)),
        )
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

    /// Transmit one Ethernet frame: virtio-net header (zeroed — no offloads) + frame,
    /// one descriptor, kick, poll the used ring for the device to consume it.
    fn send(&mut self, frame: &[u8]) -> Result<u64, L2Fail> {
        let frame_len = frame.len() as u64;
        if frame_len > MAX_FRAME {
            return Err(L2Fail::FrameTooLarge);
        }
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
    /// bytes, re-posting the receive buffer afterwards. A bounded poll that finds
    /// nothing reports a typed `io` error rather than blocking forever.
    fn recv(&mut self, max_len: u64) -> Result<Vec<u8>, L2Fail> {
        let mut spins: u64 = 0;
        while self.used_index(RX_RING_BASE) == self.rx.used_index {
            spins += 1;
            if spins > RX_POLL_LIMIT {
                return Err(L2Fail::Io(String::from(
                    "net.virtio: no frame arrived within the receive poll bound",
                )));
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
                .map_err(L2Fail::Io)?;
        }

        if bytes.is_empty() {
            return Err(L2Fail::Io(String::from(
                "net.virtio: the device delivered an unusable receive completion",
            )));
        }
        Ok(bytes)
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

/// Walk the configuration-space capability list and return the common, notify (plus its
/// multiplier), and device-config windows.
fn find_windows(device: &pci::Device) -> Result<(Region, Region, u32, Region), String> {
    let read = |offset: u32, width: pci::AccessWidth| -> Result<u64, String> {
        pci_call(
            "net.virtio: config read",
            pci::config_read(device, offset, width),
        )
    };

    let mut common: Option<Region> = None;
    let mut notify: Option<(Region, u32)> = None;
    let mut device_config: Option<Region> = None;

    let mut pointer = (read(PCI_CAP_POINTER, pci::AccessWidth::Byte)? & 0xfc) as u32;
    let mut steps = 0;
    while pointer != 0 && steps < PCI_CAP_WALK_LIMIT {
        steps += 1;
        let id = read(pointer, pci::AccessWidth::Byte)?;
        let next = (read(pointer + 1, pci::AccessWidth::Byte)? & 0xfc) as u32;
        if id == PCI_CAP_ID_VENDOR {
            let cfg_type = read(pointer + 3, pci::AccessWidth::Byte)?;
            let bar = read(pointer + 4, pci::AccessWidth::Byte)? as u8;
            let offset = read(pointer + 8, pci::AccessWidth::Dword)?;
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON if common.is_none() => {
                    common = Some(Region { bar, offset });
                }
                VIRTIO_PCI_CAP_NOTIFY if notify.is_none() => {
                    let multiplier = read(pointer + 16, pci::AccessWidth::Dword)? as u32;
                    notify = Some((Region { bar, offset }, multiplier));
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
    Ok((common, notify, multiplier, device_config))
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
        with_driver(|driver| Ok(alloc::vec![driver.interface_info()])).map_err(L2Error::from)
    }

    async fn open_interface(
        _l2: l2::L2ImplBorrow<'_>,
        name: String,
    ) -> Result<l2::L2Interface, L2Error> {
        with_driver(|driver| {
            if name.is_empty() || name == INTERFACE_NAME {
                let _ = driver;
                Ok(())
            } else {
                Err(L2Fail::NoSuchInterface)
            }
        })
        .map_err(L2Error::from)?;
        Ok(l2::L2Interface::new(VirtioInterface))
    }

    fn info(_iface: l2::L2InterfaceBorrow<'_>) -> InterfaceInfo {
        // An opened interface implies the driver is up; if anything went sideways since,
        // report the link down rather than trapping.
        with_driver(|driver| Ok(driver.interface_info())).unwrap_or(InterfaceInfo {
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
        match with_driver(|driver| driver.send(&bytes)) {
            Ok(bytes_sent) => (frame, Ok(SendResult { bytes_sent })),
            Err(fail) => (frame, Err(L2Error::from(fail))),
        }
    }

    async fn recv_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L2Error>) {
        let capacity = dst.len();
        match with_driver(|driver| driver.recv(capacity)) {
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
