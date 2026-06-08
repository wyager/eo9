//! `gfx.mem $ draw` — the pixel-output capability end to end (wit/gfx, plan/02).
//!
//! `gfx.mem` is the deterministic RAM framebuffer; `draw` draws the deterministic test
//! pattern, reads it back through the same API, and reports the FNV-1a-64 checksum.
//! These tests pin:
//!
//! * the full draw → present → read → checksum loop against an *independently computed*
//!   expected image (the pattern generator below is a verbatim copy of the demo's — a
//!   drifted copy or a provider addressing bug fails the checksum),
//! * the partial-damage path (`--frames 2` presents only the centered rectangle),
//! * the documented default geometry of an unconfigured `gfx.mem` (640x480, never traps),
//! * the typed errors (`out-of-bounds`, `bad-buffer`) via the demo's `--check` probes,
//! * `gfx.deny` answering in the API's own vocabulary (`denied`), composed and run.

use eo9_component::{Component, compose, configure};
use eo9_integration::{guest, run};
use eo9_runtime::{NamedArg, Outcome, Providers};

// ------------------------------------------------------------------------------------
// The deterministic test pattern — a verbatim copy of guest/examples/draw/src/lib.rs
// (and of xtask's expected-image generator); see the demo's module docs. Keep in
// lockstep.
// ------------------------------------------------------------------------------------

fn pattern_pixel(frame: u32, width: u32, height: u32, x: u32, y: u32) -> (u8, u8, u8) {
    let base = base_pixel(width, height, x, y);
    if frame >= 2 && in_damage_rect(width, height, x, y) {
        return (255 - base.0, 255 - base.1, 255 - base.2);
    }
    base
}

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

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The independently computed expectation: the FNV-1a-64 checksum of the final frame's
/// tightly packed xrgb8888 bytes (X byte zero) for a `frames`-frame run at WxH.
fn expected_checksum(frames: u32, width: u32, height: u32) -> u64 {
    let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = pattern_pixel(frames, width, height, x, y);
            bytes.extend_from_slice(&[b, g, r, 0]);
        }
    }
    fnv1a64(&bytes)
}

// ------------------------------------------------------------------------------------
// Harness.
// ------------------------------------------------------------------------------------

/// `gfx.mem $ draw`, unconfigured (the documented 640x480 default).
fn default_chain() -> Component {
    guest::ensure_components(&["eo9-stub-gfx-mem", "eo9-example-draw"]);
    compose(&guest::load_stub("gfx.mem"), &guest::load_example("draw"))
        .expect("gfx.mem $ draw must compose")
}

/// `configure(gfx.mem, WxH) $ draw`.
fn configured_chain(width: u32, height: u32) -> Component {
    guest::ensure_components(&["eo9-stub-gfx-mem", "eo9-example-draw"]);
    let mem = configure(
        &guest::load_stub("gfx.mem"),
        &[
            ("width", width.to_string().as_str()),
            ("height", height.to_string().as_str()),
        ],
    )
    .expect("configure(gfx.mem, --width … --height …) must bake");
    compose(&mem, &guest::load_example("draw")).expect("gfx.mem $ draw must compose")
}

fn run_draw(chain: &Component, args: &[NamedArg]) -> Outcome {
    run::run_component(chain, args, Providers::none())
}

fn assert_presented(outcome: &Outcome, expected: u64) {
    match outcome {
        Outcome::Success(success) => {
            let rendered = &success.value;
            assert!(
                rendered.contains(&expected.to_string()),
                "expected presented({expected}), got {rendered}"
            );
        }
        other => panic!("expected the program's success, got {other:?}"),
    }
}

// ------------------------------------------------------------------------------------
// Tests.
// ------------------------------------------------------------------------------------

#[test]
fn the_configured_framebuffer_round_trips_the_pattern() {
    let chain = configured_chain(320, 200);
    let outcome = run_draw(&chain, &[]);
    assert_presented(&outcome, expected_checksum(1, 320, 200));
}

