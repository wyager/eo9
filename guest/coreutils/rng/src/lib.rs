//! rng — print `count` random u64s, one per line (eo9:entropy + eo9:text).
//! Imports entropy, so `entropy.seeded $ rng --count 5` is deterministic.
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::{entropy, text};

eo9_guest::bindings!({
    world: "rng",
    apis: [entropy, text],
});

/// Human text for an output failure — the typed vocabulary, never Rust debug
/// formatting (the R2-18 enum-leak class).
fn io_fail(e: text::TextError) -> ProgramFailure {
    ProgramFailure::Io(match e {
        text::TextError::Closed => String::from("output closed"),
        text::TextError::Unsupported => String::from("io: unsupported operation"),
        text::TextError::Io(m) => format!("io: {m}"),
    })
}

eo9_guest::main! {
    fn main(count: u64) -> Result<ProgramSuccess, ProgramFailure> {
        let mut generated = 0u32;
        for _ in 0..count {
            let value = entropy::random_u64();
            text::write_out_line(&format!("{value}")).map_err(io_fail)?;
            generated += 1;
        }
        Ok(ProgramSuccess::Generated(generated))
    }
}
