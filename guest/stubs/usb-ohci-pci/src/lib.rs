//! `usb.ohci-pci` — the OHCI USB host driver over a PCI function (the QEMU lane).
//!
//! Targets the crate-local `eo9:usb-ohci-pci/ohci-pci` world: imports `eo9:pci/pci`
//! plus `eo9:text/text` for one bring-up diagnostic line, and exports `eo9:usb/usb`
//! backed by an OHCI controller enumerated as a PCI function — QEMU's
//! `-device pci-ohci`, class 0c.03.10 (eo9-ohci's `PCI_*` constants). This shell is
//! ~200 lines of claim path + error mapping: the whole device conversation (takeover,
//! schedules, transfers, enumeration) is the shared, host-tested
//! [`eo9_ohci::driver::Ohci`] core, generic over [`RegionIo`] — the board shell
//! `usb.ohci` wraps the same core over `eo9:platform`
//! (docs/board/usb-ohci-plan.md §2).
//!
//! D46 driver discipline verbatim: the take/put driver slot with a cancellation-safe
//! guard, the bring-up claim (claim the controller on first use; a failed bring-up
//! releases the claim so the next use retries), bounded polls (inside the core),
//! typed errors never traps, and the short-poll "empty result = nothing waiting"
//! contract on the interrupt endpoint.
//!
//! Events over polls (timer-crutch audit A1/A4): bring-up asks the provider for one
//! INTx vector (`pci::enable-interrupts`, the same call `disk.virtio` proves on
//! aarch64-virt/riscv64); granted, the core unmasks WDH/RHSC and `read` /
//! `watch-ports` park on the controller's interrupt instead of answering empty for
//! the consumer to pace. Refused (`unsupported`, e.g. x86_64), everything keeps the
//! original short-poll shape — capability-gated, no cfg. A report found only by the
//! post-timeout drain is reported as a loud `liveness:` line (owner doctrine: a
//! fallback rescuing work the event path owed is a bug surfacing, never silent).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;
use eo9_ohci::driver::{DriverError, Ohci, RegionIo, WaitOutcome};
use eo9_ohci::schedule::arena;
use eo9_ohci::setup::SetupPacket;

wit_bindgen::generate!({
    world: "ohci-pci",
    path: "wit",
    generate_all,
});

use eo9::pci::pci;
use eo9::text::text;
use exports::eo9::usb::types;
use exports::eo9::usb::usb::{self, ControllerInfo, PortStatus, UsbError};

// ------------------------------------------------------------------------------------------
// The RegionIo adapter: OHCI registers live in BAR 0, the DMA arena in one alloc-dma
// ------------------------------------------------------------------------------------------

/// Everything the claimed controller hands us. The PCI handles are owned resources:
/// dropping them would clear bus mastering (the provider's quiesce contract), so the
/// adapter keeps them for the driver's lifetime.
struct Adapter {
    bar: pci::Bar,
    dma: pci::DmaBuffer,
    dma_base: u32,
    /// The INTx vector the provider granted at bring-up, or `None` where the
    /// provider routes no interrupts (`unsupported`, e.g. x86_64) — the polled
    /// fallback, decided once by capability availability.
    interrupt: Option<pci::Interrupt>,
    /// Kept alive: the claim and the bus-master licence ride this handle.
    _device: pci::Device,
}

impl RegionIo for Adapter {
    type Error = pci::PciError;

    async fn read32(&mut self, offset: u64) -> Result<u32, pci::PciError> {
        pci::bar_read(&self.bar, offset, pci::AccessWidth::Dword)
            .await
            .map(|value| value as u32)
    }

    async fn write32(&mut self, offset: u64, value: u32) -> Result<(), pci::PciError> {
        pci::bar_write(&self.bar, offset, pci::AccessWidth::Dword, u64::from(value)).await
    }

    fn dma_base(&self) -> u32 {
        self.dma_base
    }

    fn dma_write(&mut self, offset: u64, bytes: &[u8]) {
        pci::dma_write(&self.dma, offset, bytes);
    }

    fn dma_read(&mut self, offset: u64, buf: &mut [u8]) {
        let bytes = pci::dma_read(&self.dma, offset, buf.len() as u64);
        buf.copy_from_slice(&bytes);
    }

