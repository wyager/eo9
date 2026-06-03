//! `gpu.virtio` — a virtio-gpu (2D) driver as an ordinary wasm component.
//!
//! Targets the crate-local `eo9:gpu-virtio/virtio` world: imports the PCI capability
//! (`eo9:pci/pci`) plus `eo9:text/text` for one diagnostic line, and exports
//! `eo9:gfx/gfx` backed by a virtio-gpu PCI function's 2D control queue. The driver
//! holds no policy of its own: which functions it can see (and therefore claim) is
//! entirely the PCI provider's business — the kernel root only when the boot granted
//! `pci`, an attenuating `pci.filtered` for "exactly this one device" grants.
//!
//! Shape of the device conversation (virtio 1.x §5.7, the 2D subset):
//!
//! * **Probe.** Enumerate the capability's view of the bus, claim the first virtio-gpu
//!   function (vendor 0x1af4, modern device id 0x1050), and walk its vendor-specific
//!   PCI capabilities to find the common / notify / ISR register windows.
//! * **Bring-up.** Reset, ACKNOWLEDGE → DRIVER, negotiate exactly `VIRTIO_F_VERSION_1`,
//!   FEATURES_OK (verified by reading it back), enable bus mastering, build control
//!   queue 0 (16 entries) in DMA buffers from `alloc-dma`, DRIVER_OK. Then over the
//!   control queue: `GET_DISPLAY_INFO` (scanout 0's geometry = the mode),
//!   `RESOURCE_CREATE_2D` (one xrgb8888 resource of that geometry),
//!   `RESOURCE_ATTACH_BACKING` (a single DMA framebuffer), `SET_SCANOUT`.
//! * **Present.** Copy the damage rows into the DMA backing (the stride math lives
//!   here; operation buffers are tightly packed per the API contract),
//!   `TRANSFER_TO_HOST_2D` for the damage rectangle, then `RESOURCE_FLUSH`. `read`
//!   answers from the same backing — the provider's copy of what was presented (the
//!   API's documented readback semantics; see wit/gfx). Completion is observed by
//!   waiting on the device's INTx when the provider routes interrupts, falling back to
//!   polling the used ring (the same machinery as `disk.virtio`).
//!
//! Like its siblings, the driver **genuinely awaits its imports** (SPEC, "Boundaries
//! are honestly async"): every `eo9:pci` operation is awaited, so a PCI provider that
//! defers — an attenuating guest like `pci.filtered`, an interrupt wait that parks the
//! core — suspends the gfx operation and resumes it on completion, instead of failing
//! it. The waits stay bounded where the waiting actually happens: the interrupt path
//! bounds its wait retries, the polled fallback bounds its spins, and the kernel bounds
//! each `wait` host call, so a dead device surfaces as a typed `io` error, never a
//! hang. The documented default state (no configure interface) is "claim the first
//! virtio-gpu function on first use"; first use also prints one `gpu.virtio: …`
//! diagnostic line so a metal session shows what was probed. One consequence of lazy
//! bring-up: the synchronous `mode` query answers a typed `io` error until the first
//! *awaited* operation has brought the device up (bring-up awaits, and a synchronous
//! export cannot) — the same shape as `disk.virtio`'s `size` reporting 0 before first
//! use. Consumers wake the device with one awaited operation (a zero-area `clear` is
//! the cheapest no-op — the draw example does exactly this) and ask again.
//!
//! Bounds: the v1 driver backs the whole framebuffer with ONE `alloc-dma` allocation,
//! so the mode is limited to the provider's 4 MiB DMA cap — 1024x768 xrgb8888 (3 MiB)
//! fits; 4K does not and reports a typed `io` error naming the limit (multi-entry
//! attach-backing is the recorded follow-up, plan/09). All polls are bounded; device
//! weirdness is a typed `gfx-error`, never a trap or hang.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

// Linked for its runtime contract (allocator, panic handler, diagnostics import); the
// provider state here is the take/put `Slot` below, not `ProviderState`.
use eo9_guest as _;

