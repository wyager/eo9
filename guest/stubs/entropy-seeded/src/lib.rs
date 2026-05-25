//! `entropy.seeded` — deterministic randomness from a fixed seed.
//!
//! Targets the `eo9:entropy/seeded` stub world: exports `eo9:entropy/entropy` backed by
//! a deterministic PRNG (SplitMix64) whose seed is bound by `configure`. Together with
//! `fs.memfs`, `time.frozen`, and `disk.mem` this forms the deterministic environment of
//! integration milestone I2 — the same seed always produces the same byte stream.
//!
//! Not cryptographically secure; it is a reproducible stand-in for tests and
//! deterministic runs (see SPEC.md, "Entropy API").

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "seeded",
    path: "../../../wit/entropy",
});

use exports::eo9::entropy::entropy;
use exports::eo9::entropy::seeded_config;
use exports::eo9::entropy::types;

/// The PRNG state: the SplitMix64 counter, bound by `configure`.
static STATE: ProviderState<u64> = ProviderState::new();

/// One step of SplitMix64: advance the counter and return the next 64-bit output.
fn next_u64(counter: &mut u64) -> u64 {
    *counter = counter.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *counter;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The `entropy.seeded` provider.
struct Stub;

/// The root-handle resource: a token referring to the shared PRNG state.
struct SeededEntropy;

impl types::Guest for Stub {
    type EntropyImpl = SeededEntropy;
}

impl types::GuestEntropyImpl for SeededEntropy {}

impl seeded_config::Guest for Stub {
    async fn configure(seed: u64) -> Result<types::EntropyImpl, String> {
        STATE.set(seed);
        Ok(types::EntropyImpl::new(SeededEntropy))
    }
}

impl entropy::Guest for Stub {
    fn default() -> types::EntropyImpl {
        types::EntropyImpl::new(SeededEntropy)
    }

    fn get_bytes(_e: entropy::EntropyImplBorrow<'_>, len: u64) -> Vec<u8> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        STATE.with(|counter| {
            let mut bytes = Vec::with_capacity(len);
            while bytes.len() < len {
                let word = next_u64(counter).to_le_bytes();
                let take = usize::min(8, len - bytes.len());
                bytes.extend_from_slice(&word[..take]);
            }
            bytes
        })
    }

    fn get_u64(_e: entropy::EntropyImplBorrow<'_>) -> u64 {
        STATE.with(next_u64)
    }
}

export!(Stub);
