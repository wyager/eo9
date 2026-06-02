//! The block-device abstraction eofs runs on, plus the in-memory implementations used by
//! tests and embedders.
//!
//! The trait is byte-addressed on purpose: it is the same shape as the `eo9:disk` API (an
//! offset-addressed flat span of bytes), which is what the milestone-2 provider component
//! will be bridging to. eofs layers its own block/extent structure on top and never relies
//! on write atomicity — a torn write of anything, including an uberblock slot, is detected
//! by checksums on the next mount (see `FORMAT.md`).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

/// An error from the underlying device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// A read or write touched bytes outside the device.
    OutOfRange,
    /// The device failed (for the simulated devices: the power was cut).
    Io,
    /// The device failed and described why. The string is the device's own words — for
    /// example a wasm driver's typed error text ("disk.virtio: the device reported request
    /// status 1") — preserved verbatim so a real hardware failure, a hung device, and a
    /// composition bug stay distinguishable all the way to the user (study 09 finding 2).
    IoNamed(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::OutOfRange => write!(f, "access outside the device"),
            DeviceError::Io => write!(f, "device i/o failure"),
            DeviceError::IoNamed(what) => write!(f, "{what}"),
        }
    }
}

impl core::error::Error for DeviceError {}

/// A flat, byte-addressed block device.
///
/// Requirements eofs places on implementations:
///
/// * Reads observe all previously completed writes (no reordering visible to the caller).
/// * `flush` makes every completed write durable before it returns. eofs only depends on
///   ordering across `flush` calls, never on the atomicity of an individual write.
/// * A write that returns an error may have been applied partially (a torn write).
pub trait BlockDevice {
    /// Device capacity in bytes.
    fn size(&self) -> u64;

    /// Read `buf.len()` bytes starting at byte `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError>;

    /// Write `data` starting at byte `offset`.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError>;

    /// Make every completed write durable.
    fn flush(&mut self) -> Result<(), DeviceError>;
}

/// A flat, byte-addressed block device whose operations may genuinely wait.
///
/// This is the boundary the **async engine core** ([`AsyncEofs`](crate::AsyncEofs)) runs
/// on: the guest provider implements it by awaiting the imported `eo9:disk` operations,
/// so a disk that parks (an interrupt-driven driver, a deferred guest chain) suspends the
/// filesystem operation instead of failing it. The contract is identical to
/// [`BlockDevice`] in every other respect (ordering, flush durability, torn writes).
///
/// Synchronous embedders (the kernel's storedisk cache, `mkfs`, the test suite) never
/// implement this trait; they keep [`BlockDevice`] and go through the [`Eofs`](crate::Eofs)
/// facade, which adapts via [`SyncDevice`] — whose futures are always immediately ready,
/// so the facade's single-poll driver never observes `Pending`.
#[allow(async_fn_in_trait)] // single-threaded no_std: no Send bound wanted
pub trait AsyncBlockDevice {
    /// Device capacity in bytes.
    fn size(&self) -> u64;

    /// Read `buf.len()` bytes starting at byte `offset`.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError>;

    /// Write `data` starting at byte `offset`.
    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError>;

    /// Make every completed write durable.
    async fn flush(&mut self) -> Result<(), DeviceError>;
}

/// A synchronous [`BlockDevice`] seen through the [`AsyncBlockDevice`] boundary.
///
/// Every future this adapter produces is ready on its first poll (the inner call has
/// already completed by the time the future exists), which is what makes the sync
/// [`Eofs`](crate::Eofs) facade sound: driving the async core over a `SyncDevice` can
/// never observe `Pending`.
pub struct SyncDevice<D: BlockDevice>(pub D);

impl<D: BlockDevice> SyncDevice<D> {
    /// Unwrap the inner device.
    pub fn into_inner(self) -> D {
        self.0
    }
}

impl<D: BlockDevice> AsyncBlockDevice for SyncDevice<D> {
    fn size(&self) -> u64 {
        self.0.size()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.0.read_at(offset, buf)
    }

    async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        self.0.write_at(offset, data)
    }

    async fn flush(&mut self) -> Result<(), DeviceError> {
        self.0.flush()
    }
}

/// A shared reference to a synchronous device, for the read-only [`probe`](crate::probe)
/// facade (probing only ever reads). Writing through it is a contract violation and
/// reports [`DeviceError::Io`].
pub(crate) struct SyncReadRef<'a, D: BlockDevice>(pub &'a D);