wit_bindgen::generate!({
    world: "virtio",
    path: "wit",
    // Pull in bindings for eo9:pci/types, eo9:gfx/types, and eo9:io/buffers, which the
    // imported and exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::pci::pci;
use eo9::text::text;
use exports::eo9::gfx::gfx::{self, Buffer, GfxError, ModeInfo, PixelFormat, Rect};
use exports::eo9::gfx::types;

// ------------------------------------------------------------------------------------------
// Constants: PCI configuration space, virtio-pci capabilities, the common config window,
// the split virtqueue (all as in disk.virtio), and the virtio-gpu 2D protocol.
// ------------------------------------------------------------------------------------------

/// virtio vendor id.
const VIRTIO_VENDOR: u16 = 0x1af4;
/// Modern (virtio 1.0+) virtio-gpu device id (0x1040 + device type 16).
const VIRTIO_GPU_MODERN: u16 = 0x1050;

/// Configuration-space offset of the capabilities pointer.
const PCI_CAP_POINTER: u32 = 0x34;
/// Vendor-specific capability id (virtio structures).
const PCI_CAP_ID_VENDOR: u64 = 0x09;
/// Upper bound on capability-list traversal.
const PCI_CAP_WALK_LIMIT: usize = 48;

/// virtio_pci_cap.cfg_type values.
const VIRTIO_PCI_CAP_COMMON: u64 = 1;
const VIRTIO_PCI_CAP_NOTIFY: u64 = 2;
const VIRTIO_PCI_CAP_ISR: u64 = 3;

/// Offsets within the common configuration window (virtio 1.x §4.1.4.3).
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

/// Split-virtqueue descriptor flags.
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// Control-queue size the driver uses (the device's maximum is reduced to this).
const QUEUE_SIZE: u16 = 16;
/// Ring DMA buffer layout (one page): descriptor table, avail ring, used ring —
/// alignments 16 / 2 / 4 are all satisfied by these offsets.
const RING_BYTES: u64 = 4096;
const DESC_OFFSET: u64 = 0;
const AVAIL_OFFSET: u64 = 256;
const USED_OFFSET: u64 = 512;

/// Command/response DMA buffer layout (one page): the command at 0, the response at
/// 1024. The largest command this driver sends is TRANSFER_TO_HOST_2D (56 bytes); the
/// largest response is RESP_OK_DISPLAY_INFO (24 + 16 * 24 = 408 bytes).
const CMD_BYTES: u64 = 4096;
const CMD_OFFSET: u64 = 0;
const RESP_OFFSET: u64 = 1024;
const RESP_MAX: u32 = 1024;

/// virtio-gpu 2D control types (virtio 1.x §5.7.4).
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

/// The control header every command and response starts with (24 bytes: le32 type,
/// le32 flags, le64 fence-id, le32 ctx-id, u8 ring-idx, u8 padding[3]).
const CTRL_HEADER: usize = 24;

/// `VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM`: memory bytes B,G,R,X — one little-endian
/// `0x00RRGGBB` word per pixel, exactly the API's `xrgb8888`.
const FORMAT_B8G8R8X8: u32 = 2;

/// The one host-side resource this driver ever creates (ids are driver-chosen, non-zero).
const RESOURCE_ID: u32 = 1;
/// The scanout this driver drives.
const SCANOUT_ID: u32 = 0;

/// Bytes per xrgb8888 pixel.
const BYTES_PER_PIXEL: u64 = 4;

/// The provider's per-allocation DMA cap (plan/02 D22); the v1 single-entry backing
/// limits the mode to this many bytes.
const MAX_FRAMEBUFFER_BYTES: u64 = 4 * 1024 * 1024;

/// Used-ring polling bound (each iteration is a host call; see disk.virtio).
const POLL_LIMIT: u64 = 50_000_000;
/// Interrupt waits per command before falling back to the polled loop.
const INTERRUPT_WAIT_RETRIES: u32 = 4;

// ------------------------------------------------------------------------------------------
// Awaited driving of the async pci imports (same pattern and reasoning as disk.virtio).
// ------------------------------------------------------------------------------------------

/// Run one PCI operation, awaiting it (a deferring provider suspends us; our consumer
/// awaits us in turn), and flatten its result, labelling failures with `what`.
async fn pci_call<T>(
    what: &str,
    future: impl Future<Output = Result<T, pci::PciError>>,
) -> Result<T, String> {
    match future.await {
        Ok(value) => Ok(value),
        Err(error) => Err(format!("{what}: {error:?}")),
    }
}

/// One diagnostic line on the console (best-effort; used for rare degraded-mode notices).
fn diag(line: &str) {
    let handle = text::default();
    let _ = text::write(&handle, text::OutputStream::Out, line);
    let _ = text::write(&handle, text::OutputStream::Out, "\n");
}

// ------------------------------------------------------------------------------------------
// Driver state
// ------------------------------------------------------------------------------------------

/// One register window discovered from a virtio PCI capability.
struct Region {
    bar: u8,
    offset: u64,
}

/// The brought-up device and scanned-out framebuffer.
struct Driver {
    /// Keeps the exclusive claim on the function alive for the component's lifetime.
    _device: pci::Device,
    bars: Vec<(u8, pci::Bar)>,
    common: Region,
    notify: Region,
    notify_offset: u64,
    /// The ISR status window (read-to-clear; deasserts INTx).
    isr: Option<Region>,
    /// The INTx vector the provider granted, or `None` (poll the used ring).
    interrupt: Option<pci::Interrupt>,
    ring: pci::DmaBuffer,
    /// Command + response page.
    command: pci::DmaBuffer,
    /// The framebuffer backing: what `TRANSFER_TO_HOST_2D` reads and `read` answers from.
    /// Allocated once GET_DISPLAY_INFO has reported the geometry. NEVER dropped while the
    /// device is live: freeing any DMA buffer makes the kernel conservatively quiesce the
    /// task's devices (bus-master off — and on QEMU's virtio-pci, clearing bus mastering
    /// also clears DRIVER_OK), which would kill the device mid-conversation. The take/put
    /// slot preserves this: the guard returns the whole `Driver` (framebuffer included)
    /// to the slot on every exit path, including cancellation, so the allocation lives
    /// as long as the component.
    framebuffer: Option<pci::DmaBuffer>,
    queue_size: u16,
    /// Next avail-ring index (free-running, wraps at 65536 like the device's view).
    avail_index: u16,
    /// Used-ring entries consumed so far (free-running).
    used_index: u16,
    width: u32,
    height: u32,
    /// Whether the once-per-conversation polled-fallback notice was already printed.
    reported_polled_fallback: bool,
}

/// The provider's state slot. Operations await mid-flight (the pci calls), so the driver
/// is **taken out** of the slot for the duration of an operation and put back afterwards;
/// a second activation arriving while the slot is `Busy` gets a typed error — never a
/// `RefCell` re-borrow trap (the discipline `ProviderState` cannot offer across awaits).
enum Slot {
    /// Not brought up yet (first use probes and initializes).
    Empty,
    /// An operation is in flight (the driver is on that operation's stack).
    Busy,
    /// Brought up and idle.
    Ready(Driver),
}

struct DriverState {
    inner: RefCell<Slot>,
}

// SAFETY: guest components run single-threaded (shared-memory threading is an ungranted
// capability — see SPEC "Execution APIs"); `Sync` is only needed for the `static`.
unsafe impl Sync for DriverState {}

static STATE: DriverState = DriverState {
    inner: RefCell::new(Slot::Empty),
};

impl DriverState {
    fn take(&self) -> Result<Option<Driver>, GfxError> {
        let mut slot = self.inner.borrow_mut();
        match core::mem::replace(&mut *slot, Slot::Busy) {
            Slot::Ready(driver) => Ok(Some(driver)),
            Slot::Empty => Ok(None),
            Slot::Busy => Err(GfxError::Io(String::from(
                "gpu.virtio: the device is busy with a concurrent operation; \
                 issue gfx operations sequentially",
            ))),
        }
    }

    fn put(&self, driver: Driver) {
        *self.inner.borrow_mut() = Slot::Ready(driver);
    }

    fn clear(&self) {
        *self.inner.borrow_mut() = Slot::Empty;
    }

    /// The mode if the device is up and idle; an explanatory typed error otherwise. For
    /// the synchronous `mode` query only — it cannot bring the device up (bring-up
    /// awaits; the same shape as `disk.virtio`'s `size` reporting 0 before first use).
    fn peek_mode(&self) -> Result<ModeInfo, GfxError> {
        match &*self.inner.borrow() {
            Slot::Ready(driver) => Ok(ModeInfo {
                width: driver.width,
                height: driver.height,
                stride: driver.width * BYTES_PER_PIXEL as u32,
                format: PixelFormat::Xrgb8888,
            }),
            Slot::Busy => Err(GfxError::Io(String::from(
                "gpu.virtio: the device is busy with a concurrent operation; \
                 ask for the mode again when it completes",
            ))),
            Slot::Empty => Err(GfxError::Io(String::from(
                "gpu.virtio: the device is not brought up yet — `mode` is synchronous \
                 and bring-up awaits, so issue one awaited gfx operation first (a \
                 zero-area `clear` is the cheapest) and ask again",
            ))),
        }
    }
}

/// Returns the driver to the slot when an operation ends — including by *cancellation*
/// (the operation's future dropped mid-await): without this, a cancelled operation would
/// leave the slot `Busy` forever. Before putting the driver back it consumes any
/// completion the device has *already posted* (a synchronous DMA read), settling the
/// common cancellation shape — the command finished while the cancel was landing. A
/// command the device is *still* processing cannot be settled here (`Drop` cannot
/// await); it stays visible as `avail_index != used_index`, and the next operation's
/// [`Driver::drain_stale`] waits it out before any shared descriptor or buffer is
/// reused. On the normal path the cursors already match and the resync is a no-op.
/// One side effect of cancellation landing inside the INTx `pci::wait`: the vector
/// moved into that wait drops with the cancelled future, so later commands complete in
/// polled mode — graceful degradation, not a failure.
struct DriverGuard {
    driver: Option<Driver>,
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if let Some(mut driver) = self.driver.take() {
            let raw = pci::dma_read(&driver.ring, USED_OFFSET + 2, 2);
            driver.used_index = u16::from_le_bytes([raw[0], raw[1]]);
            STATE.put(driver);
        }
    }
}

/// Run `f` over the brought-up driver, probing and initializing the device on first use
/// (the documented default state — there is no configure interface).
async fn with_driver<R>(
    f: impl AsyncFnOnce(&mut Driver) -> Result<R, GfxError>,
) -> Result<R, GfxError> {
    let driver = match STATE.take()? {
        Some(driver) => driver,
        None => match Driver::bring_up().await {
            Ok(driver) => driver,
            Err(message) => {
                STATE.clear();
                return Err(GfxError::Io(message));
            }
        },
    };
    let mut guard = DriverGuard {
        driver: Some(driver),
    };
    f(guard.driver.as_mut().expect("guard holds the driver")).await
    // `guard` drops here (or wherever this future is dropped), returning the driver.
}

impl Driver {
    /// Find, claim, and bring up the first virtio-gpu function visible through the
    /// granted PCI capability, then create + scan out the framebuffer resource. Every
    /// step reports a typed, labelled error — device weirdness is an `io` failure of
    /// the gfx operation, never a trap.
    async fn bring_up() -> Result<Driver, String> {
        let root = pci::default();
        let devices = pci_call("gpu.virtio: enumerate", pci::enumerate(&root)).await?;
        let target = devices
            .iter()
            .find(|d| d.vendor_id == VIRTIO_VENDOR && d.device_id == VIRTIO_GPU_MODERN)
            .ok_or_else(|| {
                String::from(
                    "gpu.virtio: no virtio-gpu function is visible through the granted \
                     pci capability (expected vendor 0x1af4, device 0x1050) — attach one \
                     to this boot (`cargo xtask qemu <arch> pci gpu`, or QEMU \
                     `-device virtio-gpu-pci`), or check that an attenuator composed in \
                     front of this driver allows the display's address",
                )
            })?;
        let address = target.address;
        let device = pci_call("gpu.virtio: open", pci::open(&root, address)).await?;

        // Walk the vendor-specific capabilities to find the virtio register windows.
        let (common, notify_base, notify_multiplier, isr) = find_windows(&device).await?;

        // Open each BAR the windows live in exactly once.
        let mut bar_indices: Vec<u8> = Vec::new();
        for index in [common.bar, notify_base.bar] {
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
            let bar = pci_call("gpu.virtio: open-bar", pci::open_bar(&device, index)).await?;
            bars.push((index, bar));
        }

        // The ring and command/response DMA pages. The framebuffer backing is allocated
        // by `scan_out` below, once GET_DISPLAY_INFO has told us the geometry.
        let ring = pci_call(
            "gpu.virtio: alloc-dma (ring)",
            pci::alloc_dma(&device, RING_BYTES),
        )
        .await?;
        let command = pci_call(
            "gpu.virtio: alloc-dma (command)",
            pci::alloc_dma(&device, CMD_BYTES),
        )
        .await?;
        let mut driver = Driver {
            _device: device,
            bars,
            common,
            notify: notify_base,
            notify_offset: 0,
            isr,
            interrupt: None,
            ring,
            command,
            framebuffer: None,
            queue_size: 0,
            avail_index: 0,
            used_index: 0,
            width: 0,
            height: 0,
            reported_polled_fallback: false,
        };
        driver.start(notify_multiplier).await?;

        // Interrupt delivery (same contract as disk.virtio): best-effort, falls back to
        // polling. Needs the ISR window so deliveries can be acknowledged.
        if driver.isr.is_some() {
            driver.interrupt =
                match pci::enable_interrupts(&driver._device, pci::InterruptKind::Intx, 1).await {
                    Ok(mut vectors) if !vectors.is_empty() => Some(vectors.remove(0)),
                    _ => None,
                };
        }

        // The display pipeline: mode → framebuffer backing → resource → scanout.
        driver.scan_out().await?;

        // One best-effort diagnostic line so a metal session shows what was probed.
        let handle = text::default();
        let line = format!(
            "gpu.virtio: virtio-gpu {}x{} xrgb8888, queue size {}, completion: {}",
            driver.width,
            driver.height,
            driver.queue_size,
            if driver.interrupt.is_some() {
                "INTx interrupt"
            } else {
                "polled"
            },
        );
        let _ = text::write(&handle, text::OutputStream::Out, &line);
        let _ = text::write(&handle, text::OutputStream::Out, "\n");
        Ok(driver)
    }

    /// Negotiate features and build control queue 0 — the device side of bring-up,
    /// identical in shape to disk.virtio's.
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
                return Err(String::from("gpu.virtio: device did not reset"));
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

        // Feature negotiation: accept exactly VIRTIO_F_VERSION_1 (the device may offer
        // EDID/VIRGL etc.; the 2D protocol below needs none of them).
        self.common_write(COMMON_DEVICE_FEATURE_SELECT, pci::AccessWidth::Dword, 1)
            .await?;
        let high_features = self
            .common_read(COMMON_DEVICE_FEATURE, pci::AccessWidth::Dword)
            .await?;
        if high_features & FEATURE_VERSION_1_HIGH == 0 {
            return Err(String::from(
                "gpu.virtio: the device does not offer VIRTIO_F_VERSION_1",
            ));
        }
        self.common_write(COMMON_DRIVER_FEATURE_SELECT, pci::AccessWidth::Dword, 0)
            .await?;
        self.common_write(COMMON_DRIVER_FEATURE, pci::AccessWidth::Dword, 0)
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
                "gpu.virtio: the device rejected the negotiated feature set",
            ));
        }

        // The device DMAs the rings, commands, and the framebuffer backing.
        pci_call(
            "gpu.virtio: set-bus-master",
            pci::set_bus_master(&self._device, true),
        )
        .await?;

        // Control queue 0.
        let queues = self
            .common_read(COMMON_NUM_QUEUES, pci::AccessWidth::Word)
            .await?;
        if queues == 0 {
            return Err(String::from("gpu.virtio: the device exposes no virtqueues"));
        }
        self.common_write(COMMON_QUEUE_SELECT, pci::AccessWidth::Word, 0)
            .await?;
        let max_size = self
            .common_read(COMMON_QUEUE_SIZE, pci::AccessWidth::Word)
            .await?;
        if max_size == 0 {
            return Err(String::from("gpu.virtio: virtqueue 0 is not available"));
        }
        let queue_size = core::cmp::min(max_size, u64::from(QUEUE_SIZE)) as u16;
        self.common_write(
            COMMON_QUEUE_SIZE,
            pci::AccessWidth::Word,
            u64::from(queue_size),
        )
        .await?;
        // The driver owns ring initialization (virtio 1.0 §3.1.1).
        pci::dma_write(&self.ring, 0, &[0u8; 1024]);
        let ring_address = pci::dma_address(&self.ring);
        self.write_address(COMMON_QUEUE_DESC, ring_address + DESC_OFFSET)
            .await?;
        self.write_address(COMMON_QUEUE_DRIVER, ring_address + AVAIL_OFFSET)
            .await?;
        self.write_address(COMMON_QUEUE_DEVICE, ring_address + USED_OFFSET)
            .await?;
        let queue_notify_off = self
            .common_read(COMMON_QUEUE_NOTIFY_OFF, pci::AccessWidth::Word)
            .await?;
        self.notify_offset = self.notify.offset + queue_notify_off * u64::from(notify_multiplier);
        pci::dma_write(&self.ring, AVAIL_OFFSET, &[0, 0, 0, 0]);
        self.common_write(COMMON_QUEUE_ENABLE, pci::AccessWidth::Word, 1)
            .await?;

        // Everything is in place: tell the device the driver is live.
        let live = with_features_ok | STATUS_DRIVER_OK;
        self.common_write(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte, live)
            .await?;
        self.queue_size = queue_size;
        Ok(())
    }

    /// The display pipeline: query scanout 0's geometry, allocate the framebuffer
    /// backing, create the host resource, attach the backing, and set the scanout.
    async fn scan_out(&mut self) -> Result<(), String> {
        // GET_DISPLAY_INFO → scanout 0's rect.
        let response = self
            .command(CMD_GET_DISPLAY_INFO, &[], RESP_OK_DISPLAY_INFO)
            .await?;
        // Each pmode is 24 bytes: rect{le32 x,y,w,h}, le32 enabled, le32 flags; scanout
        // 0's pmode is first.
        if response.len() < CTRL_HEADER + 24 {
            return Err(String::from(
                "gpu.virtio: short GET_DISPLAY_INFO response from the device",
            ));
        }
        let le32 = |offset: usize| {
            u32::from_le_bytes([
                response[offset],
                response[offset + 1],
                response[offset + 2],
                response[offset + 3],
            ])
        };
        let width = le32(CTRL_HEADER + 8);
        let height = le32(CTRL_HEADER + 12);
        let enabled = le32(CTRL_HEADER + 16);
        if width == 0 || height == 0 {
            return Err(format!(
                "gpu.virtio: the device reports no usable scanout-0 geometry \
                 ({width}x{height}, enabled {enabled})"
            ));
        }
        let framebuffer_bytes = u64::from(width) * u64::from(height) * BYTES_PER_PIXEL;
        if framebuffer_bytes > MAX_FRAMEBUFFER_BYTES {
            return Err(format!(
                "gpu.virtio: the {width}x{height} scanout needs {framebuffer_bytes} bytes of \
                 backing, over the v1 driver's single-allocation limit of \
                 {MAX_FRAMEBUFFER_BYTES} (multi-entry backing is the recorded follow-up)"
            ));
        }

        // The framebuffer backing — allocated exactly once, never dropped (see the
        // field's docs).
        self.framebuffer = Some(
            pci_call(
                "gpu.virtio: alloc-dma (framebuffer)",
                pci::alloc_dma(&self._device, framebuffer_bytes),
            )
            .await?,
        );

        // RESOURCE_CREATE_2D: resource RESOURCE_ID, xrgb8888, the scanout geometry.
        let mut create = [0u8; 16];
        create[0..4].copy_from_slice(&RESOURCE_ID.to_le_bytes());
        create[4..8].copy_from_slice(&FORMAT_B8G8R8X8.to_le_bytes());
        create[8..12].copy_from_slice(&width.to_le_bytes());
        create[12..16].copy_from_slice(&height.to_le_bytes());
        self.command(CMD_RESOURCE_CREATE_2D, &create, RESP_OK_NODATA)
            .await?;

        // RESOURCE_ATTACH_BACKING: one entry, the whole framebuffer.
        let mut attach = [0u8; 24];
        attach[0..4].copy_from_slice(&RESOURCE_ID.to_le_bytes());
        attach[4..8].copy_from_slice(&1u32.to_le_bytes());
        let framebuffer = self
            .framebuffer
            .as_ref()
            .ok_or_else(|| String::from("gpu.virtio: internal error: no framebuffer"))?;
        attach[8..16].copy_from_slice(&pci::dma_address(framebuffer).to_le_bytes());
        attach[16..20].copy_from_slice(&(framebuffer_bytes as u32).to_le_bytes());
        self.command(CMD_RESOURCE_ATTACH_BACKING, &attach, RESP_OK_NODATA)
            .await?;

        // SET_SCANOUT: scanout 0 shows the whole resource.
        let mut scanout = [0u8; 24];
        scanout[8..12].copy_from_slice(&width.to_le_bytes());
        scanout[12..16].copy_from_slice(&height.to_le_bytes());
        scanout[16..20].copy_from_slice(&SCANOUT_ID.to_le_bytes());
        scanout[20..24].copy_from_slice(&RESOURCE_ID.to_le_bytes());
        self.command(CMD_SET_SCANOUT, &scanout, RESP_OK_NODATA)
            .await?;

        self.width = width;
        self.height = height;
        Ok(())
    }

    // --- register access helpers ----------------------------------------------------------

    fn bar(&self, index: u8) -> Result<&pci::Bar, String> {
        self.bars
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, bar)| bar)
            .ok_or_else(|| String::from("gpu.virtio: internal error: BAR not opened"))
    }

    async fn common_read(&self, register: u64, width: pci::AccessWidth) -> Result<u64, String> {
        let bar = self.bar(self.common.bar)?;
        pci_call(
            "gpu.virtio: common config read",
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
            "gpu.virtio: common config write",
            pci::bar_write(bar, self.common.offset + register, width, value),
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

    async fn notify_queue(&self) -> Result<(), String> {
        let bar = self.bar(self.notify.bar)?;
        pci_call(
            "gpu.virtio: queue notify",
            pci::bar_write(bar, self.notify_offset, pci::AccessWidth::Word, 0),
        )
        .await
    }

    // --- one control-queue command ----------------------------------------------------------

    /// Settle a command a *cancelled* operation left in flight before the shared
    /// descriptors and DMA buffers are touched again. A cancellation (the operation's
    /// future dropped mid-await — reachable through any pci provider that defers, e.g.
    /// an interposed `pci.filtered`) can leave a command published, possibly not even
    /// kicked if the drop landed during the notify, with the device still free to DMA
    /// the descriptor chain and the command/response page at any later moment. Reusing
    /// those for a new command while the old one is live would let the device read torn
    /// state, and its eventual completion would be consumed by the new command's wait
    /// as if it were its own — the silent-misattribution class (plan/09 D34). The
    /// cursor pair makes the situation visible: `avail_index` counts published
    /// commands, `used_index` consumed completions, level between healthy operations
    /// (the [`DriverGuard`] resync keeps them level when the completion had already
    /// posted by the time the cancel landed). On divergence: kick once (idempotent —
    /// and the cancelled command may never have been kicked), then consume the leftover
    /// completion with the normal bounded wait machinery, discarding its response. The
    /// invariant this establishes: when an operation begins writing device-shared
    /// state, the device has posted completions for every previously published command,
    /// so no completion is ever attributed to a command other than the one that
    /// produced it, under any cancellation timing.
    async fn drain_stale(&mut self) -> Result<(), String> {
        while self.avail_index != self.used_index {
            self.notify_queue().await?;
            self.wait_for_completion("a cancelled control command")
                .await?;
        }
        Ok(())
    }

    /// Issue one 2D command (`payload` follows the 24-byte control header), wait for its
    /// completion, and check the response type. Returns the raw response bytes.
    async fn command(
        &mut self,
        command_type: u32,
        payload: &[u8],
        expected_response: u32,
    ) -> Result<Vec<u8>, String> {
        self.drain_stale().await?;
        // The command: header (type, everything else zero — no fences) + payload.
        let mut bytes = Vec::with_capacity(CTRL_HEADER + payload.len());
        bytes.extend_from_slice(&command_type.to_le_bytes());
        bytes.extend_from_slice(&[0u8; CTRL_HEADER - 4]);
        bytes.extend_from_slice(payload);
        pci::dma_write(&self.command, CMD_OFFSET, &bytes);
        // Preset the response header type to zero so a completion that somehow skipped
        // writing the response is caught by the type check below.
        pci::dma_write(&self.command, RESP_OFFSET, &[0u8; 4]);

        // Two-descriptor chain: the command (device-readable), the response
        // (device-writable).
        let command_address = pci::dma_address(&self.command);
        self.write_descriptor(
            0,
            command_address + CMD_OFFSET,
            bytes.len() as u32,
            DESC_F_NEXT,
            1,
        );
        self.write_descriptor(1, command_address + RESP_OFFSET, RESP_MAX, DESC_F_WRITE, 0);

        // Publish descriptor 0 in the avail ring, bump avail.idx, kick, wait.
        let slot = u64::from(self.avail_index % self.queue_size);
        pci::dma_write(&self.ring, AVAIL_OFFSET + 4 + 2 * slot, &0u16.to_le_bytes());
        self.avail_index = self.avail_index.wrapping_add(1);
        pci::dma_write(
            &self.ring,
            AVAIL_OFFSET + 2,
            &self.avail_index.to_le_bytes(),
        );
        self.notify_queue().await?;
        self.wait_for_completion("the control command").await?;

        let response = pci::dma_read(&self.command, RESP_OFFSET, u64::from(RESP_MAX));
        let response_type =
            u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
        if response_type != expected_response {
            return Err(format!(
                "gpu.virtio: command {command_type:#06x} answered {response_type:#06x} \
                 (expected {expected_response:#06x})"
            ));
        }
        Ok(response)
    }

    /// Wait for the in-flight command to complete (used ring advancing). Interrupt mode
    /// with polled fallback — the same contract and shape as disk.virtio.
    async fn wait_for_completion(&mut self, what: &str) -> Result<(), String> {
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
                match pci::wait(&vector).await {
                    Ok(_deliveries) => self.acknowledge_isr().await,
                    Err(_) => break false,
                }
            };
            self.interrupt = Some(vector);
            if completed {
                // Acknowledge unconditionally, not just on the wait branch: a completion
                // that was already in the used ring before (or between) waits never had
                // its ISR read, and an unread ISR keeps the level-sensitive INTx asserted
                // — the controller then records a delivery the moment the next `wait`
                // unmasks the line, and that stale delivery makes the wait return without
                // the completion it was called for (a spurious retry, and in the worst
                // case a permanent drift into the polled fallback). Read-to-clear is
                // idempotent, so acknowledging twice on the wait branch is harmless.
                self.acknowledge_isr().await;
                return Ok(());
            }
            // The interrupt path gave up (retries exhausted or the wait failed): say so
            // once per device conversation, because the polled fallback below can spin
            // for many seconds with the console blocked — silence here reads as a hang.
            if !self.reported_polled_fallback {
                self.reported_polled_fallback = true;
                diag(&format!(
                    "gpu.virtio: interrupt waits for {what} were not served; falling back \
                     to polling the used ring (this is slower but bounded)"
                ));
            }
        }

        let mut spins: u64 = 0;
        loop {
            if self.used_advanced() {
                if self.interrupt.is_some() {
                    self.acknowledge_isr().await;
                }
                return Ok(());
            }
            spins += 1;
            if spins > POLL_LIMIT {
                let used_raw = pci::dma_read(&self.ring, USED_OFFSET, 8);
                let status = self
                    .common_read(COMMON_DEVICE_STATUS, pci::AccessWidth::Byte)
                    .await
                    .unwrap_or(u64::MAX);
                return Err(format!(
                    "gpu.virtio: the device did not complete {what} (poll limit; \
                     device status {status:#x}, avail idx {}, used flags/idx/elem0 {used_raw:?})",
                    self.avail_index
                ));
            }
        }
    }

    /// Whether the used ring has advanced; consumes one completion when it has. Kept
    /// await-free (a synchronous DMA read) so a cancellation can never land mid-consume
    /// — D34's pattern point (4).
    fn used_advanced(&mut self) -> bool {
        let raw = pci::dma_read(&self.ring, USED_OFFSET + 2, 2);
        let used = u16::from_le_bytes([raw[0], raw[1]]);
        if used != self.used_index {
            self.used_index = self.used_index.wrapping_add(1);
            true
        } else {
            false
        }
    }

    /// Read (and thereby clear) the device's ISR status register. Best-effort.
    async fn acknowledge_isr(&self) {
        if let Some(isr) = &self.isr
            && let Ok(bar) = self.bar(isr.bar)
        {
            let _ = pci::bar_read(bar, isr.offset, pci::AccessWidth::Byte).await;
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

    // --- the gfx operations -----------------------------------------------------------------

    /// The framebuffer backing (always present once bring-up succeeded).
    fn fb(&self) -> Result<&pci::DmaBuffer, GfxError> {
        self.framebuffer
            .as_ref()
            .ok_or_else(|| GfxError::Io(String::from("gpu.virtio: internal error: no framebuffer")))
    }

    /// Validate `rect` against the mode; the framebuffer stride is `width * 4`.
    fn check_rect(&self, rect: &Rect) -> Result<(), GfxError> {
        let end_x = rect.x.checked_add(rect.width);
        let end_y = rect.y.checked_add(rect.height);
        let (Some(end_x), Some(end_y)) = (end_x, end_y) else {
            return Err(GfxError::OutOfBounds);
        };
        if end_x > self.width || end_y > self.height {
            return Err(GfxError::OutOfBounds);
        }
        Ok(())
    }

    /// Validate that `buffer_len` covers the rectangle's tightly packed pixels.
    fn check_buffer(rect: &Rect, buffer_len: u64) -> Result<u64, GfxError> {
        let needed = u64::from(rect.width) * u64::from(rect.height) * BYTES_PER_PIXEL;
        if buffer_len < needed {
            return Err(GfxError::BadBuffer(format!(
                "the rectangle needs {needed} bytes ({}x{} xrgb8888), the buffer holds \
                 {buffer_len}",
                rect.width, rect.height
            )));
        }
        Ok(needed)
    }

    /// TRANSFER_TO_HOST_2D + RESOURCE_FLUSH for `rect` — make the backing's pixels for
    /// that rectangle visible on the scanout.
    async fn flush_rect(&mut self, rect: &Rect) -> Result<(), GfxError> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        let stride = u64::from(self.width) * BYTES_PER_PIXEL;
        let offset = u64::from(rect.y) * stride + u64::from(rect.x) * BYTES_PER_PIXEL;

        // TRANSFER_TO_HOST_2D: rect, le64 offset (into the backing), resource id.
        let mut transfer = [0u8; 32];
        transfer[0..4].copy_from_slice(&rect.x.to_le_bytes());
        transfer[4..8].copy_from_slice(&rect.y.to_le_bytes());
        transfer[8..12].copy_from_slice(&rect.width.to_le_bytes());
        transfer[12..16].copy_from_slice(&rect.height.to_le_bytes());
        transfer[16..24].copy_from_slice(&offset.to_le_bytes());
        transfer[24..28].copy_from_slice(&RESOURCE_ID.to_le_bytes());
        self.command(CMD_TRANSFER_TO_HOST_2D, &transfer, RESP_OK_NODATA)
            .await
            .map_err(GfxError::Io)?;

        // RESOURCE_FLUSH: rect, resource id.
        let mut flush = [0u8; 24];
        flush[0..4].copy_from_slice(&rect.x.to_le_bytes());
        flush[4..8].copy_from_slice(&rect.y.to_le_bytes());
        flush[8..12].copy_from_slice(&rect.width.to_le_bytes());
        flush[12..16].copy_from_slice(&rect.height.to_le_bytes());
        flush[16..20].copy_from_slice(&RESOURCE_ID.to_le_bytes());
        self.command(CMD_RESOURCE_FLUSH, &flush, RESP_OK_NODATA)
            .await
            .map_err(GfxError::Io)?;
        Ok(())
    }
}

