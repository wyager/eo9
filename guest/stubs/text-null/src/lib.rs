//! `text.null` — accept and discard standard text streams.
//!
//! Targets the `eo9:text/null` stub world: exports `eo9:text/text` where writes to
//! stdout/stderr succeed and are discarded, and stdin is permanently at end of input
//! (`read-line` answers `none`). Composing a program with `text.null` gives it the API
//! surface without anywhere for the text to go (see SPEC.md, "Text API").

#![no_std]

extern crate alloc;

use alloc::string::String;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "null",
    path: "../../../wit/text",
});

use exports::eo9::text::null_config;
use exports::eo9::text::text::{self, OutputStream, TextError};
use exports::eo9::text::types;

/// The `text.null` provider.
struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct NullText;

impl types::Guest for Stub {
    type TextImpl = NullText;
}

impl types::GuestTextImpl for NullText {}

impl null_config::Guest for Stub {
    async fn configure() -> Result<types::TextImpl, String> {
        Ok(types::TextImpl::new(NullText))
    }
}

impl text::Guest for Stub {
    fn default() -> types::TextImpl {
        types::TextImpl::new(NullText)
    }

    fn write(
        _t: text::TextImplBorrow<'_>,
        _to: OutputStream,
        _text: String,
    ) -> Result<(), TextError> {
        Ok(())
    }

    async fn read_line(_t: text::TextImplBorrow<'_>) -> Result<Option<String>, TextError> {
        Ok(None)
    }
}

export!(Stub);
