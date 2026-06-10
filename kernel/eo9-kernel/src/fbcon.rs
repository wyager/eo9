//! fbcon — the kernel console tee onto the board's HDMI framebuffer (the demo's
//! "shell visible on HDMI" leg; docs/board/usb-boot-demo-plan.md Part B).
//!
//! Two halves, split exactly like `gfxfb`/`simplefb`:
//!
//! * **The blitter core** (everything up to the `tee` module): a 100×30 character grid
//!   over the 800×480 packed-RGB888 surface, 8×16 glyphs, white-on-black. Pure math —
//!   no hardware, no alloc — writing through the tiny [`Surface`] trait, so the unit
//!   tests drive it against a RAM model on the host triple and pin checksums (the repo's
//!   FNV-1a-64 convention). White-on-black is deliberate: r=g=b is the invariant of the
//!   board's HDMI chroma mangling (docs/board/hdmi-simplefb-plan.md), so the console
//!   renders faithfully through the broken link.
//!
//! * **The kernel tee** (the `tee` module, board profile only): hooks the single console
//!   TX chokepoint (`uart::put_byte` — every `kprintln!`, shell echo and panic report
//!   funnels through it) and feeds each byte to the grid when fbcon is active. Output
//!   policy is tee, not switch: serial transcripts remain the bench instrument.
//!
//! **Safety in all printing contexts.** The console itself is lock-free (uart.rs: every
//! write goes straight to the MMIO registers); fbcon must not change that. Tee bytes
//! flow through a lock-free SPSC ring (the `rxring` discipline, mirrored): `tee_byte`
//! always enqueues — a print re-entering mid-tee (an exception handler or an IRQ-context
//! `kprintln!` landing while a render is in progress on the single boot core) parks its
//! bytes in the ring instead of losing them, and the renderer behind the BUSY try-enter
//! guard drains the ring when the interrupted render resumes. The only remaining drops
//! are a full ring and the few-instruction producer window, both COUNTED and surfaced
//! by a rate-limited `fbcon: N tee bytes dropped` note (the no-silent-loss doctrine);
//! nothing ever blocks or deadlocks. The panic path clears the stale guards first
//! (`panic_reset` — the pre-empted renderer never resumes), so the report reaches HDMI
//! even when the panic struck mid-render.
//!
//! **Cost discipline.** Inactive (token absent): one relaxed load per byte on board
//! builds, nothing at all elsewhere (the module compiles out of non-board kernels except
//! for host tests). Active: bounded work per byte — at most two cell paints (glyph +
//! cursor, ≤ 768 framebuffer bytes) except on a line-feed past the last row, which
//! scrolls by repainting the grid from the 3000-byte shadow text (writes only; reading
//! Device-nGnRnE memory back for a memmove would double the cost), and on erase-to-EOL
//! (CSI K), which black-fills at most one row of pixels.
//!
//! **The CSI subset.** fbcon renders exactly what the repo's console emitters emit
//! (census 2026-06-09: eosh-inc's editor emits SGR 31/0 — the M3 red marking — plus
//! `\b \b`, `\r\n` and BEL; the kernel's read-line echo emits only `\b \b`; nothing
//! emits cursor moves):
//!
//! * **SGR** (`ESC [ … m`): parameter 31 sets the red pen, 0 (or no parameter) resets
//!   to white; all other parameters are ignored. Red breaks the grayscale (r=g=b)
//!   chroma-mangling immunity above — under the boot-state-dependent link mismatch the
//!   mark renders wrong-hued but still *distinct*, which is the marking's whole job;
//!   `Marker::INVERSE` (SGR 7/27) remains the colorspace-proof follow-up.
//! * **CSI K** (parameter 0 or none): erase from the cursor to the end of the line —
//!   the repaint primitive the wrap-aware editor lane (area/40) coordinates on.
//!
//! Everything else ESC-led is consumed and ignored as before (CSI through its final
//! byte — unknown finals, malformed parameter bytes and parameter overflows consume the
//! whole sequence without effect — and two-byte ESC-x otherwise), never rendered.

use crate::gfxfb;

// -------------------------------------------------------------------------------------
// Font: 8×8 public-domain bitmaps, doubled vertically to the 8×16 cell
// -------------------------------------------------------------------------------------
//
// Provenance: `font8x8_basic` from Daniel Hepper's font8x8 collection
// (https://github.com/dhepper/font8x8, "License: Public Domain"), itself based on
// Marcel Sondaar's mode13h font and IBM's public-domain VGA fonts. Glyphs 0x20..=0x7E
// copied verbatim (95 × 8 bytes); each byte is one row, LSB = leftmost pixel. The 8×16
// cell is the 8×8 glyph with every row doubled — chunky but unambiguous at 800×480.

