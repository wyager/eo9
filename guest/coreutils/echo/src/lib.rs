//! echo — write text to stdout (eo9:text only; a minimal-capability tool).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

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
    /// `echo <word>…` — write the words to stdout joined by single spaces (variadic
    /// tail, like cat's paths). A bare `echo` prints an empty line. The legacy
    /// named-flag spelling `echo --text hi` binds the one-element list.
    fn main(text: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        text::write_out_line(&text.join(" ")).map_err(io_fail)?;
        Ok(ProgramSuccess::Done)
    }
}