/// Walk the vendor-specific PCI capabilities for the virtio register windows: common,
/// notify (with its multiplier), and ISR (optional — interrupt mode needs it).
async fn find_windows(
    device: &pci::Device,
) -> Result<(Region, Region, u32, Option<Region>), String> {
    async fn read(
        device: &pci::Device,
        offset: u32,
        width: pci::AccessWidth,
    ) -> Result<u64, String> {
        pci_call(
            "gpu.virtio: config read",
            pci::config_read(device, offset, width),
        )
        .await
    }

    let mut common: Option<Region> = None;
    let mut notify: Option<(Region, u32)> = None;
    let mut isr: Option<Region> = None;

    let mut pointer = (read(device, PCI_CAP_POINTER, pci::AccessWidth::Byte).await? & 0xfc) as u32;
    let mut steps = 0;
    while pointer != 0 && steps < PCI_CAP_WALK_LIMIT {
        steps += 1;
        let id = read(device, pointer, pci::AccessWidth::Byte).await?;
        let next = (read(device, pointer + 1, pci::AccessWidth::Byte).await? & 0xfc) as u32;
        if id == PCI_CAP_ID_VENDOR {
            let cfg_type = read(device, pointer + 3, pci::AccessWidth::Byte).await?;
            let bar = read(device, pointer + 4, pci::AccessWidth::Byte).await? as u8;
            let offset = read(device, pointer + 8, pci::AccessWidth::Dword).await?;
            match cfg_type {
                VIRTIO_PCI_CAP_COMMON if common.is_none() => {
                    common = Some(Region { bar, offset });
                }
                VIRTIO_PCI_CAP_NOTIFY if notify.is_none() => {
                    let multiplier =
                        read(device, pointer + 16, pci::AccessWidth::Dword).await? as u32;
                    notify = Some((Region { bar, offset }, multiplier));
                }
                VIRTIO_PCI_CAP_ISR if isr.is_none() => {
                    isr = Some(Region { bar, offset });
                }
                _ => {}
            }
        }
        pointer = next;
    }

    let common = common.ok_or_else(|| {
        String::from("gpu.virtio: the function has no virtio common-config capability")
    })?;
    let (notify, multiplier) = notify
        .ok_or_else(|| String::from("gpu.virtio: the function has no virtio notify capability"))?;
    Ok((common, notify, multiplier, isr))
}

