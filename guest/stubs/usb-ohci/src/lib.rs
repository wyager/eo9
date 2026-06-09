//! `usb.ohci` — the OHCI USB host driver over a memory-mapped platform region (the
//! board lane).
//!
//! Targets the crate-local `eo9:usb-ohci/ohci-platform` world: imports
//! `eo9:platform/platform` plus `eo9:text/text` for one bring-up diagnostic line, and
//! exports `eo9:usb/usb` backed by an OHCI controller claimed by region name (the
//! RK3588 usb-host blocks; the board profile's table lands with the M1 lane — under
//! QEMU this shell refuses typed with `no-controller`, which is the M0 transcript).
//! This shell is ~200 lines of claim path + error mapping: the whole device
//! conversation (takeover, schedules, transfers, enumeration) is the shared,
//! host-tested [`eo9_ohci::driver::Ohci`] core, generic over [`RegionIo`] — the QEMU
//! shell `usb.ohci-pci` wraps the same core over `eo9:pci`
//! (docs/board/usb-ohci-plan.md §2).
//!
//! D46 driver discipline verbatim: the take/put driver slot with a cancellation-safe
//! guard, the bring-up claim (claim the controller on first use; a failed bring-up
//! releases the claim so the next use retries), bounded polls (inside the core),
//! typed errors never traps, and the short-poll "empty result = nothing waiting"
//! contract on the interrupt endpoint.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;
use eo9_ohci::driver::{DriverError, Ohci, RegionIo};
use eo9_ohci::schedule::arena;
use eo9_ohci::setup::SetupPacket;

wit_bindgen::generate!({
    world: "ohci-platform",
    path: "wit",
    generate_all,
});

use eo9::platform::platform;
use eo9::text::text;
use exports::eo9::usb::types;
use exports::eo9::usb::usb::{self, ControllerInfo, PortStatus, UsbError};
use exports::eo9::usb_ohci::ohci_config;

// ------------------------------------------------------------------------------------------
// The RegionIo adapter: OHCI registers are the claimed region, the DMA arena one
// alloc-dma
// ------------------------------------------------------------------------------------------

/// Everything the claimed controller hands us. The handles are owned resources:
/// dropping the region would release the machine-wide claim, so the adapter keeps
/// them for the driver's lifetime.
struct Adapter {
    region: platform::Region,
    dma: platform::DmaBuffer,
    dma_base: u32,
}

impl RegionIo for Adapter {
    type Error = platform::PlatformError;

    async fn read32(&mut self, offset: u64) -> Result<u32, platform::PlatformError> {
        platform::read(&self.region, offset, platform::AccessWidth::Dword)
            .await
            .map(|value| value as u32)
    }

    async fn write32(&mut self, offset: u64, value: u32) -> Result<(), platform::PlatformError> {
        platform::write(
            &self.region,
            offset,
            platform::AccessWidth::Dword,
            u64::from(value),
        )
        .await
    }

    fn dma_base(&self) -> u32 {
        self.dma_base
    }

    fn dma_write(&mut self, offset: u64, bytes: &[u8]) {
        platform::dma_write(&self.dma, offset, bytes);
    }

    fn dma_read(&mut self, offset: u64, buf: &mut [u8]) {
        let bytes = platform::dma_read(&self.dma, offset, buf.len() as u64);
        buf.copy_from_slice(&bytes);
    }
}

type Driver = Ohci<Adapter>;