    async fn wait_interrupt(&mut self) -> Result<WaitOutcome, pci::PciError> {
        let Some(vector) = &self.interrupt else {
            return Ok(WaitOutcome::Unsupported);
        };
        match pci::wait(vector).await {
            // A delivery (possibly coalesced or shared-line spurious): the core
            // re-checks the cause registers.
            Ok(_deliveries) => Ok(WaitOutcome::Delivered),
            // Bound expiry (the kernel's INTX_WAIT_BOUND, the SPEC awaits-are-bounded
            // rule) or any other wait failure: ONE poll round, then the caller may
            // wait again — a persistently failing wait degrades to a provider-bound-
            // paced poll loop, never a hang and never a hot spin.
            // SOUNDNESS NOTE: "never a hot spin" assumes a wait error takes the
            // provider's bound or the handle stays valid for the task's life — true
            // today (vectors are task-scoped). If a vector can ever die mid-task
            // (instant errors), this arm must degrade instead (GAPS "usb event-mode
            // wait errors map to TimedOut").
            Err(_) => Ok(WaitOutcome::TimedOut),
        }
    }
}

type Driver = Ohci<Adapter>;

/// Find, claim, and bring up the first OHCI-class PCI function. Every step reports a
/// typed, labelled error — device weirdness is a typed `usb-error`, never a trap.
async fn probe() -> Result<Driver, UsbError> {
    let labelled = |what: &'static str| {
        move |error: pci::PciError| match error {
            pci::PciError::Denied => UsbError::Denied,
            other => UsbError::Io(format!("usb.ohci-pci: {what}: {other:?}")),
        }
    };

    let root = pci::default();
    let devices = pci::enumerate(&root).await.map_err(labelled("enumerate"))?;
    let target = devices
        .iter()
        .find(|device| {
            device.class_code == eo9_ohci::PCI_CLASS_SERIAL_BUS
                && device.subclass == eo9_ohci::PCI_SUBCLASS_USB
                && device.prog_if == eo9_ohci::PCI_PROGIF_OHCI
        })
        .ok_or(UsbError::NoController)?;

    let device = pci::open(&root, target.address)
        .await
        .map_err(|error| match error {
            pci::PciError::Busy => UsbError::Busy,
            other => labelled("open")(other),
        })?;

    // The OHCI operational registers are BAR 0 (a 4 KiB-class memory BAR on QEMU's
    // pci-ohci; the OHCI register file itself is 256 bytes + 4 per port).
    let bars = pci::bars(&device).await.map_err(labelled("bars"))?;
    let register_bar = bars
        .iter()
        .find(|bar| bar.index == 0 && matches!(bar.kind, pci::BarKind::Memory))
        .ok_or_else(|| {
            UsbError::Io(String::from(
                "usb.ohci-pci: the claimed function has no memory BAR 0 (not an OHCI?)",
            ))
        })?;
    let bar = pci::open_bar(&device, register_bar.index)
        .await
        .map_err(labelled("open-bar"))?;

    // The controller masters the bus for every schedule fetch and data transfer.
    pci::set_bus_master(&device, true)
        .await
        .map_err(labelled("set-bus-master"))?;

    // One arena holds the HCCA, EDs, TDs, and buffers (eo9_ohci::schedule::arena).
    let dma = pci::alloc_dma(&device, arena::SIZE)
        .await
        .map_err(labelled("alloc-dma"))?;
    let base = pci::dma_address(&dma);
    if base + arena::SIZE > u64::from(u32::MAX) {
        // OHCI pointers are 32-bit; the kernel's heap sits below 4 GiB on every
        // supported machine, so this is a misconfiguration, reported typed.
        return Err(UsbError::Io(format!(
            "usb.ohci-pci: the DMA arena at {base:#x} is beyond the controller's 32-bit reach"
        )));
    }

    // Interrupt delivery: ask the provider for one INTx vector (the disk.virtio
    // shape). `unsupported` — or any other refusal — means this platform/provider
    // does not route PCI interrupts; the driver then keeps the polled shape and the
    // consumers pace their own reads (capability-gated, never cfg-gated).
    let interrupt = match pci::enable_interrupts(&device, pci::InterruptKind::Intx, 1).await {
        Ok(mut vectors) if !vectors.is_empty() => Some(vectors.remove(0)),
        _ => None,
    };
    let events = interrupt.is_some();

    let mut driver = Ohci::new(Adapter {
        bar,
        dma,
        dma_base: base as u32,
        interrupt,
        _device: device,
    });
    let info = driver.bring_up().await.map_err(map_driver_error)?;
    if events {
        // Unmask exactly the consumed causes (WDH + RHSC, under MIE): reads and the
        // port watch park on the controller interrupt from here on.
        driver.enable_events().await.map_err(map_driver_error)?;
    }

    // One diagnostic line, like net.rtl8125's: the bring-up facts a transcript needs
    // (including how completions are observed — the disk.virtio convention).
    let handle = text::default();
    let _ = text::write(
        &handle,
        text::OutputStream::Out,
        &format!(
            "usb.ohci-pci: OHCI {:x}.{:x} at pci {:04x}:{:02x}:{:02x}.{} - {} root-hub \
             port(s), events: {}\n",
            info.revision >> 4,
            info.revision & 0xf,
            target.address.segment,
            target.address.bus,
            target.address.device,
            target.address.function,
            info.ports,
            if events { "INTx interrupt" } else { "polled" },
        ),
    );
    Ok(driver)
}

