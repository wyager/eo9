//! echo — write text to stdout (eo9:text only; a minimal-capability tool).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::text;

eo9_guest::bindings!({
    world: "echo",
    apis: [text],
});

/// Human text for an output failure — the typed vocabulary, never Rust debug
/// formatting (the R2-18 enum-leak class).
fn io_fail(e: text::TextError) -> ProgramFailure {
    ProgramFailure::Io(match e {
        text::TextError::Closed => String::from("output closed"),
        text::TextError::Io(m) => format!("io: {m}"),
    })
}

eo9_guest::main! {
    fn main(text: String) -> Result<ProgramSuccess, ProgramFailure> {
        text::write_out_line(&text).map_err(io_fail)?;
        Ok(ProgramSuccess::Done)
    }
}