#[test]
fn an_unconfigured_gfx_mem_defaults_to_640x480_and_never_traps() {
    let chain = default_chain();
    let outcome = run_draw(&chain, &[]);
    assert_presented(&outcome, expected_checksum(1, 640, 480));
}

/// FNV-1a-64 of draw's frame-1 pattern at the Orange Pi 5 Plus board geometry, as
/// tightly packed xrgb8888 (X = 0). The SAME literal is pinned in the kernel's
/// gfx.simplefb core tests (kernel/eo9-kernel/src/gfxfb.rs), where the RGB888 packing
/// must reproduce it — the cross-backend checksum identity. If either side's copy of
/// the pattern or packing drifts, its own pin fails; on the board, `draw` (granted
/// `gfx`) reporting `presented(…)` with this number is the M3 acceptance.
const PATTERN_800X480_FRAME1_FNV: u64 = 0xd66b_49ee_575f_f0d9;

#[test]
fn the_board_geometry_checksum_is_pinned_for_gfx_simplefb() {
    assert_eq!(
        expected_checksum(1, 800, 480),
        PATTERN_800X480_FRAME1_FNV,
        "the local pattern copy drifted from the pinned board-geometry checksum"
    );
    let chain = configured_chain(800, 480);
    let outcome = run_draw(&chain, &[]);
    assert_presented(&outcome, PATTERN_800X480_FRAME1_FNV);
}

#[test]
fn a_second_frame_presents_only_the_damage_rectangle() {
    let chain = configured_chain(320, 200);
    let outcome = run_draw(&chain, &[NamedArg::new("frames", "2")]);
    // The expectation composites the inverted centered rectangle over frame 1: if the
    // provider mishandled the partial present (wrong offset, wrong stride, full-frame
    // overwrite with damage-only bytes), the checksum cannot match.
    assert_presented(&outcome, expected_checksum(2, 320, 200));
}

#[test]
fn an_out_of_bounds_present_answers_the_typed_error() {
    let chain = configured_chain(64, 64);
    let outcome = run_draw(&chain, &[NamedArg::new("check", "\"oob-rect\"")]);
    match outcome {
        Outcome::Success(success) => assert!(
            success.value.contains("out-of-bounds"),
            "expected probed(out-of-bounds), got {}",
            success.value
        ),
        other => panic!("expected the probe's success, got {other:?}"),
    }
}

#[test]
fn an_undersized_buffer_answers_the_typed_error() {
    let chain = configured_chain(64, 64);
    let outcome = run_draw(&chain, &[NamedArg::new("check", "\"short-buffer\"")]);
    match outcome {
        Outcome::Success(success) => assert!(
            success.value.contains("bad-buffer"),
            "expected probed(bad-buffer), got {}",
            success.value
        ),
        other => panic!("expected the probe's success, got {other:?}"),
    }
}

#[test]
fn gfx_deny_seals_the_import_and_the_program_reports_denied() {
    guest::ensure_components(&["eo9-stub-gfx-deny", "eo9-example-draw"]);
    let sealed = compose(&guest::load_stub("gfx.deny"), &guest::load_example("draw"))
        .expect("gfx.deny $ draw must compose");

    let info = sealed.describe();
    let residual: Vec<&str> = info.imports.iter().map(|i| i.interface.as_str()).collect();
    assert!(
        !residual.iter().any(|i| i.starts_with("eo9:gfx/")),
        "gfx.deny must seal the gfx import (no residual gfx requirement): {residual:?}"
    );

    // `draw`'s very first gfx call is its wake-up `clear` (see the example: device-backed
    // providers bring up on the first awaited operation), which gfx.deny answers with the
    // API's own `denied` — the program reports it in its own vocabulary; never a trap,
    // never a loader error.
    let outcome = run_draw(&sealed, &[]);
    match outcome {
        Outcome::Failure(failure) => assert!(
            failure.value.to_lowercase().contains("denied"),
            "expected the program's gfx(denied) failure, got {}",
            failure.value
        ),
        other => panic!("expected the program's typed failure, got {other:?}"),
    }
}
