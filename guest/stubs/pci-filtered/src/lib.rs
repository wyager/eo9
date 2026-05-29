//! `pci.filtered` — an allow-listed view of an underlying PCI capability.
//!
//! Targets the `eo9:pci/filtered` stub world: imports `eo9:pci/pci` and re-exports it with
//! enumeration and `open` restricted to a configured allow-list of device addresses — the
//! "exactly this one device" grant from SPEC.md ("PCI API"). Concretely:
//!
//! * `configure(allow)` binds the list of visible [`device addresses`](DeviceAddress);
//!   unconfigured, the documented default is the **empty list** — nothing is visible and
//!   every `open` answers `denied` (the option-C never-trap rule, plan/09 Decision 14).
//! * `enumerate` forwards to the underlying capability and keeps only allow-listed
//!   functions; `open` refuses anything outside the list with `denied`.
//! * Everything reached *through* an allowed device (configuration space, BARs, bus-master
//!   control, interrupts, DMA buffers) forwards to the underlying provider on resources
//!   this provider owns and wraps — a consumer can never reach an underlying handle except
//!   through the filtered view, and never for a device the list does not name.
//!
//! Composed as `pci.filtered --allow … $ driver`, the driver sees a bus containing exactly
//! the allowed functions; the kernel's root provider (and its own boot-time grant) sits
//! underneath, unchanged.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "filtered",
    path: "../../../wit/pci",
    generate_all,
});

use eo9::pci::pci as underlying;
use eo9::pci::types::{DeviceAddress, PciImpl};
use exports::eo9::pci::filtered_config;
use exports::eo9::pci::pci::{
    self, AccessWidth, BarInfo, BarKind, DeviceInfo, HeaderType, InterruptKind, PciError,
};

/// The configured allow-list. Unconfigured means "allow nothing" (see the module docs).
static ALLOW: ProviderState<Vec<(u16, u8, u8, u8)>> = ProviderState::new();

/// Whether `address` is on the configured allow-list (empty / unconfigured: nothing is).
fn allowed(segment: u16, bus: u8, device: u8, function: u8) -> bool {
    if !ALLOW.is_set() {
        return false;
    }
    ALLOW.with(|list| list.contains(&(segment, bus, device, function)))
}

/// Map the underlying provider's error onto this provider's (structurally identical)
/// exported error type.
fn map_error(error: underlying::PciError) -> PciError {
    match error {
        underlying::PciError::Denied => PciError::Denied,
        underlying::PciError::NotFound => PciError::NotFound,
        underlying::PciError::Busy => PciError::Busy,
        underlying::PciError::OutOfRange => PciError::OutOfRange,
        underlying::PciError::Unsupported => PciError::Unsupported,
        underlying::PciError::Exhausted => PciError::Exhausted,
        underlying::PciError::Io(message) => PciError::Io(message),
    }
}

/// Map an underlying device-info record onto the exported one. (`device-address` itself is
/// a `use` of the shared types interface, so it needs no mapping.)
fn map_info(info: underlying::DeviceInfo) -> DeviceInfo {
    DeviceInfo {
        address: info.address,
        vendor_id: info.vendor_id,
        device_id: info.device_id,
        class_code: info.class_code,
        subclass: info.subclass,
        prog_if: info.prog_if,
        revision: info.revision,
        header: match info.header {
            underlying::HeaderType::Endpoint => HeaderType::Endpoint,
            underlying::HeaderType::PciBridge => HeaderType::PciBridge,
            underlying::HeaderType::CardbusBridge => HeaderType::CardbusBridge,
        },
    }
}

/// Map an underlying BAR description onto the exported one.
fn map_bar(bar: underlying::BarInfo) -> BarInfo {
    BarInfo {
        index: bar.index,
        kind: match bar.kind {
            underlying::BarKind::Memory => BarKind::Memory,
            underlying::BarKind::Io => BarKind::Io,
        },
        size: bar.size,
        prefetchable: bar.prefetchable,
        wide: bar.wide,
    }
}

