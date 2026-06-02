//! draw — the pixel-output example.
//!
//! Targets the `eo9-examples:draw/draw` world (see `wit/world.wit`): queries the
//! framebuffer mode, draws the deterministic test pattern sized to it, reads the result
//! back through the same API, and reports the FNV-1a-64 checksum of the final frame —
//! so one program verifies any gfx provider end to end. With `--frames 2` the second
//! frame presents only the centered damage rectangle (the partial-damage path); with
//! `--check …` it probes that the API's typed errors actually come back typed.
//!
//! THE PATTERN IS REPLICATED VERBATIM in tests/eo9-integration/tests/gfx.rs and in
//! xtask's expected-image generator (`gfx_pattern` module) — the three copies must stay
//! in lockstep, which the integration tests and `check-gpu` enforce by construction
//! (a drifted copy fails the checksum / image comparison).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::gfx::gfx;
use eo9_guest::buffer;

eo9_guest::bindings!({
    world: "draw",
    apis: [io, gfx],
});

// ------------------------------------------------------------------------------------
// The deterministic test pattern (see the module docs: replicated in the integration
// tests and xtask — keep the three copies in lockstep).
// ------------------------------------------------------------------------------------

/// One pattern pixel (xrgb8888 channel values), for frame 1 or the frame-2 composite.
///
/// Layout, in precedence order: a one-pixel white border (off-by-one canary); three
/// quarter-size solid rectangles stepping down the diagonal (red, green, blue —
/// stacking-order canary); elsewhere an x-gradient in red, a y-gradient in green, and
/// an XOR texture in blue (orientation, scaling, addressing canaries). Frame 2 inverts
/// the centered half-size rectangle — the partial-damage canary.
fn pattern_pixel(frame: u32, width: u32, height: u32, x: u32, y: u32) -> (u8, u8, u8) {
    let base = base_pixel(width, height, x, y);
    if frame >= 2 && in_damage_rect(width, height, x, y) {
        return (255 - base.0, 255 - base.1, 255 - base.2);
    }
    base
}

/// The frame-2 damage rectangle: centered, half the mode's size.
fn damage_rect(width: u32, height: u32) -> (u32, u32, u32, u32) {
    (width / 4, height / 4, width / 2, height / 2)
}

fn in_damage_rect(width: u32, height: u32, x: u32, y: u32) -> bool {
    let (dx, dy, dw, dh) = damage_rect(width, height);
    x >= dx && x < dx + dw && y >= dy && y < dy + dh
}

fn base_pixel(width: u32, height: u32, x: u32, y: u32) -> (u8, u8, u8) {
    if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
        return (255, 255, 255);
    }
    let qw = width / 4;
    let qh = height / 4;
    let in_quarter = |x0: u32, y0: u32| x >= x0 && x < x0 + qw && y >= y0 && y < y0 + qh;
    if in_quarter(width / 8, height / 8) {
        return (255, 32, 32);
    }
    if in_quarter(3 * width / 8, 3 * height / 8) {
        return (32, 255, 32);
    }
    if in_quarter(5 * width / 8, 5 * height / 8) {
        return (32, 32, 255);
    }
    (
        ((x as u64 * 255) / u64::from(width - 1).max(1)) as u8,
        ((y as u64 * 255) / u64::from(height - 1).max(1)) as u8,
        ((x ^ y) & 0xff) as u8,
    )
}

/// FNV-1a 64 over a byte slice.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Tightly packed xrgb8888 rows of `frame`'s pattern for the given rectangle.
fn pattern_bytes(frame: u32, width: u32, height: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
    let (rx, ry, rw, rh) = rect;
    let mut out = Vec::with_capacity(rw as usize * rh as usize * 4);
    for y in ry..ry + rh {
        for x in rx..rx + rw {
            let (r, g, b) = pattern_pixel(frame, width, height, x, y);
            out.extend_from_slice(&[b, g, r, 0]);
        }
    }
    out
}

// ------------------------------------------------------------------------------------
// The program.
// ------------------------------------------------------------------------------------

