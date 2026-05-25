//! Root provider for `eo9:entropy` — the host OS's cryptographically secure RNG.
//!
//! Both operations are synchronous (matching the WIT signatures): the OS entropy source
//! never blocks meaningfully once seeded, so there is nothing to complete asynchronously
//! and nothing in flight for a kill to interact with.
//!
//! The deterministic flavor (`entropy.seeded`) is a guest-side stub provider (area 09),
//! not part of this crate.

use crate::buffer::OwnedBuffer;

/// The host trait mirroring the WIT `eo9:entropy/entropy` interface (minus `default`).
pub trait EntropyHost: Send + Sync {
    /// `len` cryptographically secure random bytes.
    fn get_bytes(&self, len: u64) -> Vec<u8>;
    /// A single random 64-bit value.
    fn get_u64(&self) -> u64;
}

/// The unix entropy provider, backed by the OS RNG (`getrandom`, i.e. `getentropy(2)` /
/// `getrandom(2)`). Corresponds to the WIT `entropy-impl` root handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct EntropyProvider;

impl EntropyProvider {
    /// A provider reading the host's RNG.
    pub fn new() -> Self {
        Self
    }

    /// Fill an owned buffer with random bytes and hand it back (a convenience for
    /// callers already working in owned buffers; not part of the WIT surface).
    pub fn fill_buffer(&self, mut buffer: OwnedBuffer) -> OwnedBuffer {
        fill(buffer.as_mut_slice());
        buffer
    }
}

impl EntropyHost for EntropyProvider {
    fn get_bytes(&self, len: u64) -> Vec<u8> {
        let len = usize::try_from(len).expect("entropy request exceeds host address space");
        let mut bytes = vec![0u8; len];
        fill(&mut bytes);
        bytes
    }

    fn get_u64(&self) -> u64 {
        let mut bytes = [0u8; 8];
        fill(&mut bytes);
        u64::from_le_bytes(bytes)
    }
}

/// The WIT surface has no error path: a failing OS RNG is unrecoverable misconfiguration
/// of the host, so it is a panic here rather than a silently weaker random stream.
fn fill(dest: &mut [u8]) {
    getrandom::fill(dest).expect("host OS randomness source failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_bytes_returns_the_requested_length() {
        let provider = EntropyProvider::new();
        assert_eq!(provider.get_bytes(0).len(), 0);
        assert_eq!(provider.get_bytes(1).len(), 1);
        assert_eq!(provider.get_bytes(4096).len(), 4096);
    }

    #[test]
    fn independent_draws_differ() {
        let provider = EntropyProvider::new();
        // 32 bytes of CSPRNG output colliding is beyond astronomically unlikely; a
        // failure here means the provider is not actually reading the OS RNG.
        assert_ne!(provider.get_bytes(32), provider.get_bytes(32));
        let draws: Vec<u64> = (0..4).map(|_| provider.get_u64()).collect();
        assert!(draws.windows(2).any(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn fill_buffer_round_trips_ownership() {
        let provider = EntropyProvider::new();
        let buffer = provider.fill_buffer(OwnedBuffer::new(64));
        assert_eq!(buffer.len(), 64);
        assert_ne!(buffer.as_slice(), &[0u8; 64]);
    }
}
