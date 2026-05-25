//! Helpers for the `eo9:io/buffers` owned-buffer round-trip.
//!
//! I/O buffers are opaque, possibly DMA-backed resources: a program transfers an owned
//! `buffer` to a backend and receives it back when the operation completes, on both the
//! success and the error path (see SPEC.md, "Ownership and buffers"). These helpers
//! cover the two ends of that round-trip from the guest's point of view — filling a
//! fresh buffer from guest memory before an operation, and copying a returned buffer's
//! contents back out afterwards.

pub use crate::api::io::buffers::Buffer;

use alloc::vec::Vec;

/// Allocate a buffer of exactly `bytes.len()` bytes and copy `bytes` into it.
pub fn from_bytes(bytes: &[u8]) -> Buffer {
    let buffer = Buffer::new(bytes.len() as u64);
    if !bytes.is_empty() {
        buffer.write(0, bytes);
    }
    buffer
}

/// Allocate a zero-filled buffer of `len` bytes (e.g. as the destination of a read).
pub fn with_capacity(len: u64) -> Buffer {
    Buffer::new(len)
}

/// Copy the first `len` bytes of `buffer` into guest memory.
///
/// Traps if `len` exceeds the buffer's capacity, mirroring the underlying accessor.
pub fn prefix_to_vec(buffer: &Buffer, len: u64) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    buffer.read(0, len)
}

/// Copy the entire contents of `buffer` into guest memory.
pub fn to_vec(buffer: &Buffer) -> Vec<u8> {
    prefix_to_vec(buffer, buffer.len())
}