/// Report a rescue loudly (owner doctrine): a report that arrived only through the
/// post-timeout drain is work the interrupt owed — a `liveness:` line, first
/// occurrence and every 16th after, the kernel detectors' rate-limit convention.
fn report_rescue() {
    use core::sync::atomic::{AtomicU32, Ordering};
    static RESCUES: AtomicU32 = AtomicU32::new(0);
    let n = RESCUES.fetch_add(1, Ordering::Relaxed);
    if n.is_multiple_of(16) {
        let handle = text::default();
        let _ = text::write(
            &handle,
            text::OutputStream::Out,
            &format!(
                "liveness: usb.ohci-pci: a HID report arrived only via the post-timeout \
                 drain poll - the WDH interrupt owed this wake (n={})\n",
                n + 1,
            ),
        );
    }
}

/// Map the shared core's typed failures onto the WIT's vocabulary.
fn map_driver_error(error: DriverError<pci::PciError>) -> UsbError {
    match error {
        DriverError::Io(pci::PciError::Denied) => UsbError::Denied,
        DriverError::Io(other) => UsbError::Io(format!("usb.ohci-pci: pci: {other:?}")),
        DriverError::NotOhci { revision } => UsbError::Io(format!(
            "usb.ohci-pci: HcRevision {revision:#x} is not an OHCI 1.x controller"
        )),
        DriverError::Timeout(_) => UsbError::Timeout,
        DriverError::Stall => UsbError::Stall,
        DriverError::Transfer(code) => {
            UsbError::Io(format!("usb.ohci-pci: transfer error: {code:?}"))
        }
        DriverError::NoSuchPort | DriverError::NotConnected => UsbError::NotFound,
        DriverError::Hub(limitation) => {
            UsbError::Io(format!("usb.ohci-pci: hub traversal: {limitation}"))
        }
        DriverError::Enumeration(error) => {
            UsbError::Io(format!("usb.ohci-pci: enumeration: {error:?}"))
        }
        DriverError::DoneQueueCorrupt => {
            UsbError::Io(String::from("usb.ohci-pci: corrupt done queue"))
        }
    }
}

// ------------------------------------------------------------------------------------------
// Driver slot (take/put with a cancellation-safe guard — the D46 pattern verbatim)
// ------------------------------------------------------------------------------------------

/// The driver's home between operations: an operation takes the driver *out* of the
/// slot for its duration (a `ProviderState` borrow must never be held across an
/// await).
struct DriverSlot {
    driver: Option<Driver>,
    /// Whether bring-up has been claimed (set before the first `probe().await` so a
    /// concurrent first use cannot start a second probe; cleared again if bring-up
    /// fails, so the next use retries — plan/09 D41).
    brought_up: bool,
}

static STATE: ProviderState<DriverSlot> = ProviderState::new();

/// Puts the driver back when the operation that took it finishes — including by
/// cancellation (the future dropped mid-await), so a cancelled operation can never
/// leave the slot empty.
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

