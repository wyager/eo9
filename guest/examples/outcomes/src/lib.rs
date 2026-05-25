//! outcomes — conformance fixture for the typed outcome vocabulary.
//!
//! Targets the `eo9-examples:outcomes/outcomes` world (see `wit/world.wit`): produces
//! whichever outcome its arguments request — success variants, failure variants,
//! argument rejection, or a guest trap — so every outcome path can be exercised from
//! the host side.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::text;

eo9_guest::bindings!({
    world: "outcomes",
    apis: [text],
});

eo9_guest::main! {
    fn main(mode: String, detail: String) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));

        match mode.as_str() {
            "ok" => {
                text::write_out_line(&format!("outcomes: completing with {detail:?}"))
                    .map_err(io_failure)?;
                Ok(ProgramSuccess::Completed(detail))
            }
            "quiet" => Ok(ProgramSuccess::Quiet),
            "fail" => {
                text::write_err_line(&format!("outcomes: failing with {detail:?}"))
                    .map_err(io_failure)?;
                Err(ProgramFailure::RequestedFailure(detail))
            }
            // A guest panic lowers to the wasm `unreachable` instruction (see the SDK's
            // runtime profile), so the host observes a trap rather than an outcome.
            "trap" => panic!("outcomes: trapping as requested"),
            other => Err(ProgramFailure::BadArguments(format!(
                "unknown mode {other:?}: expected ok, quiet, fail, or trap"
            ))),
        }
    }
}