/// ASCII 0x20..=0x7E, one 8-byte bitmap per glyph (row-major, LSB = leftmost pixel).
const FONT: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x20 space
    [0x18, 0x3c, 0x3c, 0x18, 0x18, 0x00, 0x18, 0x00], // 0x21 !
    [0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x22 "
    [0x36, 0x36, 0x7f, 0x36, 0x7f, 0x36, 0x36, 0x00], // 0x23 #
    [0x0c, 0x3e, 0x03, 0x1e, 0x30, 0x1f, 0x0c, 0x00], // 0x24 $
    [0x00, 0x63, 0x33, 0x18, 0x0c, 0x66, 0x63, 0x00], // 0x25 %
    [0x1c, 0x36, 0x1c, 0x6e, 0x3b, 0x33, 0x6e, 0x00], // 0x26 &
    [0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x27 apostrophe
    [0x18, 0x0c, 0x06, 0x06, 0x06, 0x0c, 0x18, 0x00], // 0x28 (
    [0x06, 0x0c, 0x18, 0x18, 0x18, 0x0c, 0x06, 0x00], // 0x29 )
    [0x00, 0x66, 0x3c, 0xff, 0x3c, 0x66, 0x00, 0x00], // 0x2a *
    [0x00, 0x0c, 0x0c, 0x3f, 0x0c, 0x0c, 0x00, 0x00], // 0x2b +
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x06], // 0x2c ,
    [0x00, 0x00, 0x00, 0x3f, 0x00, 0x00, 0x00, 0x00], // 0x2d -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x00], // 0x2e .
    [0x60, 0x30, 0x18, 0x0c, 0x06, 0x03, 0x01, 0x00], // 0x2f /
    [0x3e, 0x63, 0x73, 0x7b, 0x6f, 0x67, 0x3e, 0x00], // 0x30 0
    [0x0c, 0x0e, 0x0c, 0x0c, 0x0c, 0x0c, 0x3f, 0x00], // 0x31 1
    [0x1e, 0x33, 0x30, 0x1c, 0x06, 0x33, 0x3f, 0x00], // 0x32 2
    [0x1e, 0x33, 0x30, 0x1c, 0x30, 0x33, 0x1e, 0x00], // 0x33 3
    [0x38, 0x3c, 0x36, 0x33, 0x7f, 0x30, 0x78, 0x00], // 0x34 4
    [0x3f, 0x03, 0x1f, 0x30, 0x30, 0x33, 0x1e, 0x00], // 0x35 5
    [0x1c, 0x06, 0x03, 0x1f, 0x33, 0x33, 0x1e, 0x00], // 0x36 6
    [0x3f, 0x33, 0x30, 0x18, 0x0c, 0x0c, 0x0c, 0x00], // 0x37 7
    [0x1e, 0x33, 0x33, 0x1e, 0x33, 0x33, 0x1e, 0x00], // 0x38 8
    [0x1e, 0x33, 0x33, 0x3e, 0x30, 0x18, 0x0e, 0x00], // 0x39 9
    [0x00, 0x0c, 0x0c, 0x00, 0x00, 0x0c, 0x0c, 0x00], // 0x3a :
    [0x00, 0x0c, 0x0c, 0x00, 0x00, 0x0c, 0x0c, 0x06], // 0x3b ;
    [0x18, 0x0c, 0x06, 0x03, 0x06, 0x0c, 0x18, 0x00], // 0x3c <
    [0x00, 0x00, 0x3f, 0x00, 0x00, 0x3f, 0x00, 0x00], // 0x3d =
    [0x06, 0x0c, 0x18, 0x30, 0x18, 0x0c, 0x06, 0x00], // 0x3e >
    [0x1e, 0x33, 0x30, 0x18, 0x0c, 0x00, 0x0c, 0x00], // 0x3f ?
    [0x3e, 0x63, 0x7b, 0x7b, 0x7b, 0x03, 0x1e, 0x00], // 0x40 @
    [0x0c, 0x1e, 0x33, 0x33, 0x3f, 0x33, 0x33, 0x00], // 0x41 A
    [0x3f, 0x66, 0x66, 0x3e, 0x66, 0x66, 0x3f, 0x00], // 0x42 B
    [0x3c, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3c, 0x00], // 0x43 C
    [0x1f, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1f, 0x00], // 0x44 D
    [0x7f, 0x46, 0x16, 0x1e, 0x16, 0x46, 0x7f, 0x00], // 0x45 E
    [0x7f, 0x46, 0x16, 0x1e, 0x16, 0x06, 0x0f, 0x00], // 0x46 F
    [0x3c, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7c, 0x00], // 0x47 G
    [0x33, 0x33, 0x33, 0x3f, 0x33, 0x33, 0x33, 0x00], // 0x48 H
    [0x1e, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x1e, 0x00], // 0x49 I
    [0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1e, 0x00], // 0x4a J
    [0x67, 0x66, 0x36, 0x1e, 0x36, 0x66, 0x67, 0x00], // 0x4b K
    [0x0f, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7f, 0x00], // 0x4c L
    [0x63, 0x77, 0x7f, 0x7f, 0x6b, 0x63, 0x63, 0x00], // 0x4d M
    [0x63, 0x67, 0x6f, 0x7b, 0x73, 0x63, 0x63, 0x00], // 0x4e N
    [0x1c, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1c, 0x00], // 0x4f O
    [0x3f, 0x66, 0x66, 0x3e, 0x06, 0x06, 0x0f, 0x00], // 0x50 P
    [0x1e, 0x33, 0x33, 0x33, 0x3b, 0x1e, 0x38, 0x00], // 0x51 Q
    [0x3f, 0x66, 0x66, 0x3e, 0x36, 0x66, 0x67, 0x00], // 0x52 R
    [0x1e, 0x33, 0x07, 0x0e, 0x38, 0x33, 0x1e, 0x00], // 0x53 S
    [0x3f, 0x2d, 0x0c, 0x0c, 0x0c, 0x0c, 0x1e, 0x00], // 0x54 T
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3f, 0x00], // 0x55 U
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x1e, 0x0c, 0x00], // 0x56 V
    [0x63, 0x63, 0x63, 0x6b, 0x7f, 0x77, 0x63, 0x00], // 0x57 W
    [0x63, 0x63, 0x36, 0x1c, 0x1c, 0x36, 0x63, 0x00], // 0x58 X
    [0x33, 0x33, 0x33, 0x1e, 0x0c, 0x0c, 0x1e, 0x00], // 0x59 Y
    [0x7f, 0x63, 0x31, 0x18, 0x4c, 0x66, 0x7f, 0x00], // 0x5a Z
    [0x1e, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1e, 0x00], // 0x5b [
    [0x03, 0x06, 0x0c, 0x18, 0x30, 0x60, 0x40, 0x00], // 0x5c backslash
    [0x1e, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1e, 0x00], // 0x5d ]
    [0x08, 0x1c, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00], // 0x5e ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff], // 0x5f _
    [0x0c, 0x0c, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x60 `
    [0x00, 0x00, 0x1e, 0x30, 0x3e, 0x33, 0x6e, 0x00], // 0x61 a
    [0x07, 0x06, 0x06, 0x3e, 0x66, 0x66, 0x3b, 0x00], // 0x62 b
    [0x00, 0x00, 0x1e, 0x33, 0x03, 0x33, 0x1e, 0x00], // 0x63 c
    [0x38, 0x30, 0x30, 0x3e, 0x33, 0x33, 0x6e, 0x00], // 0x64 d
    [0x00, 0x00, 0x1e, 0x33, 0x3f, 0x03, 0x1e, 0x00], // 0x65 e
    [0x1c, 0x36, 0x06, 0x0f, 0x06, 0x06, 0x0f, 0x00], // 0x66 f
    [0x00, 0x00, 0x6e, 0x33, 0x33, 0x3e, 0x30, 0x1f], // 0x67 g
    [0x07, 0x06, 0x36, 0x6e, 0x66, 0x66, 0x67, 0x00], // 0x68 h
    [0x0c, 0x00, 0x0e, 0x0c, 0x0c, 0x0c, 0x1e, 0x00], // 0x69 i
    [0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1e], // 0x6a j
    [0x07, 0x06, 0x66, 0x36, 0x1e, 0x36, 0x67, 0x00], // 0x6b k
    [0x0e, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x1e, 0x00], // 0x6c l
    [0x00, 0x00, 0x33, 0x7f, 0x7f, 0x6b, 0x63, 0x00], // 0x6d m
    [0x00, 0x00, 0x1f, 0x33, 0x33, 0x33, 0x33, 0x00], // 0x6e n
    [0x00, 0x00, 0x1e, 0x33, 0x33, 0x33, 0x1e, 0x00], // 0x6f o
    [0x00, 0x00, 0x3b, 0x66, 0x66, 0x3e, 0x06, 0x0f], // 0x70 p
    [0x00, 0x00, 0x6e, 0x33, 0x33, 0x3e, 0x30, 0x78], // 0x71 q
    [0x00, 0x00, 0x3b, 0x6e, 0x66, 0x06, 0x0f, 0x00], // 0x72 r
    [0x00, 0x00, 0x3e, 0x03, 0x1e, 0x30, 0x1f, 0x00], // 0x73 s
    [0x08, 0x0c, 0x3e, 0x0c, 0x0c, 0x2c, 0x18, 0x00], // 0x74 t
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6e, 0x00], // 0x75 u
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x1e, 0x0c, 0x00], // 0x76 v
    [0x00, 0x00, 0x63, 0x6b, 0x7f, 0x7f, 0x36, 0x00], // 0x77 w
    [0x00, 0x00, 0x63, 0x36, 0x1c, 0x36, 0x63, 0x00], // 0x78 x
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x3e, 0x30, 0x1f], // 0x79 y
    [0x00, 0x00, 0x3f, 0x19, 0x0c, 0x26, 0x3f, 0x00], // 0x7a z
    [0x38, 0x0c, 0x0c, 0x07, 0x0c, 0x0c, 0x38, 0x00], // 0x7b {
    [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00], // 0x7c |
    [0x07, 0x0c, 0x0c, 0x38, 0x0c, 0x0c, 0x07, 0x00], // 0x7d }
    [0x6e, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0x7e ~
];

/// The replacement glyph for bytes outside printable ASCII: a hollow box.
const BOX_GLYPH: [u8; 8] = [0x00, 0x3f, 0x21, 0x21, 0x21, 0x21, 0x3f, 0x00];

/// The 8×8 bitmap for a shadow-text cell. `0` is the empty (never-written) cell and
/// renders as space; printable ASCII renders its glyph; everything else the box.
fn glyph(cell: u8) -> &'static [u8; 8] {
    match cell {
        0 => &FONT[0],
        0x20..=0x7e => &FONT[(cell - 0x20) as usize],
        _ => &BOX_GLYPH,
    }
}

/// Cell attribute: the red pen (SGR 31) is set, white otherwise. Stored per cell in the
/// shadow attribute plane so scroll and redraw reproduce the marking.
const ATTR_RED: u8 = 1;

/// Foreground bytes for an attribute, in the framebuffer's B, G, R memory order
/// (gfxfb's little-endian DRM `RGB888` convention). White is the default; red is the
/// M3 inadmissible-region mark (SGR 31).
fn fg(attr: u8) -> [u8; 3] {
    if attr & ATTR_RED != 0 {
        [0x00, 0x00, 0xff]
    } else {
        [0xff; 3]
    }
}

// -------------------------------------------------------------------------------------
// The blitter core
// -------------------------------------------------------------------------------------

/// Character columns (100 × 8 px = the full 800-px width).
pub const COLS: usize = 100;
/// Character rows (30 × 16 px = the full 480-px height).
pub const ROWS: usize = 30;
/// Cell width in pixels (the font's native width).
const CELL_W: usize = 8;
/// Cell height in pixels (each font row doubled).
const CELL_H: usize = 16;
/// Framebuffer bytes per cell pixel-row (8 px × 3 bytes).
const CELL_ROW_BYTES: usize = CELL_W * gfxfb::FB_BPP;

// The grid tiles the verified surface exactly; a geometry drift fails the build here.
const _: () = assert!(COLS * CELL_W == gfxfb::WIDTH as usize);
const _: () = assert!(ROWS * CELL_H == gfxfb::HEIGHT as usize);
const _: () = assert!(COLS * CELL_ROW_BYTES == gfxfb::STRIDE);

/// Where the grid's pixels go: a write-only view of the 800×480 RGB888 surface, byte
/// offsets from its base. The host tests implement it over a RAM model; the kernel tee
/// implements it over `simplefb`'s Device-mapping-safe stores. Write-only on purpose —
/// scrolling repaints from the shadow text instead of reading device memory back.
pub trait Surface {
    /// Copy `bytes` to surface offset `offset` (`offset + bytes.len() <= FB_BYTES`,
    /// guaranteed by the grid's geometry — the compile-time assertions above).
    fn write(&mut self, offset: usize, bytes: &[u8]);
}

/// ANSI/CSI parsing state (the supported subset renders; everything else is consumed
/// so a colored or cursor-moving print can never corrupt the grid).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    /// Ordinary bytes.
    Ground,
    /// An ESC (0x1B) has been seen; deciding the sequence shape.
    Escape,
    /// Inside `ESC [` … ; collecting parameters until the final byte (0x40..=0x7E).
    Csi,
}

/// How many numeric CSI parameters are kept; longer sequences are consumed whole and
/// ignored (nothing in the repo emits more than two — the census in the module docs).
const CSI_MAX_PARAMS: usize = 4;

/// Parameter accumulator for one CSI sequence.
#[derive(Clone, Copy)]
struct CsiState {
    /// Parameter values, `params[..=n]` meaningful once `any` is set (empty slots are
    /// 0, matching the ANSI default-parameter rule).
    params: [u16; CSI_MAX_PARAMS],
    /// The slot currently accumulating.
    n: usize,
    /// Whether any parameter byte (digit or `;`) arrived — distinguishes `ESC[m`
    /// (zero parameters) from `ESC[0m` for dispatchers that care about the count.
    any: bool,
    /// Set on malformed input (non-numeric parameter bytes, intermediates, parameter
    /// overflow): the sequence is still consumed to its final byte, then ignored.
    poisoned: bool,
}

impl CsiState {
    const fn new() -> CsiState {
        CsiState {
            params: [0; CSI_MAX_PARAMS],
            n: 0,
            any: false,
            poisoned: false,
        }
    }
}

/// The console grid: shadow text + attribute planes (what each cell holds), cursor,
/// pen, escape-parse state. The framebuffer is write-only downstream of this struct —
/// every repaint renders from the shadow, so scroll never reads device memory.
#[derive(Clone)]
pub struct Grid {
    /// Row-major cell contents; `0` = never written (renders as space).
    text: [u8; COLS * ROWS],
    /// Row-major cell attributes ([`ATTR_RED`]); scroll and redraw carry them with the
    /// text so the M3 marking survives both.
    attrs: [u8; COLS * ROWS],
    /// Cursor column, `0..COLS`.
    col: usize,
    /// Cursor row, `0..ROWS`.
    row: usize,
    /// The current pen: attributes stamped on every glyph printed (SGR sets/resets it).
    pen: u8,
    /// Escape-sequence parsing state.
    esc: Esc,
    /// CSI parameter accumulator (meaningful while `esc == Esc::Csi`).
    csi: CsiState,
}

impl Grid {
    /// An all-empty grid, cursor at the origin. `const` so the kernel's static lives in
    /// `.bss` (the shadow text's empty cell is 0, not `b' '`, for exactly this reason).
    pub const fn new() -> Grid {
        Grid {
            text: [0; COLS * ROWS],
            attrs: [0; COLS * ROWS],
            col: 0,
            row: 0,
            pen: 0,
            esc: Esc::Ground,
            csi: CsiState::new(),
        }
    }

    /// Paint one cell: the glyph for `cell` in its attribute's foreground on black, or
    /// inverted (the cursor block — background pixels light up, glyph pixels go black).
    /// The inverted block is always white: the cursor's color is the console's, not the
    /// text's (a red block would otherwise appear over invisible red-attributed spaces,
    /// e.g. the cells the editor's BS-space-BS erase writes inside a marked region).
    /// 16 row writes of 24 bytes.
    fn paint_cell(
        &self,
        s: &mut impl Surface,
        row: usize,
        col: usize,
        cell: u8,
        attr: u8,
        inverted: bool,
    ) {
        let bitmap = glyph(cell);
        let color = fg(if inverted { 0 } else { attr });
        let base = row * CELL_H * gfxfb::STRIDE + col * CELL_ROW_BYTES;
        for py in 0..CELL_H {
            // Each 8×8 font row covers two pixel rows (the vertical doubling).
            let bits = bitmap[py / 2];
            let mut out = [0u8; CELL_ROW_BYTES];
            for px in 0..CELL_W {
                // LSB = leftmost pixel (the font8x8 convention).
                if ((bits >> px) & 1 != 0) != inverted {
                    out[px * gfxfb::FB_BPP..(px + 1) * gfxfb::FB_BPP].copy_from_slice(&color);
                }
            }
            s.write(base + py * gfxfb::STRIDE, &out);
        }
    }

    /// Repaint every cell from the shadow planes (normal video), one full pixel row per
    /// write. On an empty grid this is the screen clear.
    fn repaint(&self, s: &mut impl Surface) {
        for trow in 0..ROWS {
            for py in 0..CELL_H {
                let mut out = [0u8; gfxfb::STRIDE];
                for tcol in 0..COLS {
                    let bits = glyph(self.text[trow * COLS + tcol])[py / 2];
                    let color = fg(self.attrs[trow * COLS + tcol]);
                    for px in 0..CELL_W {
                        if (bits >> px) & 1 != 0 {
                            let at = tcol * CELL_ROW_BYTES + px * gfxfb::FB_BPP;
                            out[at..at + gfxfb::FB_BPP].copy_from_slice(&color);
                        }
                    }
                }
                s.write((trow * CELL_H + py) * gfxfb::STRIDE, &out);
            }
        }
    }

    /// Paint the cursor: the cell under it in inverted video (a solid block on an empty
    /// cell — the live-typing feel the demo wants).
    fn cursor(&self, s: &mut impl Surface) {
        let at = self.row * COLS + self.col;
        self.paint_cell(s, self.row, self.col, self.text[at], self.attrs[at], true);
    }

    /// Un-paint the cursor (repaint its cell in normal video).
    fn uncursor(&self, s: &mut impl Surface) {
        let at = self.row * COLS + self.col;
        self.paint_cell(s, self.row, self.col, self.text[at], self.attrs[at], false);
    }

    /// Full redraw: every cell from the shadow plus the cursor. Activation paints the
    /// initial (black) screen with this; the tests use it to pin the invariant that
    /// incremental painting always equals a fresh redraw of the same shadow state.
    pub fn redraw(&self, s: &mut impl Surface) {
        self.repaint(s);
        self.cursor(s);
    }

    /// Advance to the next row; on the last row, scroll by one (shadow memmove + full
    /// repaint — the framebuffer is never read). Attributes travel with their text.
    fn line_feed(&mut self, s: &mut impl Surface) {
        if self.row + 1 < ROWS {
            self.row += 1;
        } else {
            self.text.copy_within(COLS.., 0);
            self.text[(ROWS - 1) * COLS..].fill(0);
            self.attrs.copy_within(COLS.., 0);
            self.attrs[(ROWS - 1) * COLS..].fill(0);
            self.repaint(s);
        }
    }

    /// CSI K (mode 0): clear the shadow from the cursor to the end of the row, black-fill
    /// those pixels (one write per pixel row — bounded by a single row), repaint the
    /// cursor. The erased cells lose their attributes too: erase produces empty cells,
    /// not red-tinted ones.
    fn erase_to_eol(&mut self, s: &mut impl Surface) {
        let start = self.row * COLS + self.col;
        let end = (self.row + 1) * COLS;
        self.text[start..end].fill(0);
        self.attrs[start..end].fill(0);
        let bytes = (COLS - self.col) * CELL_ROW_BYTES;
        let base = self.row * CELL_H * gfxfb::STRIDE + self.col * CELL_ROW_BYTES;
        let zeros = [0u8; gfxfb::STRIDE];
        for py in 0..CELL_H {
            s.write(base + py * gfxfb::STRIDE, &zeros[..bytes]);
        }
        self.cursor(s);
    }

    /// Act on a completed, well-formed CSI sequence. The rendered subset is exactly the
    /// console's emission census (module docs): SGR 0/31 and CSI K mode 0. Every other
    /// final (cursor moves, clears, unknown SGR parameters) is ignored.
    fn csi_dispatch(&mut self, final_byte: u8, s: &mut impl Surface) {
        let count = if self.csi.any { self.csi.n + 1 } else { 0 };
        match final_byte {
            b'm' => {
                // SGR. No parameters = reset (the ANSI default-0 rule).
                if count == 0 {
                    self.pen = 0;
                }
                for &param in &self.csi.params[..count] {
                    match param {
                        0 => self.pen = 0,
                        31 => self.pen |= ATTR_RED,
                        _ => {}
                    }
                }
            }
            b'K' => {
                // EL: only mode 0 (cursor to end of line) is emitted/rendered.
                if count == 0 || self.csi.params[0] == 0 {
                    self.erase_to_eol(s);
                }
            }
            _ => {}
        }
    }

    /// Feed one console byte: parse escapes (rendering the CSI subset, consuming the
    /// rest), handle LF/CR/BS/TAB, render printables in the current pen (anything else
    /// printable-positioned renders the box glyph), keep the cursor painted. Bounded
    /// work: at most two cell paints, plus the repaint on a scroll and the row fill on
    /// an erase.
    pub fn feed(&mut self, byte: u8, s: &mut impl Surface) {
        match self.esc {
            Esc::Escape => {
                // ESC [ opens a CSI; ESC ESC stays armed; any other byte completes a
                // two-byte sequence (consumed).
                self.esc = match byte {
                    b'[' => {
                        self.csi = CsiState::new();
                        Esc::Csi
                    }
                    0x1b => Esc::Escape,
                    _ => Esc::Ground,
                };
                return;
            }
            Esc::Csi => match byte {
                b'0'..=b'9' => {
                    let slot = &mut self.csi.params[self.csi.n];
                    *slot = slot
                        .saturating_mul(10)
                        .saturating_add((byte - b'0') as u16);
                    self.csi.any = true;
                    return;
                }
                b';' => {
                    if self.csi.n + 1 < CSI_MAX_PARAMS {
                        self.csi.n += 1;
                    } else {
                        // Too many parameters: consume the rest, ignore the sequence.
                        self.csi.poisoned = true;
                    }
                    self.csi.any = true;
                    return;
                }
                // Other parameter/intermediate bytes (`?`, `<`, SP..`/`, …): nothing we
                // render uses them — consume to the final, then ignore.
                0x20..=0x3f => {
                    self.csi.poisoned = true;
                    return;
                }
                // The final byte ends the sequence; well-formed ones dispatch.
                0x40..=0x7e => {
                    self.esc = Esc::Ground;
                    if !self.csi.poisoned {
                        self.csi_dispatch(byte, s);
                    }
                    return;
                }
                // A control byte inside a CSI: abort the sequence and process it
                // normally (eating a stray LF would lose a line; malformed input,
                // defensive).
                _ => self.esc = Esc::Ground,
            },
            Esc::Ground => {}
        }
        match byte {
            0x1b => self.esc = Esc::Escape,
            b'\n' => {
                // The kernel's line convention is bare `\n` (kprintln); treat it as
                // CR+LF so serial-destined text lays out identically on the grid.
                self.uncursor(s);
                self.col = 0;
                self.line_feed(s);
                self.cursor(s);
            }
            b'\r' => {
                self.uncursor(s);
                self.col = 0;
                self.cursor(s);
            }
            0x08 => {
                // Backspace moves the cursor; at column 0 it is a no-op.
                if self.col > 0 {
                    self.uncursor(s);
                    self.col -= 1;
                    self.cursor(s);
                }
            }
            b'\t' => {
                // Next 8-column tab stop, clamped to the last column (no wrap).
                self.uncursor(s);
                self.col = ((self.col / 8 + 1) * 8).min(COLS - 1);
                self.cursor(s);
            }
            // Other C0 controls (BEL, VT, …): ignored, not boxed.
            0x00..=0x1f => {}
            // Printable ASCII renders its glyph in the current pen; 0x7F and the high
            // half render the box (the shadow keeps the raw byte so a repaint
            // reproduces the box).
            _ => {
                let at = self.row * COLS + self.col;
                self.text[at] = byte;
                self.attrs[at] = self.pen;
                // Painting the glyph overwrites the inverted cursor block in place.
                self.paint_cell(s, self.row, self.col, byte, self.pen, false);
                self.col += 1;
                if self.col == COLS {
                    self.col = 0;
                    self.line_feed(s);
                }
                self.cursor(s);
            }
        }
    }
}

// -------------------------------------------------------------------------------------
// The tee ring (host-tested; used by the kernel tee below)
// -------------------------------------------------------------------------------------

/// The tee ring: a lock-free single-producer/single-consumer byte ring between
/// `tee_byte` (producer — any printing context, including IRQ/exception handlers) and
/// the renderer drain behind the BUSY guard (consumer). Mirrors `crate::rxring`'s
/// head/tail acquire-release discipline (mirrored, not reused: rxring's producer side
/// notes the executor input edge — the wrong side effect for console *output* — and its
/// capacity is the paste-line bound, not a render backlog bound).
///
/// Capacity: the long renderer windows are a scroll repaint (~1.1 MB of device writes)
/// and an erase row-fill; the bytes arriving meanwhile are IRQ-context `kprintln!`
/// lines, a few hundred bytes at worst. 1 KiB parks several such lines; beyond that the
/// producer drops (counted, surfaced — never silent).
#[cfg(any(
    test,
    all(target_os = "none", target_arch = "aarch64", feature = "board-opi5plus")
))]
mod ring {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Ring capacity (power of two; one slot stays empty to distinguish full from
    /// empty).
    pub(super) const TEE_RING_CAP: usize = 1024;

    /// The SPSC byte ring. Producer serialization (one pusher at a time) and consumer
    /// serialization (one popper at a time) are the *caller's* contract — in the tee
    /// they are the PUSH_BUSY and BUSY try-enter guards respectively.
    pub(super) struct TeeRing {
        buf: UnsafeCell<[u8; TEE_RING_CAP]>,
        /// Next index the producer will write.
        head: AtomicUsize,
        /// Next index the consumer will read.
        tail: AtomicUsize,
    }

    // SAFETY: access is coordinated through `head`/`tail` with acquire/release ordering
    // under the caller's single-producer/single-consumer contract.
    unsafe impl Sync for TeeRing {}

    impl TeeRing {
        pub(super) const fn new() -> TeeRing {
            TeeRing {
                buf: UnsafeCell::new([0; TEE_RING_CAP]),
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
            }
        }

        /// Publish `byte`, or return `false` when the ring is full (the caller counts
        /// the drop — never overwrite unrendered bytes).
        pub(super) fn push(&self, byte: u8) -> bool {
            let head = self.head.load(Ordering::Relaxed);
            let next = (head + 1) % TEE_RING_CAP;
            if next == self.tail.load(Ordering::Acquire) {
                return false;
            }
            // SAFETY: the caller is the sole producer; this slot is at `head`, ahead of
            // the consumer's `tail`.
            unsafe { (*self.buf.get())[head] = byte };
            self.head.store(next, Ordering::Release);
            true
        }

        /// Take one byte, or `None` when the ring is empty.
        pub(super) fn pop(&self) -> Option<u8> {
            let tail = self.tail.load(Ordering::Relaxed);
            if tail == self.head.load(Ordering::Acquire) {
                return None;
            }
            // SAFETY: the caller is the sole consumer; this slot was published by the
            // producer (head moved past it with release ordering, observed by the
            // acquire load above).
            let byte = unsafe { (*self.buf.get())[tail] };
            self.tail.store((tail + 1) % TEE_RING_CAP, Ordering::Release);
            Some(byte)
        }

        /// Whether nothing is waiting (the drain loop's exit re-check).
        pub(super) fn is_empty(&self) -> bool {
            self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Relaxed)
        }
    }
}

// -------------------------------------------------------------------------------------
// The kernel tee (board profile only)
// -------------------------------------------------------------------------------------

#[cfg(all(
    target_os = "none",
    target_arch = "aarch64",
    feature = "board-opi5plus"
))]
mod tee {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::ring::TeeRing;
    use super::{COLS, Grid, ROWS, Surface};
    use crate::gfxfb;

    /// The grid behind the tee. Sole mutator is the [`drain`] loop under the [`BUSY`]
    /// guard (plus [`activate`] before [`ACTIVE`] is ever true); single boot core.
    struct GridCell(UnsafeCell<Grid>);
    // SAFETY: every `&mut` borrow is serialized by the BUSY try-enter guard below (and
    // activation happens-before the first guarded borrow via the ACTIVE release store).
    unsafe impl Sync for GridCell {}

    static GRID: GridCell = GridCell(UnsafeCell::new(Grid::new()));
    /// Whether the tee is live (the `fbcon` token was accepted and the surface located).
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    /// Consumer try-enter guard: whoever wins it drains the ring into the grid; a print
    /// re-entering mid-render (exception/IRQ print on the boot core) loses the swap,
    /// parks its bytes in the ring and returns — the interrupted holder drains them
    /// when it resumes. Never blocks, never aliases the grid.
    static BUSY: AtomicBool = AtomicBool::new(false);
    /// Producer try-enter guard: serializes ring pushes against a print re-entering in
    /// the few-instruction push window itself (single core — the inner pusher cannot
    /// wait for the interrupted one). The loser drops its byte, COUNTED in [`DROPPED`].
    static PUSH_BUSY: AtomicBool = AtomicBool::new(false);
    /// The ring between every printing context and the renderer ([`super::ring`]).
    static TEE_RING: TeeRing = TeeRing::new();
    /// Total tee bytes lost (ring full, or the producer-window collision above). Never
    /// silent: [`drain`] surfaces growth through the rate-limited note below.
    static DROPPED: AtomicUsize = AtomicUsize::new(0);
    /// The drop count already reported (rate limit state: report on first loss, then
    /// only when the count has doubled — a sustained overflow cannot spam the console).
    static NOTED: AtomicUsize = AtomicUsize::new(0);
    /// The located, map-checked surface base (set before `ACTIVE`).
    static FB_BASE: AtomicUsize = AtomicUsize::new(0);

    /// [`Surface`] over the located framebuffer, through the Device-mapping-safe
    /// single-register-width stores (`simplefb::copy_out`).
    struct FbSurface {
        base: usize,
    }

    impl Surface for FbSurface {
        fn write(&mut self, offset: usize, bytes: &[u8]) {
            debug_assert!(offset + bytes.len() <= gfxfb::FB_BYTES);
            // SAFETY: activation map-checked `[base, base + FB_BYTES)` (the locator's
            // device-window gate); the grid only emits in-surface offsets (compile-time
            // geometry assertions + the host test suite).
            unsafe { crate::simplefb::copy_out(self.base + offset, bytes) };
        }
    }

    /// Activate the console tee: locate the surface (evidence lines included), gate the
    /// geometry, clear the screen, then go live. On any refusal the kernel keeps running
    /// serial-only — fbcon is a demo leg, never a boot dependency. Called once from
    /// `kmain` when the `fbcon` boot token is present (and `gfx` is not — the scanout
    /// has one owner per boot; `kmain` enforces the exclusion).
    pub fn activate() {
        let Ok(located) = crate::simplefb::locate_and_report() else {
            crate::kprintln!("fbcon: unavailable (framebuffer locate failed; serial only)");
            return;
        };
        let (width, height) = gfxfb::act_geometry(located.act);
        if (width, height) != (gfxfb::WIDTH, gfxfb::HEIGHT) || located.vir != gfxfb::PROFILE_VIR {
            crate::kprintln!(
                "fbcon: unavailable (live scanout {width}x{height} vir={:#x} is not the \
                 supported {}x{} stride-{} surface)",
                located.vir,
                gfxfb::WIDTH,
                gfxfb::HEIGHT,
                gfxfb::STRIDE,
            );
            return;
        }
        FB_BASE.store(located.base, Ordering::Relaxed);
        let mut surface = FbSurface { base: located.base };
        // SAFETY: ACTIVE is still false, so tee_byte never touches the grid yet; the
        // single boot core is executing here.
        let grid = unsafe { &mut *GRID.0.get() };
        *grid = Grid::new();
        grid.redraw(&mut surface);
        ACTIVE.store(true, Ordering::Release);
        // From here every console byte tees — this line is the first on the monitor
        // (and the grep-stable activation marker on serial).
        crate::kprintln!("fbcon: active {COLS}x{ROWS}");
    }

    /// Serial-only print window: bytes printed while muted skip the framebuffer
    /// (serial carries them). Used for periodic chatter — the watchdog heartbeat —
    /// that would otherwise scroll the HDMI console forever.
    static MUTED: AtomicBool = AtomicBool::new(false);

    /// Mute or unmute the framebuffer tee around a serial-only print (single boot
    /// core: callers bracket one print, so no nesting bookkeeping is needed).
    pub fn set_tee_mute(muted: bool) {
        MUTED.store(muted, Ordering::Relaxed);
    }

    /// The per-byte tee, called from the console TX chokepoint (`uart::put_byte`).
    /// Inactive cost: one relaxed load. Never blocks: the byte is parked in the ring
    /// (always, except the two counted drop windows) and rendered by whoever holds —
    /// or now wins — the BUSY guard. Re-entrant prints (exception/IRQ printing while a
    /// render is in progress) therefore reach the framebuffer once the interrupted
    /// render resumes, instead of being silently lost.
    pub fn tee_byte(byte: u8) {
        if !ACTIVE.load(Ordering::Relaxed) || MUTED.load(Ordering::Relaxed) {
            return;
        }
        // Producer side: the try-enter guard serializes against a print that
        // interrupted another print inside this same window (single core — waiting
        // would deadlock). The loser's byte is dropped but counted.
        if PUSH_BUSY.swap(true, Ordering::Acquire) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if !TEE_RING.push(byte) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        PUSH_BUSY.store(false, Ordering::Release);
        drain();
    }

    /// Render everything waiting in the ring, if no one else already is. The loop
    /// re-checks after releasing BUSY: a byte parked between the last empty pop and
    /// the release would otherwise sit unrendered until the next print (a backstop
    /// rescuing event-path work is a bug — the event path delivers it here).
    fn drain() {
        loop {
            if BUSY.swap(true, Ordering::Acquire) {
                // The interrupted holder (below us on this core's stack) drains the
                // ring — including our bytes — when it resumes.
                return;
            }
            let mut surface = FbSurface {
                base: FB_BASE.load(Ordering::Relaxed),
            };
            // SAFETY: the BUSY try-enter guard makes this the only live `&mut Grid`
            // (single core; re-entrant prints bailed out above), and ACTIVE's release
            // store ordered activation's initialization before this borrow.
            let grid = unsafe { &mut *GRID.0.get() };
            while let Some(byte) = TEE_RING.pop() {
                grid.feed(byte, &mut surface);
            }
            note_drops();
            BUSY.store(false, Ordering::Release);
            if TEE_RING.is_empty() {
                return;
            }
        }
    }

    /// Surface tee-byte loss loudly but rate-limited (no-silent-loss doctrine): log on
    /// the first lost byte, then only when the total has at least doubled since the
    /// last note. Called while holding BUSY; the note's own bytes re-enter `tee_byte`,
    /// park in the ring (the BUSY swap fails for them) and are rendered by our caller's
    /// drain loop — bounded, no recursion.
    fn note_drops() {
        let dropped = DROPPED.load(Ordering::Relaxed);
        let noted = NOTED.load(Ordering::Relaxed);
        if dropped == noted {
            return;
        }
        if noted == 0 || dropped >= noted.saturating_mul(2) {
            NOTED.store(dropped, Ordering::Relaxed);
            crate::kprintln!(
                "fbcon: {dropped} console bytes dropped from the HDMI tee \
                 (ring overflow or re-entrant print; serial carried them)"
            );
        }
    }

    /// Panic-path reset: clear the try-enter guards so the panic report renders even
    /// when the panic struck mid-render or mid-push. Sound to force only because the
    /// pre-empted holder's frame never resumes — the panic handler does not return —
    /// so the guard it left set would otherwise mute HDMI for the whole report. The
    /// grid may be mid-sequence (a torn CSI parse garbles at most the next cells);
    /// `feed` itself stays memory-safe on any state.
    pub fn panic_reset() {
        PUSH_BUSY.store(false, Ordering::Release);
        BUSY.store(false, Ordering::Release);
    }
}

#[cfg(all(
    target_os = "none",
    target_arch = "aarch64",
    feature = "board-opi5plus"
))]
pub use tee::{activate, panic_reset, set_tee_mute, tee_byte};

#[cfg(not(all(
    target_os = "none",
    target_arch = "aarch64",
    feature = "board-opi5plus"
)))]
#[allow(dead_code)]
pub fn set_tee_mute(_muted: bool) {}

// -------------------------------------------------------------------------------------
// Host tests
// -------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a-64 of the frame after rendering [`KNOWN_LINES`] on a fresh grid. Pinned so
    /// glyph placement, doubling, cursor video or scroll math drifting fails here before
    /// it garbles a bench capture.
    const KNOWN_LINES_FNV: u64 = 0x4ff3_5cda_86ee_dfe9;

    /// FNV-1a-64 of the frame after the 35-line scroll test (5 lines past the bottom).
    const SCROLLED_FNV: u64 = 0x5780_a57c_9910_70f9;

    const KNOWN_LINES: &str =
        "Eo9 fbcon\neosh> net.rtl8125 $ curl http://example.com\r\nok 100x30\n";

    /// RAM model of the 800×480 RGB888 surface.
    struct ModelFb(Vec<u8>);

    impl ModelFb {
        fn new() -> ModelFb {
            ModelFb(vec![0u8; gfxfb::FB_BYTES])
        }
    }

    impl Surface for ModelFb {
        fn write(&mut self, offset: usize, bytes: &[u8]) {
            self.0[offset..offset + bytes.len()].copy_from_slice(bytes);
        }
    }

    fn fnv(bytes: &[u8]) -> u64 {
        let mut f = gfxfb::Fnv1a64::new();
        f.update(bytes);
        f.value()
    }

    fn feed_str(grid: &mut Grid, s: &mut impl Surface, text: &str) {
        for byte in text.bytes() {
            grid.feed(byte, s);
        }
    }

    /// Render a fresh grid + `text` into a new model; return both.
    fn render(text: &str) -> (Grid, ModelFb) {
        let mut grid = Grid::new();
        let mut fb = ModelFb::new();
        grid.redraw(&mut fb);
        feed_str(&mut grid, &mut fb, text);
        (grid, fb)
    }

    /// The incremental-paint invariant: the frame must equal a fresh full redraw of the
    /// same shadow state (cursor included). Every behavior test pins this too.
    fn assert_matches_redraw(grid: &Grid, fb: &ModelFb) {
        let mut fresh = ModelFb::new();
        grid.clone().redraw(&mut fresh);
        assert_eq!(
            fb.0, fresh.0,
            "incremental painting diverged from a full redraw"
        );
    }

    /// White foreground bytes (B, G, R memory order).
    const WHITE: [u8; 3] = [0xff; 3];
    /// Red foreground bytes (B, G, R memory order — gfxfb's DRM `RGB888` convention).
    const RED: [u8; 3] = [0x00, 0x00, 0xff];

    /// Assert cell (`trow`,`tcol`) renders `ch`'s glyph in `color` on black (normal
    /// video — not the cursor cell).
    fn assert_cell_color(fb: &ModelFb, trow: usize, tcol: usize, ch: u8, color: [u8; 3]) {
        let bitmap = glyph(ch);
        for py in 0..CELL_H {
            for px in 0..CELL_W {
                let set = (bitmap[py / 2] >> px) & 1 != 0;
                let at = (trow * CELL_H + py) * gfxfb::STRIDE
                    + tcol * CELL_ROW_BYTES
                    + px * gfxfb::FB_BPP;
                let want = if set { color } else { [0u8; 3] };
                assert_eq!(
                    &fb.0[at..at + 3],
                    &want,
                    "pixel ({px},{py}) of cell ({trow},{tcol}) {:?}",
                    char::from(ch)
                );
            }
        }
    }

    #[test]
    fn known_lines_render_to_the_pinned_checksum() {
        let (grid, fb) = render(KNOWN_LINES);
        assert_matches_redraw(&grid, &fb);
        assert_eq!(fnv(&fb.0), KNOWN_LINES_FNV);
    }

    #[test]
    fn glyphs_are_white_on_black_doubled_rows_lsb_left() {
        let (_, fb) = render("A");
        // Cell (0,0) holds 'A' in normal video; check every pixel against the font row
        // (LSB = leftmost, each font row doubled vertically).
        let bitmap = &FONT[(b'A' - 0x20) as usize];
        for py in 0..16 {
            for px in 0..8 {
                let expected = (bitmap[py / 2] >> px) & 1 != 0;
                let at = py * gfxfb::STRIDE + px * gfxfb::FB_BPP;
                let pixel = &fb.0[at..at + 3];
                assert_eq!(
                    pixel == [0xff; 3],
                    expected,
                    "pixel ({px},{py}) of 'A' is wrong"
                );
            }
        }
        // Chroma-mangling immunity: the whole frame is grayscale, r=g=b per pixel.
        for px in fb.0.chunks_exact(3) {
            assert!(px[0] == px[1] && px[1] == px[2]);
        }
    }

    #[test]
    fn the_cursor_is_an_inverted_block() {
        let (_, fb) = render("A");
        // The cursor sits at cell (1,0) over an empty cell: a solid white block.
        let base = CELL_ROW_BYTES;
        for py in 0..16 {
            let row = &fb.0[base + py * gfxfb::STRIDE..base + py * gfxfb::STRIDE + CELL_ROW_BYTES];
            assert!(row.iter().all(|&b| b == 0xff), "cursor row {py} not solid");
        }
    }

    #[test]
    fn scroll_by_one_when_the_last_row_overflows() {
        let mut grid = Grid::new();
        let mut fb = ModelFb::new();
        grid.redraw(&mut fb);
        // 35 numbered lines into 30 rows: 5 scrolls.
        for i in 0..35 {
            let line = format!("line{i:02}\n");
            feed_str(&mut grid, &mut fb, &line);
        }
        // Each trailing `\n` past row 29 scrolls: rows hold line06..line34 and the
        // freshly opened last row is empty, cursor at its start.
        assert_eq!(&grid.text[..6], b"line06");
        assert_eq!(&grid.text[28 * COLS..28 * COLS + 6], b"line34");
        assert!(grid.text[29 * COLS..].iter().all(|&c| c == 0));
        assert_eq!((grid.row, grid.col), (ROWS - 1, 0));
        assert_matches_redraw(&grid, &fb);
        assert_eq!(fnv(&fb.0), SCROLLED_FNV);
    }

    #[test]
    fn backspace_at_column_zero_is_a_no_op() {
        let (grid, fb) = render("ab\x08\x08\x08x");
        // Two BS reach column 0; the third must do nothing; 'x' overwrites 'a'.
        assert_eq!(&grid.text[..2], b"xb");
        assert_eq!((grid.row, grid.col), (0, 1));
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn csi_and_two_byte_escapes_are_stripped() {
        // Colors, a clear, a cursor move, and a two-byte ESC sequence around plain text.
        let (grid, fb) = render("\x1b[2J\x1b[1;32meosh\x1b[0m> \x1b[10;20H\x1bMok");
        let (plain_grid, plain_fb) = render("eosh> ok");
        assert_eq!(grid.text[..], plain_grid.text[..]);
        assert_eq!((grid.row, grid.col), (plain_grid.row, plain_grid.col));
        assert_eq!(fb.0, plain_fb.0);
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn a_control_byte_aborts_a_csi_instead_of_being_eaten() {
        // A malformed CSI interrupted by a newline: the newline must still take effect.
        let (grid, fb) = render("\x1b[31\nX");
        assert_eq!(grid.text[COLS], b'X');
        assert_eq!((grid.row, grid.col), (1, 1));
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn wrap_at_the_last_column_and_box_glyphs_for_non_ascii() {
        let mut grid = Grid::new();
        let mut fb = ModelFb::new();
        grid.redraw(&mut fb);
        for _ in 0..COLS {
            grid.feed(b'x', &mut fb);
        }
        // The 100th glyph wraps the cursor to the next row.
        assert_eq!((grid.row, grid.col), (1, 0));
        // Non-ASCII and DEL render the box glyph (shadow keeps the raw byte).
        grid.feed(0x80, &mut fb);
        grid.feed(0xff, &mut fb);
        grid.feed(0x7f, &mut fb);
        assert_eq!(&grid.text[COLS..COLS + 3], &[0x80, 0xff, 0x7f]);
        // The box paints identically for all three.
        let cell = |fb: &ModelFb, col: usize| {
            let mut out = Vec::new();
            for py in 0..CELL_H {
                let at = (CELL_H + py) * gfxfb::STRIDE + col * CELL_ROW_BYTES;
                out.extend_from_slice(&fb.0[at..at + CELL_ROW_BYTES]);
            }
            out
        };
        assert_eq!(cell(&fb, 0), cell(&fb, 1));
        assert_ne!(cell(&fb, 0), vec![0u8; CELL_H * CELL_ROW_BYTES]);
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn tab_advances_to_the_next_stop_and_clamps() {
        let (grid, _) = render("ab\t");
        assert_eq!((grid.row, grid.col), (0, 8));
        let (grid, fb) = render("\t\t");
        assert_eq!((grid.row, grid.col), (0, 16));
        assert_matches_redraw(&grid, &fb);
    }

    /// Bench bug "characters go missing": a blank or colliding glyph hides exactly like
    /// a dropped byte. Space is the one legitimately blank glyph; every other printable
    /// must render visibly and distinctly (the table is `font8x8_basic` verbatim — this
    /// pins that a future edit cannot blank or duplicate an entry), and the non-ASCII
    /// fallback must be visible too.
    #[test]
    fn every_printable_glyph_is_populated_and_distinct() {
        let pop = |bitmap: &[u8; 8]| bitmap.iter().map(|row| row.count_ones()).sum::<u32>();
        assert_eq!(pop(glyph(b' ')), 0, "space must be blank");
        for byte in 0x21..=0x7eu8 {
            assert!(
                pop(glyph(byte)) >= 4,
                "glyph {byte:#04x} {:?} is blank or near-blank",
                char::from(byte)
            );
        }
        for a in 0x20..=0x7eu8 {
            for b in (a + 1)..=0x7eu8 {
                assert_ne!(
                    glyph(a),
                    glyph(b),
                    "glyphs {:?} and {:?} collide",
                    char::from(a),
                    char::from(b)
                );
            }
        }
        assert!(pop(&BOX_GLYPH) >= 4, "the unknown-byte box must be visible");
    }

    #[test]
    fn sgr_31_renders_red_and_sgr_0_resets() {
        let (grid, fb) = render("w\u{1b}[31mr\u{1b}[0mn");
        assert_cell_color(&fb, 0, 0, b'w', WHITE);
        assert_cell_color(&fb, 0, 1, b'r', RED);
        assert_cell_color(&fb, 0, 2, b'n', WHITE);
        assert_eq!(grid.attrs[..3], [0, ATTR_RED, 0]);
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn sgr_parameter_shapes() {
        // 31 applies even alongside ignored parameters.
        let (grid, fb) = render("\u{1b}[1;31mx");
        assert_cell_color(&fb, 0, 0, b'x', RED);
        assert_matches_redraw(&grid, &fb);
        // An empty SGR resets (the ANSI default-0 rule).
        let (grid, fb) = render("\u{1b}[31m\u{1b}[my");
        assert_cell_color(&fb, 0, 0, b'y', WHITE);
        assert_matches_redraw(&grid, &fb);
        // Unknown parameters alone change nothing.
        let (grid, fb) = render("\u{1b}[32mz");
        assert_cell_color(&fb, 0, 0, b'z', WHITE);
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn red_marking_survives_a_scroll() {
        let mut grid = Grid::new();
        let mut fb = ModelFb::new();
        grid.redraw(&mut fb);
        feed_str(&mut grid, &mut fb, "ok\n\u{1b}[31mbad\u{1b}[0m\n");
        // 27 line feeds reach the last row; the 28th scrolls once: `ok` falls off,
        // `bad` lands on row 0 — still red (the attribute plane scrolled with it).
        for _ in 0..28 {
            feed_str(&mut grid, &mut fb, "\n");
        }
        assert_eq!(&grid.text[..3], b"bad");
        assert_eq!(grid.attrs[..3], [ATTR_RED; 3]);
        assert_cell_color(&fb, 0, 0, b'b', RED);
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn csi_k_erases_to_the_end_of_the_line() {
        let (grid, fb) = render("abcdef\rxy\u{1b}[K");
        assert_eq!(&grid.text[..2], b"xy");
        assert!(grid.text[2..COLS].iter().all(|&c| c == 0));
        assert_eq!((grid.row, grid.col), (0, 2));
        assert_matches_redraw(&grid, &fb);
        // Beyond the cursor block the row is black.
        for py in 0..CELL_H {
            let from = py * gfxfb::STRIDE + 3 * CELL_ROW_BYTES;
            let to = (py + 1) * gfxfb::STRIDE;
            assert!(fb.0[from..to].iter().all(|&b| b == 0), "row {py} not erased");
        }
        // An explicit mode 0 behaves identically.
        let (_, explicit) = render("abcdef\rxy\u{1b}[0K");
        assert_eq!(explicit.0, fb.0);
    }

    #[test]
    fn erase_with_a_red_pen_leaves_clean_cells_but_keeps_the_pen() {
        let (grid, fb) = render("\u{1b}[31mab\r\u{1b}[K");
        assert!(grid.text[..COLS].iter().all(|&c| c == 0));
        assert!(grid.attrs[..COLS].iter().all(|&a| a == 0));
        assert_matches_redraw(&grid, &fb);
        let (grid, fb) = render("\u{1b}[31mab\r\u{1b}[Kz");
        assert_cell_color(&fb, 0, 0, b'z', RED);
        assert_matches_redraw(&grid, &fb);
    }

    /// Census discipline: nothing in the repo emits EL modes 1/2, cursor moves, clears
    /// or exotic parameter shapes — they are consumed whole with no effect (the day
    /// something emits one, implement it; never let it leak glyphs or move the cursor).
    #[test]
    fn unimplemented_csi_sequences_are_consumed_without_effect() {
        let (grid, fb) =
            render("ab\u{1b}[1K\u{1b}[2K\u{1b}[5G\u{1b}[2J\u{1b}[99999m\u{1b}[1;2;3;4;5m\u{1b}[?25lc");
        let (plain_grid, plain_fb) = render("abc");
        assert_eq!(grid.text[..], plain_grid.text[..]);
        assert_eq!((grid.row, grid.col), (plain_grid.row, plain_grid.col));
        assert_eq!(fb.0, plain_fb.0);
        assert_matches_redraw(&grid, &fb);
    }

    /// Bench bug "render errors with history recall": the exact eosh-inc editor stream
    /// (census in the module docs) — prompt, red-marked tail, recall's BS-space-BS
    /// erase, marker close, recalled line echo — must leave the screen exactly like a
    /// freshly typed line.
    #[test]
    fn the_editor_marker_and_recall_stream_renders_like_a_fresh_line() {
        let mut grid = Grid::new();
        let mut fb = ModelFb::new();
        grid.redraw(&mut fb);
        feed_str(&mut grid, &mut fb, "eosh> help \u{1b}[31mqq");
        assert_cell_color(&fb, 0, 11, b'q', RED);
        assert_cell_color(&fb, 0, 12, b'q', RED);
        // Recall: the editor erases the 7 line chars (`help qq`), then closes the mark.
        for _ in 0..7 {
            feed_str(&mut grid, &mut fb, "\u{8} \u{8}");
        }
        feed_str(&mut grid, &mut fb, "\u{1b}[0m");
        assert_eq!((grid.row, grid.col), (0, 6));
        // The recalled entry echoes.
        feed_str(&mut grid, &mut fb, "curl q");
        let (_, fresh) = render("eosh> curl q");
        assert_eq!(fb.0, fresh.0, "recall repaint must render like a fresh line");
        assert_matches_redraw(&grid, &fb);
    }

    #[test]
    fn tee_ring_carries_bytes_in_order_and_reports_full() {
        use super::ring::{TEE_RING_CAP, TeeRing};
        let ring = TeeRing::new();
        assert!(ring.is_empty());
        for i in 0..TEE_RING_CAP - 1 {
            assert!(ring.push((i % 251) as u8), "push {i} refused");
        }
        // One slot stays empty to distinguish full from empty; a full ring refuses
        // (the tee counts that refusal as a drop) instead of overwriting.
        assert!(!ring.push(0xaa));
        for i in 0..TEE_RING_CAP - 1 {
            assert_eq!(ring.pop(), Some((i % 251) as u8));
        }
        assert_eq!(ring.pop(), None);
        assert!(ring.is_empty());
        // Wraparound preserves order.
        for round in 0..2 * TEE_RING_CAP {
            assert!(ring.push((round % 256) as u8));
            assert_eq!(ring.pop(), Some((round % 256) as u8));
        }
        assert!(ring.is_empty());
    }
}