// ------------------------------------------------------------------------------------------
// The exported eo9:gfx provider
// ------------------------------------------------------------------------------------------

/// The `gpu.virtio` provider.
struct Stub;

/// The root-handle resource: a token referring to the claimed and scanned-out device.
struct VirtioGfx;

impl types::Guest for Stub {
    type GfxImpl = VirtioGfx;
}

impl types::GuestGfxImpl for VirtioGfx {}

impl gfx::Guest for Stub {
    fn default() -> types::GfxImpl {
        types::GfxImpl::new(VirtioGfx)
    }

    fn mode(_g: gfx::GfxImplBorrow<'_>) -> Result<ModeInfo, GfxError> {
        // `mode` is a synchronous query and bring-up awaits, so it answers only once the
        // device is up (any awaited operation brings it up); before that it reports a
        // typed `io` error explaining the wake-up dance rather than trapping or
        // inventing a fake mode. The draw example wakes the device with a zero-area
        // `clear`, then asks.
        STATE.peek_mode()
    }

    async fn present(
        _g: gfx::GfxImplBorrow<'_>,
        dst: Rect,
        src: Buffer,
    ) -> (Buffer, Result<(), GfxError>) {
        // Copy out of the buffer before driving the device (never call back into the
        // buffers import while the driver is out of its slot).
        let src_len = src.len();
        let needed = u64::from(dst.width) * u64::from(dst.height) * BYTES_PER_PIXEL;
        let bytes = if src_len >= needed && needed > 0 {
            src.read(0, needed)
        } else {
            Vec::new()
        };
        let result = with_driver(async |driver| {
            driver.check_rect(&dst)?;
            Driver::check_buffer(&dst, src_len)?;
            // Copy the tight rows into the backing at the framebuffer stride.
            let stride = u64::from(driver.width) * BYTES_PER_PIXEL;
            let row_bytes = u64::from(dst.width) * BYTES_PER_PIXEL;
            let start = u64::from(dst.y) * stride + u64::from(dst.x) * BYTES_PER_PIXEL;
            for row in 0..u64::from(dst.height) {
                let row_start = (row * row_bytes) as usize;
                pci::dma_write(
                    driver.fb()?,
                    start + row * stride,
                    &bytes[row_start..row_start + row_bytes as usize],
                );
            }
            driver.flush_rect(&dst).await
        })
        .await;
        (src, result)
    }

