//! hello — the introductory Eo9 example program.
//!
//! Targets the `eo9-examples:hello/hello` world (see `wit/world.wit`): imports
//! `eo9:text/text` and `eo9:time/time`, takes named typed arguments (`name`, `excited`,
//! both optional — `name` defaults to "world", `excited` to false), and reports its
//! outcome through the world's own success/failure variants.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::{text, time};

eo9_guest::bindings!({
    world: "hello",
    apis: [text, time],
});

eo9_guest::main! {
    fn main(name: Option<String>, excited: Option<bool>) -> Result<ProgramSuccess, ProgramFailure> {
        let name = name.unwrap_or_else(|| String::from("world"));
        let excited = excited.unwrap_or(false);
        if name.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from(
                "name must not be empty",
            )));
        }

        let now = time::now();
        let punctuation = if excited { "!" } else { "." };
        let greeting = format!(
            "[{}.{:09}] Hello, {name}{punctuation}",
            now.seconds, now.nanoseconds
        );

        text::write_out_line(&greeting).map_err(|err| ProgramFailure::Io(format!("{err:?}")))?;
        Ok(ProgramSuccess::Greeted)
    }
}
