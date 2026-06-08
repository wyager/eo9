//! `platform.deny` — the platform-device capability, present but refusing.
//!
//! Targets the `eo9:platform/deny` stub world: exports `eo9:platform/platform` where
//! the operations on the root handle (`enumerate`, `claim`) fail with the API's own
//! `denied` error. Composed as `platform.deny $ driver`, a driver observes a region
//! table it is not allowed to touch — instead of the absence `platform.none` models or
//! the unsatisfied import the loader would otherwise refuse at spawn (the same posture
//! as `pci.deny`; SPEC.md, "The capability algebra").
//!
//! Because no region can ever be claimed, every operation on claimed regions,
//! interrupts, and DMA buffers is unreachable: their resource types are uninhabited,
//! which the empty matches below make explicit.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "deny",
    path: "../../../wit/platform",
});

use exports::eo9::platform::deny_config;
use exports::eo9::platform::platform::{self, AccessWidth, PlatformError, RegionInfo};
use exports::eo9::platform::types;

/// The `platform.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no region table behind it.
struct DenyPlatform;

/// Uninhabited representations for the resources `platform.deny` can never produce:
/// `claim` always refuses, so regions (and everything reached through them) cannot
/// exist.
enum NoRegion {}
enum NoInterrupt {}
enum NoDmaBuffer {}

/// Statically-checked unreachability: holding a borrow of an uninhabited resource is a
/// contradiction, which an empty match discharges.
trait Unreachable {
    fn unreachable<T>(&self) -> T;
}

impl Unreachable for NoRegion {
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

impl platform::GuestRegion for NoRegion {}
impl platform::GuestInterrupt for NoInterrupt {}
impl platform::GuestDmaBuffer for NoDmaBuffer {}

impl types::Guest for Stub {
    type PlatformImpl = DenyPlatform;
}

impl types::GuestPlatformImpl for DenyPlatform {}

impl deny_config::Guest for Stub {
    fn configure() -> Result<types::PlatformImpl, String> {
        Ok(types::PlatformImpl::new(DenyPlatform))
    }
}

impl platform::Guest for Stub {
    type Region = NoRegion;
    type Interrupt = NoInterrupt;
    type DmaBuffer = NoDmaBuffer;

    fn default() -> types::PlatformImpl {
        types::PlatformImpl::new(DenyPlatform)
    }

    async fn enumerate(
        _p: types::PlatformImplBorrow<'_>,
    ) -> Result<Vec<RegionInfo>, PlatformError> {
        Err(PlatformError::Denied)
    }

    async fn claim(
        _p: types::PlatformImplBorrow<'_>,
        _name: String,
    ) -> Result<platform::Region, PlatformError> {
        Err(PlatformError::Denied)
    }

    async fn read(
        r: platform::RegionBorrow<'_>,
        _offset: u64,
        _width: AccessWidth,
    ) -> Result<u64, PlatformError> {
        r.get::<NoRegion>().unreachable()
    }

    async fn write(
        r: platform::RegionBorrow<'_>,
        _offset: u64,
        _width: AccessWidth,
        _value: u64,
    ) -> Result<(), PlatformError> {
        r.get::<NoRegion>().unreachable()
    }

    async fn enable_interrupts(
        r: platform::RegionBorrow<'_>,
    ) -> Result<platform::Interrupt, PlatformError> {
        r.get::<NoRegion>().unreachable()
    }

    async fn wait(i: platform::InterruptBorrow<'_>) -> Result<u64, PlatformError> {
        i.get::<NoInterrupt>().unreachable()
    }

    async fn alloc_dma(
        r: platform::RegionBorrow<'_>,
        _len: u64,
    ) -> Result<platform::DmaBuffer, PlatformError> {
        r.get::<NoRegion>().unreachable()
    }

    fn dma_address(b: platform::DmaBufferBorrow<'_>) -> u64 {
        b.get::<NoDmaBuffer>().unreachable()
    }

    fn dma_len(b: platform::DmaBufferBorrow<'_>) -> u64 {
        b.get::<NoDmaBuffer>().unreachable()
    }

    fn dma_read(b: platform::DmaBufferBorrow<'_>, _offset: u64, _len: u64) -> Vec<u8> {
        b.get::<NoDmaBuffer>().unreachable()
    }

    fn dma_write(b: platform::DmaBufferBorrow<'_>, _offset: u64, _bytes: Vec<u8>) {
        b.get::<NoDmaBuffer>().unreachable()
    }
}

export!(Stub);