impl<D: BlockDevice> AsyncBlockDevice for SyncReadRef<'_, D> {
    fn size(&self) -> u64 {
        self.0.size()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.0.read_at(offset, buf)
    }

    async fn write_at(&mut self, _offset: u64, _data: &[u8]) -> Result<(), DeviceError> {
        Err(DeviceError::Io)
    }

    async fn flush(&mut self) -> Result<(), DeviceError> {
        Err(DeviceError::Io)
    }
}

/// Drive a future from the async engine core to completion *now*.
///
/// Only the sync facade uses this, and only over [`SyncDevice`]/[`SyncReadRef`] — whose
/// futures are immediately ready — so the engine core (whose only awaits are device
/// calls) always completes on the first poll. `Pending` is unreachable by construction;
/// reaching it would mean a sync device produced a future that parked, which no
/// implementation in this crate can.
pub(crate) fn poll_now<F: core::future::Future>(future: F) -> F::Output {
    let mut future = core::pin::pin!(future);
    let mut context = core::task::Context::from_waker(core::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        core::task::Poll::Ready(value) => value,
        core::task::Poll::Pending => {
            unreachable!("a synchronous device future never pends")
        }
    }
}

/// Checks that `offset..offset + len` lies inside a device of `size` bytes.
fn check_range(size: u64, offset: u64, len: usize) -> Result<(), DeviceError> {
    let len = len as u64;
    let end = offset.checked_add(len).ok_or(DeviceError::OutOfRange)?;
    if end > size {
        return Err(DeviceError::OutOfRange);
    }
    Ok(())
}

/// A RAM-backed device: a zero-initialised `Vec<u8>`.
///
/// This is the device the test suite runs on; it is also usable by any embedder that wants
/// an in-memory image (`disk.mem` in usermode, scratch images in tooling).
pub struct MemDevice {
    data: Vec<u8>,
}

impl MemDevice {
    /// A zero-filled device of `size` bytes.
    pub fn new(size: u64) -> MemDevice {
        MemDevice {
            data: vec![0; size as usize],
        }
    }

    /// Wrap an existing image.
    pub fn from_vec(data: Vec<u8>) -> MemDevice {
        MemDevice { data }
    }

    /// Take the image back out.
    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }

    /// The raw image bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    /// The raw image bytes, mutably (tests use this to inject corruption).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl BlockDevice for MemDevice {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        check_range(self.size(), offset, buf.len())?;
        let start = offset as usize;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        check_range(self.size(), offset, data.len())?;
        let start = offset as usize;
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
}

/// A power-cut simulator: wraps another device and "loses power" after a configured number
/// of writes, optionally tearing the final write partway through.
///
/// Once the cut has happened every further write and flush fails with [`DeviceError::Io`];
/// whatever reached the inner device before the cut is exactly what a remount sees. The
/// crash-consistency tests drive this across every write boundary of a scenario.
pub struct CutDevice<D> {
    inner: D,
    /// Writes still allowed to complete fully. `None` means unlimited.
    remaining: Option<u64>,
    /// How many leading bytes of the cut write are applied (a torn write).
    tear: usize,
    /// The power is out.
    dead: bool,
    /// Completed (not torn) writes so far.
    writes: u64,
}

impl<D: BlockDevice> CutDevice<D> {
    /// A wrapper that never cuts: used to count how many writes a scenario performs.
    pub fn unlimited(inner: D) -> CutDevice<D> {
        CutDevice {
            inner,
            remaining: None,
            tear: 0,
            dead: false,
            writes: 0,
        }
    }

    /// Cut the power during write number `allowed` (0-based): the first `allowed` writes
    /// complete, the next applies only its first `tear` bytes and fails, and everything
    /// after that fails outright.
    pub fn cut_after(inner: D, allowed: u64, tear: usize) -> CutDevice<D> {
        CutDevice {
            inner,
            remaining: Some(allowed),
            tear,
            dead: false,
            writes: 0,
        }
    }

    /// Number of fully completed writes so far.
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Whether the simulated power cut has happened.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Unwrap the inner device (the image as it stood at the cut).
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for CutDevice<D> {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), DeviceError> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), DeviceError> {
        if self.dead {
            return Err(DeviceError::Io);
        }
        if let Some(remaining) = self.remaining {
            if remaining == 0 {
                // The cut happens during this write: apply a prefix, then die.
                self.dead = true;
                let torn = self.tear.min(data.len());
                if torn > 0 {
                    self.inner.write_at(offset, &data[..torn])?;
                }
                return Err(DeviceError::Io);
            }
            self.remaining = Some(remaining - 1);
        }
        self.inner.write_at(offset, data)?;
        self.writes += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        if self.dead {
            return Err(DeviceError::Io);
        }
        self.inner.flush()
    }
}
