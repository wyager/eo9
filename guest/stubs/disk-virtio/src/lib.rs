//! `disk.virtio` — a virtio-blk driver as an ordinary wasm component.
//!
//! Targets the crate-local `eo9:disk-virtio/virtio` world: imports the PCI capability
//! (`eo9:pci/pci`) plus `eo9:text/text` for one diagnostic line, and exports
//! `eo9:disk/disk` backed by a modern (virtio 1.0, `disable-legacy=on`) virtio-blk PCI
//! function. The driver holds no policy of its own: which functions it can see (and
//! therefore claim) is entirely the PCI provider's business — the kernel root only when
//! the boot granted `pci`, an attenuating `pci.filtered` for "exactly this one device"
//! grants, `pci.deny` to refuse.
//!
//! Shape of the device conversation (virtio 1.0 over PCI, plan/12 Decision 43(e)):
//!
//! * **Probe.** Enumerate the capability's view of the bus, claim the first virtio-blk
//!   function (vendor 0x1af4, modern device id 0x1042; the transitional id 0x1001 is
//!   accepted when it carries the modern capabilities), and walk its vendor-specific
//!   PCI capabilities to find the common / notify / device-config register windows and
//!   which BAR each lives in.
//! * **Bring-up.** Reset, ACKNOWLEDGE → DRIVER, negotiate exactly `VIRTIO_F_VERSION_1`,
//!   FEATURES_OK (verified by reading it back), enable bus mastering, build one split
//!   virtqueue (16 entries) in DMA buffers obtained from `alloc-dma`, DRIVER_OK, and
//!   read the capacity from the device config window.
//! * **I/O.** One request at a time: a three-descriptor chain (16-byte request header,
//!   the data buffer, the 1-byte status), published through the avail ring, kicked via
//!   the notify register, completion observed by polling the used ring (the kernel's
//!   PCI provider does not deliver interrupts yet — and virtio is fine with polling).
//!
//! The exported `eo9:disk` operations are byte-addressed; the device is sector
//! addressed (512 bytes). Reads fetch the covering sectors and copy out the requested
//! range; writes read–modify–write the partial head/tail sectors and write aligned
//! sectors directly. As everywhere else in the disk contract, an access whose range
//! does not lie entirely within the device fails with `out-of-range` (no partial I/O),
//! and a zero-length access at any offset up to the capacity succeeds.
//!
//! Like `fs.eofs`, the driver drives its imports eagerly: every `eo9:pci` operation it
//! uses completes without suspending on the kernel (they are plain MMIO / memory
//! operations), so the exported `read`/`write` complete in a single poll and the driver
//! composes under consumers that poll their disk import eagerly. The documented default
//! state (no configure interface) is "claim the first virtio-blk function on first
//! use"; first use also prints one `disk.virtio: …` diagnostic line so a metal session
//! shows what was probed.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "virtio",
    path: "wit",
    // Pull in bindings for eo9:pci/types, eo9:disk/types, and eo9:io/buffers, which the
    // imported and exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::pci::pci;
use eo9::text::text;
use exports::eo9::disk::disk::{self, Buffer, ReadError, ReadResult, WriteError, WriteResult};
use exports::eo9::disk::types;

// ------------------------------------------------------------------------------------------
// Constants: PCI configuration space, virtio-pci capabilities, the common config window,
// the split virtqueue, and virtio-blk requests. All multi-byte device fields are
// little-endian (virtio 1.0 §4.1; both supported kernels are little-endian).
// ------------------------------------------------------------------------------------------

/// virtio vendor id.
const VIRTIO_VENDOR: u16 = 0x1af4;
/// Modern (virtio 1.0+) virtio-blk device id (0x1040 + device type 2).
const VIRTIO_BLK_MODERN: u16 = 0x1042;
/// Transitional virtio-blk device id; accepted only if it carries the modern capabilities.
const VIRTIO_BLK_TRANSITIONAL: u16 = 0x1001;

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

