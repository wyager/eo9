//! `disk.mem` — a RAM-backed block device.
//!
//! Targets the `eo9:disk/mem` stub world: exports `eo9:disk/disk` over a zero-filled
//! in-memory byte array whose size is bound by `configure`. Part of the deterministic
//! environment of integration milestone I2: reads and writes are a pure function of the
//! program's own operations.
//!
//! The documented default state is a zero-filled device of 16 MiB: an unconfigured
//! `disk.mem` self-initializes to it on first use, so plain `disk.mem $ fs.eofs $ program`
//! works and never traps (the default-configuration rule, plan/09 Decision 14;
//! plan/14-eofs.md milestone-2 Decisions). `configure(size)` still binds an explicit size.
//!
//! Semantics: the device has a fixed size; an access whose range `offset .. offset+len`
//! does not lie entirely within the device fails with `out-of-range` (no partial I/O),
//! and a zero-length access at any offset up to the size succeeds.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "mem",
    path: "../../../wit/disk",
    // Pull in bindings for eo9:io/buffers, which the exported disk interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::disk::disk::{self, Buffer, ReadError, ReadResult, WriteError, WriteResult};
use exports::eo9::disk::mem_config;
use exports::eo9::disk::types;

/// The device contents, bound by `configure`.
static STATE: ProviderState<Vec<u8>> = ProviderState::new();

/// Size of the documented default device (an unconfigured `disk.mem`): 16 MiB, zero-filled.
const DEFAULT_SIZE: usize = 16 * 1024 * 1024;

/// Run `f` over the device contents. An unconfigured `disk.mem` defaults to the documented
/// zero-filled 16 MiB device (the option-C default-configuration rule, plan/09 Decision 14),
/// so it never traps when used without `configure`.
fn with_device<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(vec![0; DEFAULT_SIZE]);
    }
    STATE.with(f)
}

/// Resolve `offset .. offset+len` against a device of size `device_size`, or report that
/// the range does not fit. All quantities are in bytes.
fn range(device_size: usize, offset: u64, len: u64) -> Option<(usize, usize)> {
    let offset = usize::try_from(offset).ok()?;
    let len = usize::try_from(len).ok()?;
    let end = offset.checked_add(len)?;
    if end > device_size {
        return None;
    }
    Some((offset, end))
}

/// The `disk.mem` provider.
struct Stub;

/// The root-handle resource: a token referring to the configured device contents.
struct MemDisk;

impl types::Guest for Stub {
    type DiskImpl = MemDisk;
}

impl types::GuestDiskImpl for MemDisk {}

impl mem_config::Guest for Stub {
    fn configure(size: u64) -> Result<types::DiskImpl, String> {
        let Ok(size) = usize::try_from(size) else {
            return Err(String::from("size does not fit in the guest address space"));
        };
        STATE.set(vec![0; size]);
        Ok(types::DiskImpl::new(MemDisk))
    }
}

impl disk::Guest for Stub {
    fn default() -> types::DiskImpl {
        types::DiskImpl::new(MemDisk)
    }

    fn size(_dev: disk::DiskImplBorrow<'_>) -> u64 {
        with_device(|device| device.len() as u64)
    }

    async fn flush(_dev: disk::DiskImplBorrow<'_>) -> Result<(), WriteError> {
        // Purely in-memory: every completed write is already as durable as it will ever be.
        Ok(())
    }

    async fn read(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        dst: Buffer,
    ) -> (Buffer, Result<ReadResult, ReadError>) {
        let len = dst.len();
        let result = with_device(|device| {
            let Some((start, end)) = range(device.len(), offset, len) else {
                return Err(ReadError::OutOfRange);
            };
            if start != end {
                dst.write(0, &device[start..end]);
            }
            Ok(ReadResult { bytes_read: len })
        });
        (dst, result)
    }

    async fn write(
        _dev: disk::DiskImplBorrow<'_>,
        offset: u64,
        src: Buffer,
    ) -> (Buffer, Result<WriteResult, WriteError>) {
        let len = src.len();
        // Copy out of the buffer before taking the state borrow, so the device borrow is
        // never held across a call back into the buffers import.
        let bytes = if len == 0 {
            Vec::new()
        } else {
            src.read(0, len)
        };
        let result = with_device(|device| {
            let Some((start, end)) = range(device.len(), offset, len) else {
                return Err(WriteError::OutOfRange);
            };
            device[start..end].copy_from_slice(&bytes);
            Ok(WriteResult { bytes_written: len })
        });
        (src, result)
    }
}

export!(Stub);
