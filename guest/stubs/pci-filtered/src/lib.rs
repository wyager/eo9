//! `pci.filtered` — a policy-attenuated view of an underlying PCI capability.
//!
//! Targets the `eo9:pci/filtered` stub world: imports `eo9:pci/pci` plus an
//! `eo9:pci/admit-policy` decision function, and re-exports `pci` with enumeration and
//! `open` restricted to the devices the policy admits — the "exactly this device" grant
//! from SPEC.md ("PCI API"), with the *which devices* question answered by a composed
//! policy component ("policies are programs" — SPEC, Eo9 API design). Concretely:
//!
//! * `enumerate` forwards to the underlying capability and keeps only the functions the
//!   admit policy approves; `open` refuses anything the policy does not admit with
//!   `denied`.
//! * The policy is ordinary middleware composed below this provider —
//!   `pci.admit-vendor --allow … $ pci.filtered $ driver` — so it is fused at compile
//!   time, appears in the wiring tree, and cannot be bypassed: a consumer can never
//!   reach an underlying handle except through the filtered view, and never for a
//!   device the policy refused.
//! * Everything reached *through* an admitted device (configuration space, BARs,
//!   bus-master control, interrupts, DMA buffers) forwards to the underlying provider on
//!   resources this provider owns and wraps.
//!
//! The standard policies are `pci.admit-address` (fixed bus addresses) and
//! `pci.admit-vendor` (vendor:device identity, stable across boot configs); an
//! unconfigured policy admits nothing, so plain composition stays deny-by-default.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "filtered",
    path: "../../../wit/pci",
    generate_all,
});

use eo9::pci::admit_policy;
use eo9::pci::pci as underlying;
use eo9::pci::types::{DeviceAddress, PciImpl};
use exports::eo9::pci::pci::{
    self, AccessWidth, BarInfo, BarKind, DeviceInfo, HeaderType, InterruptKind, PciError,
};

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

/// Whether the composed admit policy approves this (underlying-typed) device record.
/// The policy interface's `use pci.{device-info}` makes its parameter the same type the
/// underlying enumeration yields, so no mapping is needed.
fn admitted(info: &underlying::DeviceInfo) -> bool {
    admit_policy::admit(*info)
}

/// Find the underlying device record for `address`, if any.
async fn lookup(p: &PciImpl, address: &DeviceAddress) -> Result<underlying::DeviceInfo, PciError> {
    let devices = underlying::enumerate(p).await.map_err(map_error)?;
    devices
        .into_iter()
        .find(|info| {
            info.address.segment == address.segment
                && info.address.bus == address.bus
                && info.address.device == address.device
                && info.address.function == address.function
        })
        .ok_or(PciError::NotFound)
}

/// The `pci.filtered` provider.
struct Stub;

/// An opened, policy-admitted device of the filtered view: wraps the underlying device.
struct FilteredDevice {
    inner: underlying::Device,
}

/// An opened BAR of an admitted device: wraps the underlying BAR.
struct FilteredBar {
    inner: underlying::Bar,
}

/// An interrupt vector of an admitted device: wraps the underlying vector.
struct FilteredInterrupt {
    inner: underlying::Interrupt,
}

/// A DMA buffer mapped for an admitted device: wraps the underlying buffer.
struct FilteredDmaBuffer {
    inner: underlying::DmaBuffer,
}

impl pci::GuestDevice for FilteredDevice {}
impl pci::GuestBar for FilteredBar {}
impl pci::GuestInterrupt for FilteredInterrupt {}
impl pci::GuestDmaBuffer for FilteredDmaBuffer {}

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
        Ok(devices.into_iter().filter(admitted).map(map_info).collect())
    }

    async fn open(p: &PciImpl, address: DeviceAddress) -> Result<pci::Device, PciError> {
        // The policy decides on the device's *identity*, so look the address up first:
        // an absent device is `not-found`, a present-but-refused one is `denied`.
        let info = lookup(p, &address).await?;
        if !admitted(&info) {
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