/// virtio-blk sector size (fixed by the spec).
const SECTOR: u64 = 512;
/// virtio-blk request types.
const BLK_T_IN: u32 = 0; // device-to-driver: our disk read
const BLK_T_OUT: u32 = 1; // driver-to-device: our disk write
/// Request status byte values; anything non-zero is a device-reported failure.
const BLK_S_OK: u8 = 0;

/// Split-virtqueue descriptor flags.
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// Queue size the driver uses (the device's maximum is reduced to this; power of two).
const QUEUE_SIZE: u16 = 16;
/// Ring DMA buffer layout (one page): descriptor table, then the avail ring, then the
/// used ring — alignments 16 / 2 / 4 are all satisfied by these offsets.
const RING_BYTES: u64 = 4096;
const DESC_OFFSET: u64 = 0; // 16 bytes * 16 entries = 256
const AVAIL_OFFSET: u64 = 256; // 6 + 2 * 16 = 38
const USED_OFFSET: u64 = 512; // 6 + 8 * 16 = 134
/// Request header + status DMA buffer layout (one page).
const REQ_BYTES: u64 = 4096;
const REQ_HEADER_OFFSET: u64 = 0; // 16 bytes
const REQ_STATUS_OFFSET: u64 = 16; // 1 byte
/// Data DMA buffer: one bounce buffer reused for every request (128 sectors per request).
const DATA_BYTES: u64 = 64 * 1024;

/// Used-ring polling bound. Each iteration is a host call (a DMA-buffer read), so this
/// is minutes of wall clock even on a slow machine — hitting it means the device is not
/// completing requests at all, which is reported as an `io` error rather than hanging.
const POLL_LIMIT: u64 = 50_000_000;

// ------------------------------------------------------------------------------------------
// Eager driving of the async pci imports (same pattern and reasoning as fs.eofs).
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

/// The brought-up device: claimed function, opened BARs, the virtqueue, and the DMA
/// buffers every request reuses.
struct Driver {
    /// Keeps the exclusive claim on the function alive for the component's lifetime.
    _device: pci::Device,
    /// Opened BARs, one handle per distinct BAR index the capabilities referenced.
    bars: Vec<(u8, pci::Bar)>,
    common: Region,
    device_config: Region,
    /// Base of the notify window plus this queue's precomputed notify offset.
    notify: Region,
    notify_offset: u64,
    ring: pci::DmaBuffer,
    request: pci::DmaBuffer,
    data: pci::DmaBuffer,
    queue_size: u16,
    /// Next avail-ring index (free-running, wraps at 65536 like the device's view).
    avail_index: u16,
    /// Used-ring entries consumed so far (free-running).
    used_index: u16,
    capacity_bytes: u64,
}

/// Failures of the byte-addressed disk operations, mapped to the WIT error variants by
/// the export glue.
enum DiskFail {
    OutOfRange,
    Io(String),
}

static STATE: ProviderState<Driver> = ProviderState::new();

/// Run `f` over the brought-up driver, probing and initializing the device on first use
/// (the documented default state — there is no configure interface).
fn with_driver<R>(f: impl FnOnce(&mut Driver) -> Result<R, DiskFail>) -> Result<R, DiskFail> {
    if !STATE.is_set() {
        match Driver::bring_up() {
            Ok(driver) => STATE.set(driver),
            Err(message) => return Err(DiskFail::Io(message)),
        }
    }
    STATE.with(f)
}

impl Driver {
    /// Find, claim, and bring up the first virtio-blk function visible through the
    /// granted PCI capability. Every step reports a typed, labelled error — device
    /// weirdness is an `io` failure of the disk operation, never a trap.
    fn bring_up() -> Result<Driver, String> {
        let root = pci::default();
        let devices = pci_call("disk.virtio: enumerate", pci::enumerate(&root))?;
        let target = devices
            .iter()
            .find(|d| d.vendor_id == VIRTIO_VENDOR && d.device_id == VIRTIO_BLK_MODERN)
            .or_else(|| {
                devices.iter().find(|d| {
                    d.vendor_id == VIRTIO_VENDOR && d.device_id == VIRTIO_BLK_TRANSITIONAL
                })
            })
            .ok_or_else(|| {
                String::from(
                    "disk.virtio: no virtio-blk function is visible through the granted \
                     pci capability (expected vendor 0x1af4, device 0x1042)",
                )
            })?;
        let address = target.address;
        let device = pci_call("disk.virtio: open", pci::open(&root, address))?;

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
            let bar = pci_call("disk.virtio: open-bar", pci::open_bar(&device, index))?;
            bars.push((index, bar));
        }

