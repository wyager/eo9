//! Convenience wrappers over `eo9:entropy/entropy`.
//!
//! Each one-shot helper obtains the capability's root handle via the `default()`
//! accessor, performs a single operation, and drops the handle again. Programs drawing
//! lots of randomness should call [`default()`] once and use the raw bindings in
//! [`crate::api::entropy`] with the held handle.

pub use crate::api::entropy::entropy::EntropyImpl;

use alloc::vec::Vec;

use crate::api::entropy::entropy as raw;

/// The entropy capability's root handle (the `default()` accessor; see SPEC.md,
/// "The capability algebra").
pub fn default() -> EntropyImpl {
    raw::default()
}

/// Return `len` random bytes.
pub fn random_bytes(len: u64) -> Vec<u8> {
    raw::get_bytes(&raw::default(), len)
}

/// Return a single random 64-bit value.
pub fn random_u64() -> u64 {
    raw::get_u64(&raw::default())
}