eo9_guest::main! {
    async fn main(frames: Option<u32>, check: Option<String>) -> Result<ProgramSuccess, ProgramFailure> {
        let gfx_failure = |err: gfx::GfxError| ProgramFailure::Gfx(format!("{err:?}"));

        let g = gfx::default();
        let mode = gfx::mode(&g).map_err(gfx_failure)?;
        if mode.width < 8 || mode.height < 8 {
            return Err(ProgramFailure::BadArguments(format!(
                "the pattern needs a mode of at least 8x8, got {}x{}",
                mode.width, mode.height
            )));
        }

        // `--check …`: probe that the API's typed errors come back typed.
        if let Some(probe) = check {
            return run_probe(&g, &mode, &probe).await;
        }

        let frames = frames.unwrap_or(1);
        if frames == 0 || frames > 2 {
            return Err(ProgramFailure::BadArguments(String::from(
                "--frames must be 1 or 2",
            )));
        }

        let full = gfx::Rect { x: 0, y: 0, width: mode.width, height: mode.height };

        // Start from a known state (and exercise `clear`): all black.
        gfx::clear(&g, full, 0).await.map_err(gfx_failure)?;

        // Frame 1: the full pattern.
        let frame1 = pattern_bytes(1, mode.width, mode.height, (0, 0, mode.width, mode.height));
        let src = buffer::from_bytes(&frame1);
        let (_src, presented) = gfx::present(&g, full, src).await;
        presented.map_err(gfx_failure)?;

        // Frame 2 (optional): present only the centered damage rectangle.
        if frames == 2 {
            let (dx, dy, dw, dh) = damage_rect(mode.width, mode.height);
            let damage = pattern_bytes(2, mode.width, mode.height, (dx, dy, dw, dh));
            let src = buffer::from_bytes(&damage);
            let dst = gfx::Rect { x: dx, y: dy, width: dw, height: dh };
            let (_src, presented) = gfx::present(&g, dst, src).await;
            presented.map_err(gfx_failure)?;
        }

        // Read the final frame back through the same API and checksum it.
        let len = u64::from(mode.width) * u64::from(mode.height) * 4;
        let dst = buffer::with_capacity(len);
        let (dst, read) = gfx::read(&g, full, dst).await;
        read.map_err(gfx_failure)?;
        let pixels = buffer::prefix_to_vec(&dst, len);
        Ok(ProgramSuccess::Presented(fnv1a64(&pixels)))
    }
}

/// `--check oob-rect` / `--check short-buffer`: each must produce exactly its typed error.
async fn run_probe(
    g: &gfx::GfxImpl,
    mode: &gfx::ModeInfo,
    probe: &str,
) -> Result<ProgramSuccess, ProgramFailure> {
    match probe {
        "oob-rect" => {
            // One pixel just past the right edge: in-range buffer, out-of-range rect.
            let dst = gfx::Rect {
                x: mode.width,
                y: 0,
                width: 1,
                height: 1,
            };
            let src = buffer::from_bytes(&[0, 0, 0, 0]);
            let (_src, result) = gfx::present(g, dst, src).await;
            match result {
                Err(gfx::GfxError::OutOfBounds) => {
                    Ok(ProgramSuccess::Probed(String::from("out-of-bounds")))
                }
                other => Err(ProgramFailure::Unexpected(format!(
                    "an out-of-bounds present answered {other:?}"
                ))),
            }
        }
        "short-buffer" => {
            // The full mode with a four-byte buffer.
            let dst = gfx::Rect {
                x: 0,
                y: 0,
                width: mode.width,
                height: mode.height,
            };
            let src = buffer::from_bytes(&[0, 0, 0, 0]);
            let (_src, result) = gfx::present(g, dst, src).await;
            match result {
                Err(gfx::GfxError::BadBuffer(_)) => {
                    Ok(ProgramSuccess::Probed(String::from("bad-buffer")))
                }
                other => Err(ProgramFailure::Unexpected(format!(
                    "an undersized present answered {other:?}"
                ))),
            }
        }
        other => Err(ProgramFailure::BadArguments(format!(
            "unknown --check probe {other:?} (oob-rect, short-buffer)"
        ))),
    }
}
