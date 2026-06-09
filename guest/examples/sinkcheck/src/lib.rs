//! sinkcheck — the console-sink fake injector (the M4 mechanics test).
//!
//! Targets the `eo9-examples:sinkcheck/sinkcheck` world (see `wit/world.wit`):
//! injects `<text>\n` into the kernel console input ring and reports the accepted
//! count. The acceptance is what happens NEXT: the eosh prompt that follows this
//! program's exit reads the injected line as if the operator typed it — `sinkcheck
//! --text hello` is followed by `ok: greeted`. The USB keyboard service (`usb.kbd`)
//! is this program with a HID decoder in front.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::console_sink::sink;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "sinkcheck",
    apis: [console_sink, text],
});

eo9_guest::main! {
    async fn main(text: Option<String>) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));
        let line = text.unwrap_or_else(|| String::from("hello"));
        if line.len() > 1024 {
            return Err(ProgramFailure::BadArguments(String::from(
                "--text is limited to 1024 bytes",
            )));
        }

        let mut bytes: Vec<u8> = line.into_bytes();
        bytes.push(b'\n');
        let total = bytes.len() as u32;

        let root = sink::default();
        let accepted = sink::inject(&root, &bytes).map_err(|err| match err {
            sink::SinkError::Denied => ProgramFailure::Denied,
            sink::SinkError::Io(message) => ProgramFailure::Io(message),
        })?;

        text::write_out_line(&format!(
            "sinkcheck: injected {accepted}/{total} byte(s) into the console input ring \
             (the next prompt reads them as typed input)"
        ))
        .map_err(io_failure)?;
        Ok(ProgramSuccess::Injected(accepted))
    }
}
