//! Convenience wrappers over `eo9:text/text` (std{in,out,err}).
//!
//! Each one-shot helper obtains the capability's root handle via the `default()`
//! accessor, performs a single operation, and drops the handle again. Programs doing
//! repeated text I/O should call [`default()`] once and use the raw bindings in
//! [`crate::api::text`] with the held handle.

pub use crate::api::text::text::{OutputStream, TextError, TextImpl};

use crate::api::text::text as raw;

/// The text capability's root handle (the `default()` accessor; see SPEC.md,
/// "The capability algebra").
pub fn default() -> TextImpl {
    raw::default()
}

/// Write `text` to standard output.
pub fn write_out(text: &str) -> Result<(), TextError> {
    raw::write(&raw::default(), OutputStream::Out, text)
}

/// Write `text` to standard error.
pub fn write_err(text: &str) -> Result<(), TextError> {
    raw::write(&raw::default(), OutputStream::Err, text)
}

/// Write `text` followed by a newline to standard output.
pub fn write_out_line(text: &str) -> Result<(), TextError> {
    let handle = raw::default();
    raw::write(&handle, OutputStream::Out, text)?;
    raw::write(&handle, OutputStream::Out, "\n")
}

/// Write `text` followed by a newline to standard error.
pub fn write_err_line(text: &str) -> Result<(), TextError> {
    let handle = raw::default();
    raw::write(&handle, OutputStream::Err, text)?;
    raw::write(&handle, OutputStream::Err, "\n")
}