enum SlotView {
    Ready(Driver),
    Busy,
    NeedBringUp,
}

/// Take the driver for one operation, probing and bringing the controller up on first
/// use (the documented default — there is no configure interface). A second
/// activation arriving while one is parked mid-operation gets a typed error, never a
/// re-entrant borrow trap.
async fn acquire() -> Result<DriverGuard, UsbError> {
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
        SlotView::Busy => Err(UsbError::Busy),
        SlotView::NeedBringUp => {
            // Armed before the first await, defused on success: an error return or a
            // future dropped mid-bring-up clears the claim and the next use retries.
            let claim = BringUpClaim { armed: true };
            let driver = probe().await?;
            claim.defuse();
            Ok(DriverGuard(Some(driver)))
        }
    }
}

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

// ------------------------------------------------------------------------------------------
// The exported eo9:usb surface
// ------------------------------------------------------------------------------------------

/// The provider.
struct Shell;

/// The root-handle resource: a token — the state lives in the driver slot.
struct UsbRoot;

/// An attached device: what control transfers need to address it, plus what the
/// hub-traversal path needs (class + the configuration blob attach validated).
struct ShellDevice {
    address: u8,
    max_packet: u8,
    low_speed: bool,
    class: u8,
    config: Vec<u8>,
}

/// The opened interrupt-IN endpoint (one per controller in v1 — the core owns its
/// re-arm state). Carries whether reads park on the controller interrupt, decided
/// once at bring-up by capability availability (the consumer's pacing signal).
struct ShellEndpoint {
    events: bool,
}

impl types::Guest for Shell {
    type UsbImpl = UsbRoot;
}

impl types::GuestUsbImpl for UsbRoot {}

impl usb::GuestDevice for ShellDevice {}
impl usb::GuestEndpoint for ShellEndpoint {}

/// The largest control-transfer payload the shell accepts (the arena's control buffer).
const MAX_CONTROL_BYTES: u16 = arena::CONTROL_BUFFER_LEN as u16;

impl usb::Guest for Shell {
    type Device = ShellDevice;
    type Endpoint = ShellEndpoint;

    fn default() -> types::UsbImpl {
        types::UsbImpl::new(UsbRoot)
    }

    async fn controller(_u: types::UsbImplBorrow<'_>) -> Result<ControllerInfo, UsbError> {
        let guard = acquire().await?;
        let info = guard
            .info()
            .expect("bring-up populates the controller info");
        Ok(ControllerInfo {
            revision: info.revision,
            ports: info.ports,
        })
    }

    async fn port(_u: types::UsbImplBorrow<'_>, port: u8) -> Result<PortStatus, UsbError> {
        let mut guard = acquire().await?;
        let status = guard.port_status(port).await.map_err(map_driver_error)?;
        Ok(PortStatus {
            connected: status.connected,
            enabled: status.enabled,
            powered: status.powered,
            low_speed: status.low_speed,
            connect_change: status.connect_change,
        })
    }

    async fn watch_ports(_u: types::UsbImplBorrow<'_>) -> Result<usb::WatchOutcome, UsbError> {
        let mut guard = acquire().await?;
        let outcome = guard.wait_port_change().await.map_err(map_driver_error)?;
        Ok(match outcome {
            WaitOutcome::Delivered => usb::WatchOutcome::Changed,
            WaitOutcome::TimedOut => usb::WatchOutcome::TimedOut,
            WaitOutcome::Unsupported => usb::WatchOutcome::Unsupported,
        })
    }

    async fn attach(_u: types::UsbImplBorrow<'_>, port: u8) -> Result<usb::Device, UsbError> {
        let mut guard = acquire().await?;
        let mut config = [0u8; eo9_ohci::enumerate::MAX_CONFIG_BYTES];
        let (attached, config_len) = guard
            .attach(port, &mut config)
            .await
            .map_err(map_driver_error)?;
        Ok(usb::Device::new(ShellDevice {
            address: attached.enumerated.address,
            max_packet: attached.enumerated.max_packet_ep0,
            low_speed: attached.low_speed,
            class: attached.enumerated.device.class,
            config: config[..config_len].to_vec(),
        }))
    }