        // DMA buffers: the ring page, the request header/status page, the data bounce
        // buffer. CPU address == device address under the kernel's identity map; the
        // provider hands back the device-visible address via `dma-address`.
        let ring = pci_call(
            "disk.virtio: alloc-dma (ring)",
            pci::alloc_dma(&device, RING_BYTES),
        )?;
        let request = pci_call(
            "disk.virtio: alloc-dma (request)",
            pci::alloc_dma(&device, REQ_BYTES),
        )?;
        let data = pci_call(
            "disk.virtio: alloc-dma (data)",
            pci::alloc_dma(&device, DATA_BYTES),
        )?;

        let mut driver = Driver {
            _device: device,
            bars,
            common,
            device_config,
            notify: notify_base,
            notify_offset: 0,
            ring,
            request,
            data,
            queue_size: 0,
            avail_index: 0,
            used_index: 0,
            capacity_bytes: 0,
        };
        driver.start(notify_multiplier)?;
        Ok(driver)
    }

    /// Negotiate features, build the virtqueue, and read the capacity — the device side
    /// of bring-up, once the function is claimed and the DMA buffers exist.
    fn start(&mut self, notify_multiplier: u32) -> Result<(), String> {
        // Reset, then ACKNOWLEDGE and DRIVER.
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, 0)?;
        let mut spins = 0u32;
        while self.common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)? != 0 {
            spins += 1;
            if spins > 1000 {
                return Err(String::from("disk.virtio: device did not reset"));
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

        // Feature negotiation: accept exactly VIRTIO_F_VERSION_1. The device must offer
        // it (it is what makes the modern register layout above valid at all).
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 1)?;
        let high_features = self.common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)?;
        if high_features & FEATURE_VERSION_1_HIGH == 0 {
            return Err(String::from(
                "disk.virtio: the device does not offer VIRTIO_F_VERSION_1 \
                 (is it a legacy-only function?)",
            ));
        }
        self.common_write(COMMON_DRIVER_FEATURE_SELECT, pci::AccessWidth::Dword, 0)?;
        self.common_write(COMMON_DRIVER_FEATURE, pci::AccessWidth::Dword, 0)?;
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
                "disk.virtio: the device rejected the negotiated feature set",
            ));
        }

        // The device DMAs into the rings and the data buffer, so bus mastering must be
        // on before the first request.
        pci_call(
            "disk.virtio: set-bus-master",
            pci::set_bus_master(&self._device, true),
        )?;

        // Virtqueue 0: bound the size to QUEUE_SIZE, point the three rings at the ring
        // DMA page, remember the notify offset, and enable it.
        let queues = self.common_read(COMMON_NUM_QUEUES, pci::AccessWidth::Word)?;
        if queues == 0 {
            return Err(String::from(
                "disk.virtio: the device exposes no virtqueues",
            ));
        }
        self.common_write(COMMON_QUEUE_SELECT, pci::AccessWidth::Word, 0)?;
        let max_size = self.common_read(COMMON_QUEUE_SIZE, pci::AccessWidth::Word)?;
        if max_size == 0 {
            return Err(String::from("disk.virtio: virtqueue 0 is not available"));
        }
        let queue_size = core::cmp::min(max_size, u64::from(QUEUE_SIZE)) as u16;
        self.common_write(
            COMMON_QUEUE_SIZE,
            pci::AccessWidth::Word,
            u64::from(queue_size),
        )?;
        // The driver owns ring initialization (virtio 1.0 §3.1.1): zero the descriptor
        // table and both rings so the device's first used-index write is the first
        // non-zero value the polling loop ever observes.
        pci::dma_write(&self.ring, 0, &[0u8; 1024]);
        let ring_address = pci::dma_address(&self.ring);
        self.write_address(COMMON_QUEUE_DESC, ring_address + DESC_OFFSET)?;
        self.write_address(COMMON_QUEUE_DRIVER, ring_address + AVAIL_OFFSET)?;
        self.write_address(COMMON_QUEUE_DEVICE, ring_address + USED_OFFSET)?;
        let queue_notify_off = self.common_read(COMMON_QUEUE_NOTIFY_OFF, pci::AccessWidth::Word)?;
        self.notify_offset = self.notify.offset + queue_notify_off * u64::from(notify_multiplier);
        // Avail ring starts empty: flags 0, idx 0 (the DMA buffer is zero-filled by the
        // provider, but make the driver's published state explicit).
        pci::dma_write(&self.ring, AVAIL_OFFSET, &[0, 0, 0, 0]);
        self.common_write(COMMON_QUEUE_ENABLE, pci::AccessWidth::Word, 1)?;

        // Everything is in place: tell the device the driver is live.
        let live = with_features_ok | STATUS_DRIVER_OK;
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, live)?;

        // Capacity (in 512-byte sectors) from the device configuration window.
        let capacity_low = self.device_read(0, pci::AccessWidth::Dword)?;
        let capacity_high = self.device_read(4, pci::AccessWidth::Dword)?;
        let sectors = (capacity_high << 32) | capacity_low;
        self.capacity_bytes = sectors * SECTOR;
        self.queue_size = queue_size;

        // One best-effort diagnostic line so a metal session shows what was probed.
        let handle = text::default();
        let line = format!(
            "disk.virtio: virtio-blk {} sectors ({} MiB), queue size {queue_size}",
            sectors,
            self.capacity_bytes / (1024 * 1024),
        );
        let _ = text::write(&handle, text::OutputStream::Out, &line);
        let _ = text::write(&handle, text::OutputStream::Out, "\n");
        Ok(())
    }

    // --- register access helpers ----------------------------------------------------------

    fn bar(&self, index: u8) -> Result<&pci::Bar, String> {
        self.bars
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, bar)| bar)
            .ok_or_else(|| String::from("disk.virtio: internal error: BAR not opened"))
    }

    fn common_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.common.bar)?;
        pci_call(
            "disk.virtio: common config read",
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
            "disk.virtio: common config write",
            pci::bar_write(bar, self.common.offset + register, width, value),
        )
    }

    fn device_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.device_config.bar)?;
        pci_call(
            "disk.virtio: device config read",
            pci::bar_read(bar, self.device_config.offset + register, width),
        )
    }

    /// Write a 64-bit ring address as the two dword halves the common config expects.
    fn write_address(&self, register: u64, address: u64) -> Result<(), String> {
        self.common_write(register, pci::AccessWidth::Dword, address & 0xffff_ffff)?;
        self.common_write(register + 4, pci::AccessWidth::Dword, address >> 32)
    }

    fn notify_queue(&self) -> Result<(), String> {
        let bar = self.bar(self.notify.bar)?;
        pci_call(
            "disk.virtio: queue notify",
            pci::bar_write(bar, self.notify_offset, pci::AccessWidth::Word, 0),
        )
    }

    // --- one request ------------------------------------------------------------------------

    /// Transfer `sectors` sectors starting at `sector`. For a write, `payload` is the
    /// sector-aligned data to send; for a read the function returns the sectors read.
    fn transfer(
        &mut self,
        write: bool,
        sector: u64,
        sectors: u32,
        payload: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, String> {
        let byte_len = u64::from(sectors) * SECTOR;
        debug_assert!(byte_len <= DATA_BYTES);

        // Request header: type, reserved, starting sector (all little-endian).
        let request_type = if write { BLK_T_OUT } else { BLK_T_IN };
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&request_type.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        pci::dma_write(&self.request, REQ_HEADER_OFFSET, &header);
        // Status byte: preset to a non-status value so a completion that somehow skips
        // writing it is caught.
        pci::dma_write(&self.request, REQ_STATUS_OFFSET, &[0xff]);
        if let Some(bytes) = payload {
            pci::dma_write(&self.data, 0, bytes);
        }

        // Three-descriptor chain at slots 0..2 of the descriptor table.
        let request_address = pci::dma_address(&self.request);
        let data_address = pci::dma_address(&self.data);
        let data_flags = if write {
            DESC_F_NEXT // device reads our data
        } else {
            DESC_F_NEXT | DESC_F_WRITE // device writes our data
        };
        self.write_descriptor(0, request_address + REQ_HEADER_OFFSET, 16, DESC_F_NEXT, 1);
        self.write_descriptor(1, data_address, byte_len as u32, data_flags, 2);
        self.write_descriptor(2, request_address + REQ_STATUS_OFFSET, 1, DESC_F_WRITE, 0);

        // Publish descriptor 0 in the avail ring, then bump avail.idx.
        let slot = u64::from(self.avail_index % self.queue_size);
        pci::dma_write(&self.ring, AVAIL_OFFSET + 4 + 2 * slot, &0u16.to_le_bytes());
        self.avail_index = self.avail_index.wrapping_add(1);
        pci::dma_write(
            &self.ring,
            AVAIL_OFFSET + 2,
            &self.avail_index.to_le_bytes(),
        );

        // Kick the device and poll the used ring for the completion.
        self.notify_queue()?;
        let mut spins: u64 = 0;
        loop {
            let raw = pci::dma_read(&self.ring, USED_OFFSET + 2, 2);
            let used = u16::from_le_bytes([raw[0], raw[1]]);
            if used != self.used_index {
                self.used_index = self.used_index.wrapping_add(1);
                break;
            }
            spins += 1;
            if spins > POLL_LIMIT {
                return Err(String::from(
                    "disk.virtio: the device did not complete the request (poll limit)",
                ));
            }
        }

        let status = pci::dma_read(&self.request, REQ_STATUS_OFFSET, 1)[0];
        if status != BLK_S_OK {
            return Err(format!(
                "disk.virtio: the device reported request status {status}"
            ));
        }
        if write {
            Ok(None)
        } else {
            Ok(Some(pci::dma_read(&self.data, 0, byte_len)))
        }
    }

    /// Write one 16-byte split-virtqueue descriptor.
    fn write_descriptor(&self, index: u64, address: u64, len: u32, flags: u16, next: u16) {
        let mut descriptor = [0u8; 16];
        descriptor[0..8].copy_from_slice(&address.to_le_bytes());
        descriptor[8..12].copy_from_slice(&len.to_le_bytes());
        descriptor[12..14].copy_from_slice(&flags.to_le_bytes());
        descriptor[14..16].copy_from_slice(&next.to_le_bytes());
        pci::dma_write(&self.ring, DESC_OFFSET + index * 16, &descriptor);
    }

    // --- byte-addressed operations over the sector device -----------------------------------

    /// Read `len` bytes at byte offset `offset`.
    fn read_bytes(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, DiskFail> {
        self.check_range(offset, len)?;
        let mut out: Vec<u8> = Vec::with_capacity(len as usize);
        let mut cursor = offset;
        let mut remaining = len;
        while remaining > 0 {
            let first_sector = cursor / SECTOR;
            let within = cursor - first_sector * SECTOR;
            let take = core::cmp::min(remaining, DATA_BYTES - within);
            let sectors = within
                .checked_add(take)
                .map(|end| end.div_ceil(SECTOR))
                .unwrap_or(0) as u32;
            let chunk = self
                .transfer(false, first_sector, sectors, None)
                .map_err(DiskFail::Io)?
                .unwrap_or_default();
            let start = within as usize;
            let end = (within + take) as usize;
            out.extend_from_slice(&chunk[start..end]);
            cursor += take;
            remaining -= take;
        }
        Ok(out)
    }

    /// Write `bytes` at byte offset `offset`, read–modify–writing partial edge sectors.
    fn write_bytes(&mut self, offset: u64, bytes: &[u8]) -> Result<(), DiskFail> {
        let len = bytes.len() as u64;
        self.check_range(offset, len)?;
        let mut cursor = offset;
        let mut written: u64 = 0;
        while written < len {
            let first_sector = cursor / SECTOR;
            let within = cursor - first_sector * SECTOR;
            let take = core::cmp::min(len - written, DATA_BYTES - within);
            let end_within = within + take;
            let sectors = end_within.div_ceil(SECTOR) as u32;
            let aligned = within == 0 && end_within.is_multiple_of(SECTOR);
            let span = u64::from(sectors) * SECTOR;
            let chunk: Vec<u8> = if aligned {
                bytes[written as usize..(written + take) as usize].to_vec()
            } else {
                // Read the covering sectors, overlay the new bytes, write the span back.
                let mut current = self
                    .transfer(false, first_sector, sectors, None)
                    .map_err(DiskFail::Io)?
                    .unwrap_or_default();
                if current.len() < span as usize {
                    return Err(DiskFail::Io(String::from(
                        "disk.virtio: short read during read-modify-write",
                    )));
                }
                current[within as usize..end_within as usize]
                    .copy_from_slice(&bytes[written as usize..(written + take) as usize]);
                current
            };
            self.transfer(true, first_sector, sectors, Some(&chunk))
                .map_err(DiskFail::Io)?;
            cursor += take;
            written += take;
        }
        Ok(())
    }

    /// The disk-contract range rule: the whole range must lie within the device, and a
    /// zero-length access at any offset up to the capacity succeeds.
    fn check_range(&self, offset: u64, len: u64) -> Result<(), DiskFail> {
        let end = offset.checked_add(len).ok_or(DiskFail::OutOfRange)?;
        if end > self.capacity_bytes {
            return Err(DiskFail::OutOfRange);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------
// Capability-window discovery (the vendor-specific PCI capabilities).
// ------------------------------------------------------------------------------------------

/// Walk the configuration-space capability list and return the common, notify (plus its
/// multiplier), and device-config windows.
fn find_windows(device: &pci::Device) -> Result<(Region, Region, u32, Region), String> {
    let read = |offset: u32, width: pci::AccessWidth| -> Result<u64, String> {
        pci_call(
            "disk.virtio: config read",
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
        String::from("disk.virtio: the function has no virtio common-config capability")
    })?;
    let (notify, multiplier) = notify
        .ok_or_else(|| String::from("disk.virtio: the function has no virtio notify capability"))?;
    let device_config = device_config.ok_or_else(|| {
        String::from("disk.virtio: the function has no virtio device-config capability")
    })?;
    Ok((common, notify, multiplier, device_config))
}

// ------------------------------------------------------------------------------------------
// The exported eo9:disk provider
// ------------------------------------------------------------------------------------------

/// The `disk.virtio` provider.
struct Stub;

/// The root-handle resource: a token referring to the claimed and brought-up device.
struct VirtioDisk;

impl types::Guest for Stub {
    type DiskImpl = VirtioDisk;
}

impl types::GuestDiskImpl for VirtioDisk {}

impl disk::Guest for Stub {
    fn default() -> types::DiskImpl {
        types::DiskImpl::new(VirtioDisk)
    }

    async fn read(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, ReadError>) {
        let len = dst.len();
        let outcome = with_driver(|driver| driver.read_bytes(offset, len));
        match outcome {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    dst.write(0, &bytes);
                }
                (dst, Ok(ReadResult { bytes_read: len }))
            }
            Err(DiskFail::OutOfRange) => (dst, Err(ReadError::OutOfRange)),
            Err(DiskFail::Io(message)) => (dst, Err(ReadError::Io(message))),
        }
    }

    async fn write(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, WriteError>) {
        let len = src.len();
        // Copy out of the buffer before driving the device so no buffer call interleaves
        // with the request (same discipline as disk.mem).
        let bytes = if len == 0 {
            Vec::new()
        } else {
            src.read(0, len)
        };
        let outcome = with_driver(|driver| driver.write_bytes(offset, &bytes));
        match outcome {
            Ok(()) => (src, Ok(WriteResult { bytes_written: len })),
            Err(DiskFail::OutOfRange) => (src, Err(WriteError::OutOfRange)),
            Err(DiskFail::Io(message)) => (src, Err(WriteError::Io(message))),
        }
    }
}

export!(Stub);
