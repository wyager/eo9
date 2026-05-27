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

eo9_guest::main! {
    fn main(text: String) -> Result<ProgramSuccess, ProgramFailure> {
        text::write_out_line(&text)
            .map_err(|e| ProgramFailure::Io(format!("{e:?}")))?;
        Ok(ProgramSuccess::Done)
    }
}