    async fn read(
        _g: gfx::GfxImplBorrow<'_>,
        src: Rect,
        dst: Buffer,
    ) -> (Buffer, Result<(), GfxError>) {
        // Gather while the driver is out of its slot, write to the buffer afterwards.
        let dst_len = dst.len();
        let gathered = with_driver(async |driver| {
            driver.check_rect(&src)?;
            Driver::check_buffer(&src, dst_len)?;
            let stride = u64::from(driver.width) * BYTES_PER_PIXEL;
            let row_bytes = u64::from(src.width) * BYTES_PER_PIXEL;
            let start = u64::from(src.y) * stride + u64::from(src.x) * BYTES_PER_PIXEL;
            let mut out = Vec::with_capacity((row_bytes * u64::from(src.height)) as usize);
            for row in 0..u64::from(src.height) {
                out.extend_from_slice(&pci::dma_read(
                    driver.fb()?,
                    start + row * stride,
                    row_bytes,
                ));
            }
            Ok(out)
        })
        .await;
        let result = match gathered {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    dst.write(0, &bytes);
                }
                Ok(())
            }
            Err(err) => Err(err),
        };
        (dst, result)
    }

    async fn clear(_g: gfx::GfxImplBorrow<'_>, dst: Rect, color: u32) -> Result<(), GfxError> {
        with_driver(async |driver| {
            driver.check_rect(&dst)?;
            if dst.width == 0 || dst.height == 0 {
                return Ok(());
            }
            // One row of the solid color, written at each scanline of the rectangle.
            let mut row = Vec::with_capacity(dst.width as usize * 4);
            for _ in 0..dst.width {
                row.extend_from_slice(&color.to_le_bytes());
            }
            let stride = u64::from(driver.width) * BYTES_PER_PIXEL;
            let start = u64::from(dst.y) * stride + u64::from(dst.x) * BYTES_PER_PIXEL;
            for line in 0..u64::from(dst.height) {
                pci::dma_write(driver.fb()?, start + line * stride, &row);
            }
            driver.flush_rect(&dst).await
        })
        .await
    }
}

export!(Stub);
