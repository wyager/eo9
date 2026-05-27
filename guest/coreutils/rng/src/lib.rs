//! rng — print `count` random u64s, one per line (eo9:entropy + eo9:text).
//! Imports entropy, so `entropy.seeded $ rng --count 5` is deterministic.
#![no_std]
extern crate alloc;

use alloc::format;

use eo9_guest::{entropy, text};

eo9_guest::bindings!({
    world: "rng",
    apis: [entropy, text],
});

eo9_guest::main! {
    fn main(count: u64) -> Result<ProgramSuccess, ProgramFailure> {
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));
        let mut generated = 0u32;
        for _ in 0..count {
            let value = entropy::random_u64();
            text::write_out_line(&format!("{value}")).map_err(io_err)?;
            generated += 1;
        }
        Ok(ProgramSuccess::Generated(generated))
    }
}