/// Map the exported access width onto the underlying enum.
fn width_to_underlying(width: AccessWidth) -> underlying::AccessWidth {
    match width {
        AccessWidth::Byte => underlying::AccessWidth::Byte,
        AccessWidth::Word => underlying::AccessWidth::Word,
        AccessWidth::Dword => underlying::AccessWidth::Dword,
        AccessWidth::Qword => underlying::AccessWidth::Qword,
    }
}

/// Map the exported interrupt kind onto the underlying enum.
fn kind_to_underlying(kind: InterruptKind) -> underlying::InterruptKind {
    match kind {
        InterruptKind::Intx => underlying::InterruptKind::Intx,
        InterruptKind::Msi => underlying::InterruptKind::Msi,
        InterruptKind::MsiX => underlying::InterruptKind::MsiX,
    }
}

/// The `pci.filtered` provider.
struct Stub;

/// An opened, allow-listed device of the filtered view: wraps the underlying device.
struct FilteredDevice {
    inner: underlying::Device,
}

/// An opened BAR of an allow-listed device: wraps the underlying BAR.
struct FilteredBar {
    inner: underlying::Bar,
}

/// An interrupt vector of an allow-listed device: wraps the underlying vector.
struct FilteredInterrupt {
    inner: underlying::Interrupt,
}

/// A DMA buffer mapped for an allow-listed device: wraps the underlying buffer.
struct FilteredDmaBuffer {
    inner: underlying::DmaBuffer,
}

impl pci::GuestDevice for FilteredDevice {}
impl pci::GuestBar for FilteredBar {}
impl pci::GuestInterrupt for FilteredInterrupt {}
impl pci::GuestDmaBuffer for FilteredDmaBuffer {}

impl filtered_config::Guest for Stub {
    fn configure(allow: Vec<DeviceAddress>) -> Result<PciImpl, String> {
        ALLOW.set(
            allow
                .iter()
                .map(|a| (a.segment, a.bus, a.device, a.function))
                .collect(),
        );
        // The root handle is the underlying provider's: the filtering lives in the exported
        // operations (which are this component's), not in the handle.
        Ok(underlying::default())
    }
}

impl pci::Guest for Stub {
    type Device = FilteredDevice;
    type Bar = FilteredBar;
    type Interrupt = FilteredInterrupt;
    type DmaBuffer = FilteredDmaBuffer;

    fn default() -> PciImpl {
        underlying::default()
    }

    async fn enumerate(p: &PciImpl) -> Result<Vec<DeviceInfo>, PciError> {
        let devices = underlying::enumerate(p).await.map_err(map_error)?;
        Ok(devices
            .into_iter()
            .filter(|info| {
                allowed(
                    info.address.segment,
                    info.address.bus,
                    info.address.device,
                    info.address.function,
                )
            })
            .map(map_info)
            .collect())
    }

    async fn open(p: &PciImpl, address: DeviceAddress) -> Result<pci::Device, PciError> {
        if !allowed(
            address.segment,
            address.bus,
            address.device,
            address.function,
        ) {
            return Err(PciError::Denied);
        }
        let inner = underlying::open(p, address).await.map_err(map_error)?;
        Ok(pci::Device::new(FilteredDevice { inner }))
    }

    async fn config_read(
        dev: pci::DeviceBorrow<'_>,
        offset: u32,
        width: AccessWidth,
    ) -> Result<u64, PciError> {
        underlying::config_read(
            &dev.get::<FilteredDevice>().inner,
            offset,
            width_to_underlying(width),
        )
        .await
        .map_err(map_error)
    }

    async fn config_write(
        dev: pci::DeviceBorrow<'_>,
        offset: u32,
        width: AccessWidth,
        value: u64,
    ) -> Result<(), PciError> {
        underlying::config_write(
            &dev.get::<FilteredDevice>().inner,
            offset,
            width_to_underlying(width),
            value,
        )
        .await
        .map_err(map_error)
    }

