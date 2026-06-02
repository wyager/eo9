//! `gfx.mem` — a RAM-backed framebuffer.
//!
//! Targets the `eo9:gfx/mem` stub world: exports `eo9:gfx/gfx` over an in-memory
//! xrgb8888 framebuffer whose geometry is bound by `configure`. The deterministic gfx
//! environment: presents and reads are a pure function of the program's own operations,
//! so a drawing program's readback checksum is reproducible everywhere.
//!
//! The documented default state is a zero-filled (all-black) 640x480 framebuffer: an
//! unconfigured `gfx.mem` self-initializes to it on first use, so plain
//! `gfx.mem $ program` works and never traps (the default-configuration rule, plan/09
//! Decision 14). `configure(width, height)` still binds an explicit geometry.
//!
//! Semantics (the API contract, wit/gfx): operation buffers are tightly packed rows of
//! the operation's rectangle; the framebuffer stride here is exactly `width * 4`; a
//! rectangle that does not lie entirely within the mode fails with `out-of-bounds`; a
//! buffer smaller than the rectangle fails with `bad-buffer`; zero-area rectangles
//! succeed and change nothing.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "mem",
    path: "../../../wit/gfx",
    // Pull in bindings for eo9:io/buffers, which the exported gfx interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::gfx::gfx::{self, Buffer, GfxError, ModeInfo, PixelFormat, Rect};
use exports::eo9::gfx::mem_config;
use exports::eo9::gfx::types;

/// Bytes per xrgb8888 pixel.
const BYTES_PER_PIXEL: u32 = 4;

/// Geometry bounds for `configure`: each dimension 1..=4096 (a 4096x4096 framebuffer is
/// 64 MiB — plenty for a stub, bounded enough to never exhaust a guest heap by typo).
const MAX_DIM: u32 = 4096;

/// The framebuffer contents and geometry, bound by `configure`.
struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

static STATE: ProviderState<Framebuffer> = ProviderState::new();

/// The documented default geometry of an unconfigured `gfx.mem`.
const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 480;

fn make_framebuffer(width: u32, height: u32) -> Framebuffer {
    let len = (width as usize) * (height as usize) * BYTES_PER_PIXEL as usize;
    Framebuffer {
        width,
        height,
        pixels: vec![0; len],
    }
}

/// Run `f` over the framebuffer. An unconfigured `gfx.mem` defaults to the documented
/// zero-filled 640x480 framebuffer, so it never traps when used without `configure`.
fn with_framebuffer<R>(f: impl FnOnce(&mut Framebuffer) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(make_framebuffer(DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }
    STATE.with(f)
}

/// Validate `rect` against the mode; returns the per-row byte count and the byte offset
/// of the rectangle's first pixel for a stride of `width * 4`.
fn check_rect(fb: &Framebuffer, rect: &Rect) -> Result<(usize, usize, usize), GfxError> {
    let end_x = rect.x.checked_add(rect.width);
    let end_y = rect.y.checked_add(rect.height);
    let (Some(end_x), Some(end_y)) = (end_x, end_y) else {
        return Err(GfxError::OutOfBounds);
    };
    if end_x > fb.width || end_y > fb.height {
        return Err(GfxError::OutOfBounds);
    }
    let stride = fb.width as usize * BYTES_PER_PIXEL as usize;
    let row_bytes = rect.width as usize * BYTES_PER_PIXEL as usize;
    let start = rect.y as usize * stride + rect.x as usize * BYTES_PER_PIXEL as usize;
    Ok((stride, row_bytes, start))
}

/// Validate that `buffer_len` covers the rectangle's tightly packed pixels.
fn check_buffer(rect: &Rect, buffer_len: u64) -> Result<u64, GfxError> {
    let needed = rect.width as u64 * rect.height as u64 * BYTES_PER_PIXEL as u64;
    if buffer_len < needed {
        return Err(GfxError::BadBuffer(format!(
            "the rectangle needs {needed} bytes ({}x{} xrgb8888), the buffer holds {buffer_len}",
            rect.width, rect.height
        )));
    }
    Ok(needed)
}

/// The `gfx.mem` provider.
struct Stub;

/// The root-handle resource: a token referring to the configured framebuffer.
struct MemGfx;

impl types::Guest for Stub {
    type GfxImpl = MemGfx;
}

impl types::GuestGfxImpl for MemGfx {}

impl mem_config::Guest for Stub {
    fn configure(width: u32, height: u32) -> Result<types::GfxImpl, String> {
        if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
            return Err(format!(
                "framebuffer geometry must be 1..={MAX_DIM} on each side, got {width}x{height}"
            ));
        }
        STATE.set(make_framebuffer(width, height));
        Ok(types::GfxImpl::new(MemGfx))
    }
}

impl gfx::Guest for Stub {
    fn default() -> types::GfxImpl {
        types::GfxImpl::new(MemGfx)
    }

    fn mode(_g: gfx::GfxImplBorrow<'_>) -> Result<ModeInfo, GfxError> {
        with_framebuffer(|fb| {
            Ok(ModeInfo {
                width: fb.width,
                height: fb.height,
                stride: fb.width * BYTES_PER_PIXEL,
                format: PixelFormat::Xrgb8888,
            })
        })
    }

    async fn present(
        _g: gfx::GfxImplBorrow<'_>,
        dst: Rect,
        src: Buffer,
    ) -> (Buffer, Result<(), GfxError>) {
        // Copy out of the buffer before taking the state borrow, so the framebuffer
        // borrow is never held across a call back into the buffers import.
        let checked = check_buffer(&dst, src.len());
        let bytes = match &checked {
            Ok(needed) if *needed > 0 => src.read(0, *needed),
            _ => Vec::new(),
        };
        let result = with_framebuffer(|fb| {
            let (stride, row_bytes, start) = check_rect(fb, &dst)?;
            checked?;
            for row in 0..dst.height as usize {
                let fb_off = start + row * stride;
                let src_off = row * row_bytes;
                fb.pixels[fb_off..fb_off + row_bytes]
                    .copy_from_slice(&bytes[src_off..src_off + row_bytes]);
            }
            Ok(())
        });
        (src, result)
    }

    async fn read(
        _g: gfx::GfxImplBorrow<'_>,
        src: Rect,
        dst: Buffer,
    ) -> (Buffer, Result<(), GfxError>) {
        // Gather the rows under the state borrow, write to the buffer after releasing it.
        let gathered = with_framebuffer(|fb| {
            let (stride, row_bytes, start) = check_rect(fb, &src)?;
            check_buffer(&src, dst.len())?;
            let mut out = Vec::with_capacity(row_bytes * src.height as usize);
            for row in 0..src.height as usize {
                let fb_off = start + row * stride;
                out.extend_from_slice(&fb.pixels[fb_off..fb_off + row_bytes]);
            }
            Ok(out)
        });
        let result = match gathered {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    dst.write(0, &bytes);
                }
                Ok(())
            }
            Err(err) => Err(err),
        };
        (dst, result)
    }

    async fn clear(_g: gfx::GfxImplBorrow<'_>, dst: Rect, color: u32) -> Result<(), GfxError> {
        with_framebuffer(|fb| {
            let (stride, row_bytes, start) = check_rect(fb, &dst)?;
            let pixel = color.to_le_bytes();
            for row in 0..dst.height as usize {
                let fb_off = start + row * stride;
                for chunk in fb.pixels[fb_off..fb_off + row_bytes].chunks_exact_mut(4) {
                    chunk.copy_from_slice(&pixel);
                }
            }
            Ok(())
        })
    }
}

export!(Stub);
