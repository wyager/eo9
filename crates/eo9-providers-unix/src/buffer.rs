//! The host-side value behind the `eo9:io/buffers.buffer` resource.
//!
//! Guests see `buffer` as an opaque resource transferred by ownership; on the host side
//! it is this plain, uniquely-owned byte block. Providers take it by value for the life
//! of an operation and hand it back inside the completion value (on success and error
//! alike), which is exactly the owned-buffer round-trip the spec requires.

use std::fmt;

/// A uniquely-owned block of bytes, mirroring the WIT `buffer` resource.
///
/// The `copy_out` / `copy_in` methods mirror the WIT `read` / `write` accessors (which
/// trap on out-of-range access — the host side reports a [`BufferRangeError`] and the
/// runtime turns that into the trap). The `as_slice` / `as_mut_slice` accessors are for
/// providers, which fill or drain the buffer directly.
pub struct OwnedBuffer {
    bytes: Box<[u8]>,
}

/// An out-of-bounds `copy_out` / `copy_in` access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferRangeError {
    /// Requested start offset.
    pub offset: u64,
    /// Requested length.
    pub len: u64,
    /// Actual buffer capacity.
    pub capacity: u64,
}

impl fmt::Display for BufferRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "buffer access out of range: offset {} + len {} exceeds capacity {}",
            self.offset, self.len, self.capacity
        )
    }
}

impl std::error::Error for BufferRangeError {}

impl OwnedBuffer {
    /// A zero-filled buffer of `len` bytes (the WIT `constructor(len)`).
    ///
    /// # Panics
    ///
    /// Panics if `len` does not fit in the host's address space. Enforcing sane guest
    /// allocation sizes is the runtime's resource-limit business, not the buffer's.
    pub fn new(len: u64) -> Self {
        let len = usize::try_from(len).expect("buffer length exceeds host address space");
        Self {
            bytes: vec![0u8; len].into_boxed_slice(),
        }
    }

    /// Takes ownership of existing bytes.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self {
            bytes: bytes.into_boxed_slice(),
        }
    }

    /// Total capacity in bytes (the WIT `len()`).
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Whether the buffer has zero capacity.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Copies `len` bytes starting at `offset` out of the buffer (the WIT `read`).
    pub fn copy_out(&self, offset: u64, len: u64) -> Result<Vec<u8>, BufferRangeError> {
        let range = self.check_range(offset, len)?;
        Ok(self.bytes[range].to_vec())
    }

    /// Copies `bytes` into the buffer starting at `offset` (the WIT `write`).
    pub fn copy_in(&mut self, offset: u64, bytes: &[u8]) -> Result<(), BufferRangeError> {
        let range = self.check_range(offset, bytes.len() as u64)?;
        self.bytes[range].copy_from_slice(bytes);
        Ok(())
    }

    /// The whole buffer as a byte slice (provider-side access).
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// The whole buffer as a mutable byte slice (provider-side access).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Consumes the buffer, returning its bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes.into_vec()
    }

    fn check_range(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<std::ops::Range<usize>, BufferRangeError> {
        let error = BufferRangeError {
            offset,
            len,
            capacity: self.len(),
        };
        let end = offset.checked_add(len).ok_or(error)?;
        if end > self.len() {
            return Err(error);
        }
        // Both bounds fit in the buffer, whose length is a usize, so these cannot fail.
        Ok(usize::try_from(offset).unwrap()..usize::try_from(end).unwrap())
    }
}

impl fmt::Debug for OwnedBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedBuffer")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buffer_is_zero_filled() {
        let buf = OwnedBuffer::new(8);
        assert_eq!(buf.len(), 8);
        assert!(!buf.is_empty());
        assert_eq!(buf.copy_out(0, 8).unwrap(), vec![0u8; 8]);
    }

    #[test]
    fn copy_in_then_copy_out_round_trips() {
        let mut buf = OwnedBuffer::new(16);
        buf.copy_in(4, b"hello").unwrap();
        assert_eq!(buf.copy_out(4, 5).unwrap(), b"hello");
        assert_eq!(buf.copy_out(0, 4).unwrap(), vec![0u8; 4]);
    }

    #[test]
    fn out_of_range_access_is_rejected() {
        let mut buf = OwnedBuffer::new(4);
        assert!(buf.copy_out(2, 3).is_err());
        assert!(buf.copy_out(5, 0).is_err());
        assert!(buf.copy_in(3, b"ab").is_err());
        // Offsets that would overflow u64 are rejected, not wrapped.
        assert!(buf.copy_out(u64::MAX, 2).is_err());
    }

    #[test]
    fn zero_length_access_at_the_end_is_allowed() {
        let buf = OwnedBuffer::new(4);
        assert_eq!(buf.copy_out(4, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn from_vec_and_into_vec_round_trip() {
        let buf = OwnedBuffer::from_vec(vec![1, 2, 3]);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.into_vec(), vec![1, 2, 3]);
    }
}
