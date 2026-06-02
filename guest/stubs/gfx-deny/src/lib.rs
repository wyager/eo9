//! `gfx.deny` — the pixel-output capability, present but refusing.
//!
//! Targets the `eo9:gfx/deny` stub world: exports `eo9:gfx/gfx` where every operation
//! fails with the API's own `denied` error. Composed as `gfx.deny $ program`, a drawing
//! program observes a framebuffer it is not allowed to touch — instead of the absence
//! `gfx.none` models or the unsatisfied import the loader would otherwise refuse at
//! spawn (see SPEC.md, "The capability algebra").

#![no_std]

extern crate alloc;

use alloc::string::String;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "deny",
    path: "../../../wit/gfx",
    generate_all,
});

use exports::eo9::gfx::deny_config;
use exports::eo9::gfx::gfx::{self, Buffer, GfxError, ModeInfo, Rect};
use exports::eo9::gfx::types;

/// The `gfx.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no framebuffer behind it.
struct DenyGfx;

impl types::Guest for Stub {
    type GfxImpl = DenyGfx;
}

impl types::GuestGfxImpl for DenyGfx {}

impl deny_config::Guest for Stub {
    fn configure() -> Result<types::GfxImpl, String> {
        Ok(types::GfxImpl::new(DenyGfx))
    }
}

impl gfx::Guest for Stub {
    fn default() -> types::GfxImpl {
        types::GfxImpl::new(DenyGfx)
    }

    fn mode(_g: gfx::GfxImplBorrow<'_>) -> Result<ModeInfo, GfxError> {
        Err(GfxError::Denied)
    }

    async fn present(
        _g: gfx::GfxImplBorrow<'_>,
        _dst: Rect,
        src: Buffer,
    ) -> (Buffer, Result<(), GfxError>) {
        (src, Err(GfxError::Denied))
    }

    async fn read(
        _g: gfx::GfxImplBorrow<'_>,
        _src: Rect,
        dst: Buffer,
    ) -> (Buffer, Result<(), GfxError>) {
        (dst, Err(GfxError::Denied))
    }

    async fn clear(_g: gfx::GfxImplBorrow<'_>, _dst: Rect, _color: u32) -> Result<(), GfxError> {
        Err(GfxError::Denied)
    }
}

export!(Stub);