    async fn attach_child(d: usb::DeviceBorrow<'_>) -> Result<usb::Device, UsbError> {
        let hub = d.get::<ShellDevice>();
        let (address, max_packet, low_speed, class, hub_config) = (
            hub.address,
            hub.max_packet,
            hub.low_speed,
            hub.class,
            hub.config.clone(),
        );
        let mut guard = acquire().await?;
        let mut config = [0u8; eo9_ohci::enumerate::MAX_CONFIG_BYTES];
        let (child, config_len) = guard
            .attach_hub_child(
                address,
                max_packet,
                low_speed,
                class,
                &hub_config,
                &mut config,
            )
            .await
            .map_err(map_driver_error)?;
        Ok(usb::Device::new(ShellDevice {
            address: child.enumerated.address,
            max_packet: child.enumerated.max_packet_ep0,
            low_speed: child.low_speed,
            class: child.enumerated.device.class,
            config: config[..config_len].to_vec(),
        }))
    }

    async fn control_in(
        d: usb::DeviceBorrow<'_>,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Result<Vec<u8>, UsbError> {
        let device = d.get::<ShellDevice>();
        if request_type & 0x80 == 0 {
            return Err(UsbError::Unsupported);
        }
        let length = length.min(MAX_CONTROL_BYTES);
        let (address, max_packet, low_speed) =
            (device.address, device.max_packet, device.low_speed);
        let mut guard = acquire().await?;
        let mut buffer = [0u8; MAX_CONTROL_BYTES as usize];
        let received = guard
            .control(
                address,
                max_packet,
                low_speed,
                SetupPacket {
                    request_type,
                    request,
                    value,
                    index,
                    length,
                },
                &mut buffer[..length as usize],
            )
            .await
            .map_err(map_driver_error)?;
        Ok(buffer[..received].to_vec())
    }

    async fn control_out(
        d: usb::DeviceBorrow<'_>,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
    ) -> Result<(), UsbError> {
        let device = d.get::<ShellDevice>();
        if request_type & 0x80 != 0 {
            return Err(UsbError::Unsupported);
        }
        if !data.is_empty() {
            // No v1 request carries an OUT data stage (SET_ADDRESS/CONFIGURATION/
            // PROTOCOL/IDLE are all zero-length); typed, honest, recorded.
            return Err(UsbError::Unsupported);
        }
        let (address, max_packet, low_speed) =
            (device.address, device.max_packet, device.low_speed);
        let mut guard = acquire().await?;
        guard
            .control(
                address,
                max_packet,
                low_speed,
                SetupPacket {
                    request_type,
                    request,
                    value,
                    index,
                    length: 0,
                },
                &mut [],
            )
            .await
            .map_err(map_driver_error)?;
        Ok(())
    }

    async fn open_interrupt_in(
        d: usb::DeviceBorrow<'_>,
        endpoint: u8,
        max_packet: u16,
        interval_ms: u8,
    ) -> Result<usb::Endpoint, UsbError> {
        let device = d.get::<ShellDevice>();
        let (address, low_speed) = (device.address, device.low_speed);
        let mut guard = acquire().await?;
        guard
            .open_interrupt_in(address, low_speed, endpoint, max_packet, interval_ms)
            .await
            .map_err(map_driver_error)?;
        Ok(usb::Endpoint::new(ShellEndpoint {
            events: guard.events(),
        }))
    }

    fn event_driven(e: usb::EndpointBorrow<'_>) -> bool {
        e.get::<ShellEndpoint>().events
    }

    async fn read(_e: usb::EndpointBorrow<'_>) -> Result<Vec<u8>, UsbError> {
        let mut guard = acquire().await?;
        let mut report = [0u8; arena::INTERRUPT_BUFFER_LEN as usize];
        match guard.read_report(&mut report).await {
            Ok(read) => {
                if read.rescued {
                    // The fallback poll found work the interrupt owed: loud, never
                    // silent (the scavenge_rx lesson).
                    report_rescue();
                }
                // With events live this parked on WDH (empty = bounded wait expired,
                // call again); without, it was one short poll and the consumer owns
                // the wait policy (the recv-frame contract).
                Ok(read
                    .length
                    .map(|length| report[..length].to_vec())
                    .unwrap_or_default())
            }
            Err(error) => Err(map_driver_error(error)),
        }
    }
}

export!(Shell);