    async fn bars(dev: pci::DeviceBorrow<'_>) -> Result<Vec<BarInfo>, PciError> {
        underlying::bars(&dev.get::<FilteredDevice>().inner)
            .await
            .map(|bars| bars.into_iter().map(map_bar).collect())
            .map_err(map_error)
    }

    async fn open_bar(dev: pci::DeviceBorrow<'_>, index: u8) -> Result<pci::Bar, PciError> {
        let inner = underlying::open_bar(&dev.get::<FilteredDevice>().inner, index)
            .await
            .map_err(map_error)?;
        Ok(pci::Bar::new(FilteredBar { inner }))
    }

    async fn bar_read(
        b: pci::BarBorrow<'_>,
        offset: u64,
        width: AccessWidth,
    ) -> Result<u64, PciError> {
        underlying::bar_read(
            &b.get::<FilteredBar>().inner,
            offset,
            width_to_underlying(width),
        )
        .await
        .map_err(map_error)
    }

    async fn bar_write(
        b: pci::BarBorrow<'_>,
        offset: u64,
        width: AccessWidth,
        value: u64,
    ) -> Result<(), PciError> {
        underlying::bar_write(
            &b.get::<FilteredBar>().inner,
            offset,
            width_to_underlying(width),
            value,
        )
        .await
        .map_err(map_error)
    }

    async fn set_bus_master(dev: pci::DeviceBorrow<'_>, enable: bool) -> Result<(), PciError> {
        underlying::set_bus_master(&dev.get::<FilteredDevice>().inner, enable)
            .await
            .map_err(map_error)
    }

    async fn reset(dev: pci::DeviceBorrow<'_>) -> Result<(), PciError> {
        underlying::reset(&dev.get::<FilteredDevice>().inner)
            .await
            .map_err(map_error)
    }

    async fn enable_interrupts(
        dev: pci::DeviceBorrow<'_>,
        kind: InterruptKind,
        count: u32,
    ) -> Result<Vec<pci::Interrupt>, PciError> {
        let vectors = underlying::enable_interrupts(
            &dev.get::<FilteredDevice>().inner,
            kind_to_underlying(kind),
            count,
        )
        .await
        .map_err(map_error)?;
        Ok(vectors
            .into_iter()
            .map(|inner| pci::Interrupt::new(FilteredInterrupt { inner }))
            .collect())
    }

    async fn wait(i: pci::InterruptBorrow<'_>) -> Result<u64, PciError> {
        underlying::wait(&i.get::<FilteredInterrupt>().inner)
            .await
            .map_err(map_error)
    }

    async fn alloc_dma(dev: pci::DeviceBorrow<'_>, len: u64) -> Result<pci::DmaBuffer, PciError> {
        let inner = underlying::alloc_dma(&dev.get::<FilteredDevice>().inner, len)
            .await
            .map_err(map_error)?;
        Ok(pci::DmaBuffer::new(FilteredDmaBuffer { inner }))
    }

    fn dma_address(b: pci::DmaBufferBorrow<'_>) -> u64 {
        underlying::dma_address(&b.get::<FilteredDmaBuffer>().inner)
    }

    fn dma_len(b: pci::DmaBufferBorrow<'_>) -> u64 {
        underlying::dma_len(&b.get::<FilteredDmaBuffer>().inner)
    }

    fn dma_read(b: pci::DmaBufferBorrow<'_>, offset: u64, len: u64) -> Vec<u8> {
        underlying::dma_read(&b.get::<FilteredDmaBuffer>().inner, offset, len)
    }

    fn dma_write(b: pci::DmaBufferBorrow<'_>, offset: u64, bytes: Vec<u8>) {
        underlying::dma_write(&b.get::<FilteredDmaBuffer>().inner, offset, &bytes)
    }
}

export!(Stub);
