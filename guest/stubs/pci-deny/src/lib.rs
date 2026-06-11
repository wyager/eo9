//! `pci.deny` — the PCI capability, present but refusing.
//!
//! Targets the `eo9:pci/deny` stub world: exports `eo9:pci/pci` where the operations on
//! the root handle (`enumerate`, `open`) fail with the API's own `denied` error (see
//! SPEC.md, "PCI API": refusal is meaningful for device access, so PCI gets a deny stub
//! in its own vocabulary). Composed as `pci.deny $ driver`, a driver observes a PCI
//! hierarchy it is not allowed to touch — instead of the absence `pci.none` models or
//! the unsatisfied import the loader would otherwise refuse at spawn.
//!
//! Because no device can ever be opened, every operation on opened devices, BARs,
//! interrupt vectors, and DMA buffers is unreachable: their resource types are
//! uninhabited, which the empty matches below make explicit.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "deny",
    path: "../../../wit/pci",
    generate_all,
});

use exports::eo9::pci::deny_config;
use exports::eo9::pci::pci::{self, AccessWidth, BarInfo, DeviceInfo, InterruptKind, PciError};
use exports::eo9::pci::types::{self, DeviceAddress};

/// The `pci.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no hierarchy behind it.
struct DenyPci;

/// Uninhabited representations for the resources `pci.deny` can never produce: `open`
/// always refuses, so devices (and everything reached through them) cannot exist.
enum NoDevice {}
enum NoBar {}
enum NoInterrupt {}
enum NoDmaBuffer {}

/// Statically-checked unreachability: holding a borrow of an uninhabited resource is a
/// contradiction, which an empty match discharges.
trait Unreachable {
    fn unreachable<T>(&self) -> T;
}

impl Unreachable for NoDevice {
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}
impl Unreachable for NoBar {
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}
impl Unreachable for NoInterrupt {
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}
impl Unreachable for NoDmaBuffer {
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}

impl pci::GuestDevice for NoDevice {}
impl pci::GuestBar for NoBar {}
impl pci::GuestInterrupt for NoInterrupt {}
impl pci::GuestDmaBuffer for NoDmaBuffer {}

impl types::Guest for Stub {
    type PciImpl = DenyPci;
}

impl types::GuestPciImpl for DenyPci {}

impl deny_config::Guest for Stub {
    fn configure() -> Result<types::PciImpl, String> {
        Ok(types::PciImpl::new(DenyPci))
    }
}

impl pci::Guest for Stub {
    type Device = NoDevice;
    type Bar = NoBar;
    type Interrupt = NoInterrupt;
    type DmaBuffer = NoDmaBuffer;

    fn default() -> types::PciImpl {
        types::PciImpl::new(DenyPci)
    }

    async fn enumerate(_p: types::PciImplBorrow<'_>) -> Result<Vec<DeviceInfo>, PciError> {
        Err(PciError::Denied)
    }

    async fn open(
        _p: types::PciImplBorrow<'_>,
        _address: DeviceAddress,
    ) -> Result<pci::Device, PciError> {
        Err(PciError::Denied)
    }

    async fn config_read(
        dev: pci::DeviceBorrow<'_>,
        _offset: u32,
        _width: AccessWidth,
    ) -> Result<u64, PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn config_write(
        dev: pci::DeviceBorrow<'_>,
        _offset: u32,
        _width: AccessWidth,
        _value: u64,
    ) -> Result<(), PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn bars(dev: pci::DeviceBorrow<'_>) -> Result<Vec<BarInfo>, PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn open_bar(dev: pci::DeviceBorrow<'_>, _index: u8) -> Result<pci::Bar, PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn bar_read(
        b: pci::BarBorrow<'_>,
        _offset: u64,
        _width: AccessWidth,
    ) -> Result<u64, PciError> {
        b.get::<NoBar>().unreachable()
    }

    async fn bar_write(
        b: pci::BarBorrow<'_>,
        _offset: u64,
        _width: AccessWidth,
        _value: u64,
    ) -> Result<(), PciError> {
        b.get::<NoBar>().unreachable()
    }

    async fn set_bus_master(dev: pci::DeviceBorrow<'_>, _enable: bool) -> Result<(), PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn reset(dev: pci::DeviceBorrow<'_>) -> Result<(), PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn enable_interrupts(
        dev: pci::DeviceBorrow<'_>,
        _kind: InterruptKind,
        _count: u32,
    ) -> Result<Vec<pci::Interrupt>, PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    async fn wait(i: pci::InterruptBorrow<'_>, _max_ns: u64) -> Result<u64, PciError> {
        i.get::<NoInterrupt>().unreachable()
    }

    async fn alloc_dma(dev: pci::DeviceBorrow<'_>, _len: u64) -> Result<pci::DmaBuffer, PciError> {
        dev.get::<NoDevice>().unreachable()
    }

    fn dma_address(b: pci::DmaBufferBorrow<'_>) -> u64 {
        b.get::<NoDmaBuffer>().unreachable()
    }

    fn dma_len(b: pci::DmaBufferBorrow<'_>) -> u64 {
        b.get::<NoDmaBuffer>().unreachable()
    }

    fn dma_read(b: pci::DmaBufferBorrow<'_>, _offset: u64, _len: u64) -> Vec<u8> {
        b.get::<NoDmaBuffer>().unreachable()
    }

    fn dma_write(b: pci::DmaBufferBorrow<'_>, _offset: u64, _bytes: Vec<u8>) {
        b.get::<NoDmaBuffer>().unreachable()
    }
}

export!(Stub);
