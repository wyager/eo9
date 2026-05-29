//! A minimal in-kernel virtio-blk driver for the persistent store disk (plan/12:
//! store-on-eofs).
//!
//! This is the kernel's own polled, single-request driver for one modern virtio-blk PCI
//! function, used **only** for the kernel's persistent store disk (the disk-backed cache of
//! on-target compile results behind the `storedisk` boot token). It deliberately mirrors the
//! `disk.virtio` wasm driver (guest/stubs/disk-virtio) — same bring-up sequence, same
//! three-descriptor request shape, same polled used ring — but runs in the kernel against
//! `src/pci.rs` directly, because the store disk is infrastructure the kernel itself needs
//! before and during component execution (the wasm driver remains the way *programs* get a
//! disk capability).
//!
//! Sharing caveat (documented in plan/12): the kernel claims the **first** virtio-blk
//! function it finds and reprograms its BARs. Granting the same function to a guest
//! `disk.virtio` driver in the same boot is not supported until machine-global device
//! claiming lands; the `storedisk` demo therefore runs without the guest-side `disk` flag.
//!
//! DMA + memory model: the heap is identity-mapped (CPU address == bus address) and RW-NX,
//! which is exactly what the rings and bounce buffer need. Ring memory is written by the
//! device, so all accesses to it go through volatile reads/writes with explicit fences
//! around the publish/notify/poll steps.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{Ordering, fence};

use crate::pci;

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_BLK_MODERN: u16 = 0x1042;
const VIRTIO_BLK_TRANSITIONAL: u16 = 0x1001;

const PCI_CAP_POINTER: u32 = 0x34;
const PCI_CAP_ID_VENDOR: u64 = 0x09;
const PCI_CAP_WALK_LIMIT: usize = 48;

const VIRTIO_PCI_CAP_COMMON: u64 = 1;
const VIRTIO_PCI_CAP_NOTIFY: u64 = 2;
const VIRTIO_PCI_CAP_DEVICE: u64 = 4;

// Modern common-configuration register offsets (virtio 1.0 §4.1.4.3).
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

const STATUS_ACKNOWLEDGE: u64 = 1;
const STATUS_DRIVER: u64 = 2;
const STATUS_DRIVER_OK: u64 = 4;
const STATUS_FEATURES_OK: u64 = 8;

/// VIRTIO_F_VERSION_1 is bit 32: bit 0 of the high feature dword.
const FEATURE_VERSION_1_HIGH: u64 = 1;

const SECTOR: u64 = 512;
const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;
const BLK_S_OK: u8 = 0;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

const QUEUE_SIZE: u16 = 16;
const RING_BYTES: usize = 4096;
const DESC_OFFSET: usize = 0;
const AVAIL_OFFSET: usize = 256;
const USED_OFFSET: usize = 512;

const REQ_BYTES: usize = 4096;
const REQ_HEADER_OFFSET: usize = 0;
const REQ_STATUS_OFFSET: usize = 16;

/// The bounce buffer; one transfer moves at most this many bytes.
const DATA_BYTES: usize = 64 * 1024;

/// Poll bound for one request (the device is QEMU; this is a hang backstop, not a timeout).
const POLL_LIMIT: u64 = 50_000_000;

/// One page-aligned, identity-mapped DMA allocation. Never freed in practice (the store
/// disk lives for the rest of the boot), but `Drop` keeps it correct anyway.
struct DmaRegion {
    pointer: *mut u8,
    layout: Layout,
}

// SAFETY: the region is exclusively owned by the driver, which is itself behind a lock.
unsafe impl Send for DmaRegion {}

impl DmaRegion {
    fn new(bytes: usize) -> Result<DmaRegion, String> {
        let layout = Layout::from_size_align(bytes, 4096)
            .map_err(|_| String::from("storedisk: bad DMA layout"))?;
        // SAFETY: layout has non-zero size; the result is checked below.
        let pointer = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if pointer.is_null() {
            return Err(String::from("storedisk: DMA allocation failed"));
        }
        Ok(DmaRegion { pointer, layout })
    }