/// Find, claim, and bring up the first OHCI platform region. Every step reports a
/// typed, labelled error — device weirdness is a typed `usb-error`, never a trap.
async fn probe() -> Result<Driver, UsbError> {
    let labelled = |what: &'static str| {
        move |error: platform::PlatformError| match error {
            platform::PlatformError::Denied => UsbError::Denied,
            platform::PlatformError::Busy => UsbError::Busy,
            other => UsbError::Io(format!("usb.ohci: {what}: {other:?}")),
        }
    };

    let root = platform::default();
    let regions = platform::enumerate(&root)
        .await
        .map_err(labelled("enumerate"))?;

    // Defensive EHCI CONFIGFLAG clear FIRST (EHCI 1.0 §4.2: CF=0 routes every port to
    // the companion OHCI — the reset default, but a prior vendor-U-Boot `usb start`
    // may have set CF=1, which would leave the OHCI seeing no devices at all; the
    // plan's risk 6). One claim → one write → release per granted `-ehci` region;
    // every failure is silently skipped (defensive, not load-bearing — and absent
    // regions are simply not in the grant, e.g. all of QEMU).
    for ehci in regions
        .iter()
        .filter(|region| region.name.ends_with("-ehci"))
    {
        let Ok(claimed) = platform::claim(&root, ehci.name.clone()).await else {
            continue;
        };
        // HCCAPBASE dword (EHCI 1.0 §2.2): CAPLENGTH in [7:0] locates the
        // operational registers; CONFIGFLAG is op + 0x40 (§2.3.8).
        let Ok(capbase) = platform::read(&claimed, 0, platform::AccessWidth::Dword).await else {
            continue;
        };
        let configflag = (capbase & 0xff) + 0x40;
        if configflag + 4 <= ehci.size
            && platform::write(&claimed, configflag, platform::AccessWidth::Dword, 0)
                .await
                .is_ok()
        {
            let handle = text::default();
            let _ = text::write(
                &handle,
                text::OutputStream::Out,
                &format!(
                    "usb.ohci: cleared {} CONFIGFLAG (ports route to the companion OHCI)\n",
                    ehci.name
                ),
            );
        }
        // `claimed` drops here: the EHCI region is released, never driven.
    }

    // Which OHCI: the configured region name if `configure(region)` bound one, else
    // the first granted region whose name ends in `-ohci` (the board table orders
    // usb-host1-ohci first; narrowing is also the boot grant's job:
    // `platform=usb-host1-ohci`).
    let target = match configured_region() {
        Some(name) => regions
            .iter()
            .find(|region| region.name == name)
            .ok_or_else(|| {
                UsbError::Io(format!(
                    "usb.ohci: the configured region `{name}` is not in this grant \
                     (granted: {})",
                    region_names(&regions),
                ))
            })?,
        None => regions
            .iter()
            .find(|region| region.name.ends_with("-ohci") || region.name == "ohci")
            .ok_or(UsbError::NoController)?,
    };

    let region = platform::claim(&root, target.name.clone())
        .await
        .map_err(labelled("claim"))?;

    // One arena holds the HCCA, EDs, TDs, and buffers (eo9_ohci::schedule::arena).
    let dma = platform::alloc_dma(&region, arena::SIZE)
        .await
        .map_err(labelled("alloc-dma"))?;
    let base = platform::dma_address(&dma);
    if base + arena::SIZE > u64::from(u32::MAX) {
        // OHCI pointers are 32-bit; the kernel RAM window sits below 4 GiB on the
        // board (docs/board/usb-ohci-plan.md §0), so this is a misconfiguration.
        return Err(UsbError::Io(format!(
            "usb.ohci: the DMA arena at {base:#x} is beyond the controller's 32-bit reach"
        )));
    }

    let name = target.name.clone();
    let mut driver = Ohci::new(Adapter {
        region,
        dma,
        dma_base: base as u32,
    });
    let info = driver.bring_up().await.map_err(map_driver_error)?;

    // One diagnostic line, like net.rtl8125's: the bring-up facts a transcript needs.
    let handle = text::default();
    let _ = text::write(
        &handle,
        text::OutputStream::Out,
        &format!(
            "usb.ohci: OHCI {:x}.{:x} on region {name} - {} root-hub port(s)\n",
            info.revision >> 4,
            info.revision & 0xf,
            info.ports,
        ),
    );
    Ok(driver)
}

/// Map the shared core's typed failures onto the WIT's vocabulary.
fn map_driver_error(error: DriverError<platform::PlatformError>) -> UsbError {
    match error {
        DriverError::Io(platform::PlatformError::Denied) => UsbError::Denied,
        DriverError::Io(other) => UsbError::Io(format!("usb.ohci: platform: {other:?}")),
        DriverError::NotOhci { revision } => UsbError::Io(format!(
            "usb.ohci: HcRevision {revision:#x} is not an OHCI 1.x controller"
        )),
        DriverError::Timeout(_) => UsbError::Timeout,
        DriverError::Stall => UsbError::Stall,
        DriverError::Transfer(code) => UsbError::Io(format!("usb.ohci: transfer error: {code:?}")),
        DriverError::NoSuchPort | DriverError::NotConnected => UsbError::NotFound,
        DriverError::Hub(limitation) => {
            UsbError::Io(format!("usb.ohci: hub traversal: {limitation}"))
        }
        DriverError::Enumeration(error) => {
            UsbError::Io(format!("usb.ohci: enumeration: {error:?}"))
        }
        DriverError::DoneQueueCorrupt => UsbError::Io(String::from("usb.ohci: corrupt done queue")),
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

/// Compose-time configuration (`ohci-config.configure`): the platform region to
/// claim. Unset = the first granted `-ohci` region (the option-C default rule).
static REGION: ProviderState<String> = ProviderState::new();

/// The configured region name, if `configure` bound one.
fn configured_region() -> Option<String> {
    if REGION.is_set() {
        Some(REGION.with(|name| name.clone()))
    } else {
        None
    }
}

/// Granted region names, for the configured-name-missing diagnostic.
fn region_names(regions: &[platform::RegionInfo]) -> String {
    let mut names = String::new();
    for region in regions {
        if !names.is_empty() {
            names.push_str(", ");
        }
        names.push_str(&region.name);
    }
    if names.is_empty() {
        names.push_str("(none)");
    }
    names
}

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
/// re-arm state).
struct ShellEndpoint;

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
        Ok(usb::Endpoint::new(ShellEndpoint))
    }

    async fn read(_e: usb::EndpointBorrow<'_>) -> Result<Vec<u8>, UsbError> {
        let mut guard = acquire().await?;
        let mut report = [0u8; arena::INTERRUPT_BUFFER_LEN as usize];
        match guard.poll_interrupt(&mut report).await {
            Ok(Some(length)) => Ok(report[..length].to_vec()),
            // Nothing arrived within the short poll: the empty result, the consumer
            // owns the wait policy (the recv-frame contract).
            Ok(None) => Ok(Vec::new()),
            Err(error) => Err(map_driver_error(error)),
        }
    }
}

impl ohci_config::Guest for Shell {
    fn configure(region: String) -> Result<types::UsbImpl, String> {
        REGION.set(region);
        Ok(types::UsbImpl::new(UsbRoot))
    }
}

export!(Shell);
