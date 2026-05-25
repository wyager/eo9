//! cruncher — pure compute, no imports.
//!
//! Targets the `eo9-examples:cruncher/cruncher` world (see `wit/world.wit`): a fully
//! closed program whose only observable behaviour is its typed outcome, making it the
//! fixture for `only`/sandbox.pure demos, compile-cache determinism tests, and fuel
//! accounting (the work scales linearly with `rounds`).

#![no_std]

extern crate alloc;

use alloc::string::String;

// Nothing from the SDK is called directly (this is a closed program), but the guest
// runtime profile — allocator and panic handler — still comes from it.
use eo9_guest as _;

eo9_guest::bindings!({
    world: "cruncher",
    apis: [],
});

/// One round of the splitmix64 mixing function: a cheap, well-distributed 64-bit
/// permutation, iterated to give the host a tunable amount of pure compute.
fn mix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

eo9_guest::main! {
    fn main(seed: u64, rounds: u64) -> Result<ProgramSuccess, ProgramFailure> {
        if rounds == 0 {
            return Err(ProgramFailure::BadArguments(String::from(
                "rounds must be at least 1",
            )));
        }

        let mut digest = seed;
        for _ in 0..rounds {
            digest = mix(digest);
        }
        Ok(ProgramSuccess::Digest(digest))
    }
}