    /// Bus address of the region (identity map: equal to the CPU address).
    fn address(&self) -> u64 {
        self.pointer as u64
    }

    fn write(&self, offset: usize, bytes: &[u8]) {
        debug_assert!(offset + bytes.len() <= self.layout.size());
        for (i, byte) in bytes.iter().enumerate() {
            // SAFETY: bounds asserted above; the device only reads this memory.
            unsafe { core::ptr::write_volatile(self.pointer.add(offset + i), *byte) };
        }
    }

    fn read(&self, offset: usize, len: usize) -> Vec<u8> {
        debug_assert!(offset + len <= self.layout.size());
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            // SAFETY: bounds asserted above; volatile because the device writes this memory.
            out.push(unsafe { core::ptr::read_volatile(self.pointer.add(offset + i)) });
        }
        out
    }

    fn read_u16(&self, offset: usize) -> u16 {
        let raw = self.read(offset, 2);
        u16::from_le_bytes([raw[0], raw[1]])
    }
}

impl Drop for DmaRegion {
    fn drop(&mut self) {
        // SAFETY: allocated with this exact layout in `new`.
        unsafe { alloc::alloc::dealloc(self.pointer, self.layout) };
    }
}

/// One virtio register window: a BAR index plus an offset within it.
#[derive(Clone, Copy)]
struct Region {
    bar: u8,
    offset: u64,
}

/// The kernel's polled virtio-blk function.
pub struct VirtioBlk {
    /// Mapped base address per opened BAR index.
    bars: Vec<(u8, usize)>,
    common: Region,
    device_config: Region,
    notify_bar: u8,
    notify_offset: u64,
    ring: DmaRegion,
    request: DmaRegion,
    data: DmaRegion,
    queue_size: u16,
    avail_index: u16,
    used_index: u16,
    capacity_bytes: u64,
}

impl VirtioBlk {
    /// Find the first virtio-blk function, bring it up, and return a ready driver.
    pub fn probe_and_start() -> Result<VirtioBlk, String> {
        let functions = pci::enumerate();
        let target = functions
            .iter()
            .find(|f| f.vendor_id == VIRTIO_VENDOR && f.device_id == VIRTIO_BLK_MODERN)
            .or_else(|| {
                functions.iter().find(|f| {
                    f.vendor_id == VIRTIO_VENDOR && f.device_id == VIRTIO_BLK_TRANSITIONAL
                })
            })
            .ok_or_else(|| {
                String::from(
                    "no virtio-blk function is visible on the PCI bus \
                     (boot with the xtask `storedisk` argument so QEMU attaches one)",
                )
            })?;
        let address = target.address;

        let (common, notify, notify_multiplier, device_config) = find_windows(address)?;

        // Assign each referenced BAR exactly once and remember its mapped base.
        let descriptions = pci::describe_bars(address);
        let mut bars: Vec<(u8, usize)> = Vec::new();
        for index in [common.bar, notify.bar, device_config.bar] {
            if bars.iter().any(|(i, _)| *i == index) {
                continue;
            }
            let description = descriptions
                .iter()
                .find(|d| d.index == index)
                .ok_or_else(|| {
                    format!("storedisk: virtio capability references missing BAR {index}")
                })?;
            if description.io_space {
                return Err(String::from("storedisk: I/O-space BARs are not supported"));
            }
            let base = pci::assign_bar(address, description).ok_or_else(|| {
                String::from("storedisk: BAR assignment failed (window exhausted)")
            })?;
            bars.push((index, base));
        }

        let mut driver = VirtioBlk {
            bars,
            common,
            device_config,
            notify_bar: notify.bar,
            notify_offset: 0,
            ring: DmaRegion::new(RING_BYTES)?,
            request: DmaRegion::new(REQ_BYTES)?,
            data: DmaRegion::new(DATA_BYTES)?,
            queue_size: 0,
            avail_index: 0,
            used_index: 0,
            capacity_bytes: 0,
        };
        driver.start(address, notify, notify_multiplier)?;
        Ok(driver)
    }

    fn start(
        &mut self,
        address: pci::FunctionAddress,
        notify: Region,
        notify_multiplier: u32,
    ) -> Result<(), String> {
        // Reset, then ACKNOWLEDGE and DRIVER.
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, 0)?;
        let mut spins = 0u32;
        while self.common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)? != 0 {
            spins += 1;
            if spins > 1000 {
                return Err(String::from(
                    "storedisk: the virtio-blk device did not reset",
                ));
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

        // Features: accept exactly VIRTIO_F_VERSION_1.
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 1)?;
        let high = self.common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)?;
        if high & FEATURE_VERSION_1_HIGH == 0 {
            return Err(String::from(
                "storedisk: the virtio-blk device does not offer VIRTIO_F_VERSION_1",
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
                "storedisk: the virtio-blk device rejected the negotiated features",
            ));
        }

        // The device DMAs into the ring and data buffers.
        if !pci::set_bus_master(address, true) {
            return Err(String::from("storedisk: enabling bus mastering failed"));
        }

        // Virtqueue 0.
        let queues = self.common_read(COMMON_NUM_QUEUES, pci::AccessWidth::Word)?;
        if queues == 0 {
            return Err(String::from("storedisk: the device exposes no virtqueues"));
        }
        self.common_write(COMMON_QUEUE_SELECT, pci::AccessWidth::Word, 0)?;
        let max_size = self.common_read(COMMON_QUEUE_SIZE, pci::AccessWidth::Word)?;
        if max_size == 0 {
            return Err(String::from("storedisk: virtqueue 0 is not available"));
        }
        let queue_size = core::cmp::min(max_size, u64::from(QUEUE_SIZE)) as u16;
        self.common_write(
            COMMON_QUEUE_SIZE,
            pci::AccessWidth::Word,
            u64::from(queue_size),
        )?;
        // The allocation is zeroed, but make the driver-owned ring state explicit anyway.
        self.ring.write(0, &[0u8; 1024]);
        let ring_address = self.ring.address();
        self.write_address(COMMON_QUEUE_DESC, ring_address + DESC_OFFSET as u64)?;
        self.write_address(COMMON_QUEUE_DRIVER, ring_address + AVAIL_OFFSET as u64)?;
        self.write_address(COMMON_QUEUE_DEVICE, ring_address + USED_OFFSET as u64)?;
        let queue_notify_off = self.common_read(COMMON_QUEUE_NOTIFY_OFF, pci::AccessWidth::Word)?;
        self.notify_offset = notify.offset + queue_notify_off * u64::from(notify_multiplier);
        self.common_write(COMMON_QUEUE_ENABLE, pci::AccessWidth::Word, 1)?;

        // Driver live.
        self.common_write(
            COMMON_DEVICE_STATUS,
            pci::AccessWidth::Byte,
            with_features_ok | STATUS_DRIVER_OK,
        )?;

        // Capacity in 512-byte sectors from the device-config window.
        let low = self.device_read(0, pci::AccessWidth::Dword)?;
        let high = self.device_read(4, pci::AccessWidth::Dword)?;
        self.capacity_bytes = ((high << 32) | low) * SECTOR;
        self.queue_size = queue_size;
        Ok(())
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    // --- register access helpers ---------------------------------------------------------

    fn bar_base(&self, index: u8) -> Result<usize, String> {
        self.bars
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, base)| *base)
            .ok_or_else(|| String::from("storedisk: internal error: BAR not opened"))
    }

    fn common_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let base = self.bar_base(self.common.bar)?;
        pci::bar_read(base, self.common.offset + register, width)
            .ok_or_else(|| String::from("storedisk: common config read failed"))
    }

    fn common_write(
        &self,
        register: u64,
        width: pci::AccessWidth,
        value: u64,
    ) -> Result<(), String> {
        let base = self.bar_base(self.common.bar)?;
        if pci::bar_write(base, self.common.offset + register, width, value) {
            Ok(())
        } else {
            Err(String::from("storedisk: common config write failed"))
        }
    }

    fn device_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let base = self.bar_base(self.device_config.bar)?;
        pci::bar_read(base, self.device_config.offset + register, width)
            .ok_or_else(|| String::from("storedisk: device config read failed"))
    }

    fn write_address(&self, register: u64, address: u64) -> Result<(), String> {
        self.common_write(register, pci::AccessWidth::Dword, address & 0xffff_ffff)?;
        self.common_write(register + 4, pci::AccessWidth::Dword, address >> 32)
    }

    fn notify_queue(&self) -> Result<(), String> {
        let base = self.bar_base(self.notify_bar)?;
        if pci::bar_write(base, self.notify_offset, pci::AccessWidth::Word, 0) {
            Ok(())
        } else {
            Err(String::from("storedisk: queue notify failed"))
        }
    }

    fn write_descriptor(&self, index: usize, address: u64, len: u32, flags: u16, next: u16) {
        let mut descriptor = [0u8; 16];
        descriptor[0..8].copy_from_slice(&address.to_le_bytes());
        descriptor[8..12].copy_from_slice(&len.to_le_bytes());
        descriptor[12..14].copy_from_slice(&flags.to_le_bytes());
        descriptor[14..16].copy_from_slice(&next.to_le_bytes());
        self.ring.write(DESC_OFFSET + index * 16, &descriptor);
    }

    /// Transfer whole sectors through the bounce buffer. For a write, `payload` carries the
    /// sector-aligned bytes; for a read the sectors are returned.
    fn transfer(
        &mut self,
        write: bool,
        sector: u64,
        sectors: u32,
        payload: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, String> {
        let byte_len = sectors as usize * SECTOR as usize;
        debug_assert!(byte_len <= DATA_BYTES);

        let request_type = if write { BLK_T_OUT } else { BLK_T_IN };
        let mut header = [0u8; 16];
        header[0..4].copy_from_slice(&request_type.to_le_bytes());
        header[8..16].copy_from_slice(&sector.to_le_bytes());
        self.request.write(REQ_HEADER_OFFSET, &header);
        // Preset the status byte to a non-status value so a skipped completion is caught.
        self.request.write(REQ_STATUS_OFFSET, &[0xff]);
        if let Some(bytes) = payload {
            self.data.write(0, bytes);
        }

        let request_address = self.request.address();
        let data_address = self.data.address();
        let data_flags = if write {
            DESC_F_NEXT
        } else {
            DESC_F_NEXT | DESC_F_WRITE
        };
        self.write_descriptor(
            0,
            request_address + REQ_HEADER_OFFSET as u64,
            16,
            DESC_F_NEXT,
            1,
        );
        self.write_descriptor(1, data_address, byte_len as u32, data_flags, 2);
        self.write_descriptor(
            2,
            request_address + REQ_STATUS_OFFSET as u64,
            1,
            DESC_F_WRITE,
            0,
        );

        // Publish descriptor 0 in the avail ring, bump avail.idx, then notify. The fences
        // order the ring writes against the index publish and the doorbell.
        let slot = (self.avail_index % self.queue_size) as usize;
        self.ring
            .write(AVAIL_OFFSET + 4 + 2 * slot, &0u16.to_le_bytes());
        fence(Ordering::SeqCst);
        self.avail_index = self.avail_index.wrapping_add(1);
        self.ring
            .write(AVAIL_OFFSET + 2, &self.avail_index.to_le_bytes());
        fence(Ordering::SeqCst);
        self.notify_queue()?;

        let mut spins: u64 = 0;
        loop {
            let used = self.ring.read_u16(USED_OFFSET + 2);
            if used != self.used_index {
                self.used_index = self.used_index.wrapping_add(1);
                break;
            }
            spins += 1;
            if spins > POLL_LIMIT {
                return Err(String::from(
                    "storedisk: the virtio-blk device did not complete a request (poll limit)",
                ));
            }
            core::hint::spin_loop();
        }
        fence(Ordering::SeqCst);

        let status = self.request.read(REQ_STATUS_OFFSET, 1)[0];
        if status != BLK_S_OK {
            return Err(format!(
                "storedisk: the device reported request status {status}"
            ));
        }
        if write {
            Ok(None)
        } else {
            Ok(Some(self.data.read(0, byte_len)))
        }
    }

    // --- byte-addressed access (read-modify-write at the edges) ---------------------------

    fn check_range(&self, offset: u64, len: u64) -> Result<(), String> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| String::from("storedisk: range overflow"))?;
        if end > self.capacity_bytes {
            return Err(String::from("storedisk: access beyond the end of the disk"));
        }
        Ok(())
    }

    pub fn read_bytes(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        self.check_range(offset, buf.len() as u64)?;
        let mut cursor = offset;
        let mut filled = 0usize;
        while filled < buf.len() {
            let sector = cursor / SECTOR;
            let within = (cursor % SECTOR) as usize;
            let remaining = buf.len() - filled;
            // Whole-sector batch through the bounce buffer where possible.
            let max_sectors = (DATA_BYTES / SECTOR as usize) as u64;
            let want_sectors =
                core::cmp::min(((within + remaining) as u64).div_ceil(SECTOR), max_sectors) as u32;
            let bytes = self
                .transfer(false, sector, want_sectors, None)?
                .unwrap_or_default();
            let available = bytes.len() - within;
            let take = core::cmp::min(available, remaining);
            buf[filled..filled + take].copy_from_slice(&bytes[within..within + take]);
            filled += take;
            cursor += take as u64;
        }
        Ok(())
    }

    pub fn write_bytes(&mut self, offset: u64, data: &[u8]) -> Result<(), String> {
        self.check_range(offset, data.len() as u64)?;
        let mut cursor = offset;
        let mut consumed = 0usize;
        while consumed < data.len() {
            let sector = cursor / SECTOR;
            let within = (cursor % SECTOR) as usize;
            let remaining = data.len() - consumed;
            let max_bytes = DATA_BYTES - within;
            let take = core::cmp::min(remaining, max_bytes);
            let span_sectors = ((within + take) as u64).div_ceil(SECTOR) as u32;
            let span_bytes = span_sectors as usize * SECTOR as usize;
            let mut block = if within != 0 || take != span_bytes {
                // Read-modify-write for the partial sectors at the edges.
                self.transfer(false, sector, span_sectors, None)?
                    .unwrap_or_default()
            } else {
                alloc::vec![0u8; span_bytes]
            };
            block[within..within + take].copy_from_slice(&data[consumed..consumed + take]);
            self.transfer(true, sector, span_sectors, Some(&block))?;
            consumed += take;
            cursor += take as u64;
        }
        Ok(())
    }
}

/// Walk the function's vendor capabilities for the common/notify/device-config windows.
fn find_windows(address: pci::FunctionAddress) -> Result<(Region, Region, u32, Region), String> {
    let read = |offset: u32, width: pci::AccessWidth| -> Result<u64, String> {
        pci::config_read(address, offset, width)
            .ok_or_else(|| String::from("storedisk: configuration-space read failed"))
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

    let common =
        common.ok_or_else(|| String::from("storedisk: no virtio common-config capability"))?;
    let (notify, multiplier) =
        notify.ok_or_else(|| String::from("storedisk: no virtio notify capability"))?;
    let device_config = device_config
        .ok_or_else(|| String::from("storedisk: no virtio device-config capability"))?;
    Ok((common, notify, multiplier, device_config))
}
