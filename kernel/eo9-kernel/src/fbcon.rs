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
//! write goes straight to the MMIO registers); fbcon must not change that. The grid state
//! is guarded by a try-enter flag, never a spinlock: if a print re-enters mid-tee (an
//! exception handler or panic printing while a tee is in progress on the single boot
//! core), the inner bytes skip the framebuffer — serial still carries them — instead of
//! deadlocking. The panic path therefore prints to HDMI whenever the panic did not strike
//! mid-tee, and never hangs when it did.
//!
//! **Cost discipline.** Inactive (token absent): one relaxed load per byte on board
//! builds, nothing at all elsewhere (the module compiles out of non-board kernels except
//! for host tests). Active: bounded work per byte — at most two cell paints (glyph +
//! cursor, ≤ 768 framebuffer bytes) except on a line-feed past the last row, which
//! scrolls by repainting the grid from the 3000-byte shadow text (writes only; reading
//! Device-nGnRnE memory back for a memmove would double the cost).
//!
//! **Escape stripping.** The kernel emits no escape sequences today; a future colored
//! print must not corrupt the grid, so ESC-led sequences (CSI through its final byte,
//! two-byte ESC-x otherwise) are stripped, never rendered.

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

/// ANSI/CSI stripping state (a future colored print must not corrupt the grid).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    /// Ordinary bytes.
    Ground,
    /// An ESC (0x1B) has been seen; deciding the sequence shape.
    Escape,
    /// Inside `ESC [` … ; stripping until the final byte (0x40..=0x7E).
    Csi,
}

/// The console grid: shadow text (what each cell holds), cursor, escape-strip state.
/// The framebuffer is write-only downstream of this struct — every repaint renders from
/// the shadow, so scroll never reads device memory.
#[derive(Clone)]
pub struct Grid {
    /// Row-major cell contents; `0` = never written (renders as space).
    text: [u8; COLS * ROWS],
    /// Cursor column, `0..COLS`.
    col: usize,
    /// Cursor row, `0..ROWS`.
    row: usize,
    /// Escape-sequence stripping state.
    esc: Esc,
}

impl Grid {
    /// An all-empty grid, cursor at the origin. `const` so the kernel's static lives in
    /// `.bss` (the shadow text's empty cell is 0, not `b' '`, for exactly this reason).
    pub const fn new() -> Grid {
        Grid {
            text: [0; COLS * ROWS],
            col: 0,
            row: 0,
            esc: Esc::Ground,
        }
    }

    /// Paint one cell: the glyph for `cell`, white-on-black, or inverted (the cursor
    /// block). 16 row writes of 24 bytes.
    fn paint_cell(&self, s: &mut impl Surface, row: usize, col: usize, cell: u8, inverted: bool) {
        let bitmap = glyph(cell);
        let base = row * CELL_H * gfxfb::STRIDE + col * CELL_ROW_BYTES;
        for py in 0..CELL_H {
            // Each 8×8 font row covers two pixel rows (the vertical doubling).
            let bits = bitmap[py / 2];
            let mut out = [0u8; CELL_ROW_BYTES];
            for px in 0..CELL_W {
                // LSB = leftmost pixel (the font8x8 convention).
                if ((bits >> px) & 1 != 0) != inverted {
                    out[px * gfxfb::FB_BPP..(px + 1) * gfxfb::FB_BPP].fill(0xff);
                }
            }
            s.write(base + py * gfxfb::STRIDE, &out);
        }
    }

    /// Repaint every cell from the shadow text (normal video), one full pixel row per
    /// write. On an empty grid this is the screen clear.
    fn repaint(&self, s: &mut impl Surface) {
        for trow in 0..ROWS {
            for py in 0..CELL_H {
                let mut out = [0u8; gfxfb::STRIDE];
                for tcol in 0..COLS {
                    let bits = glyph(self.text[trow * COLS + tcol])[py / 2];
                    for px in 0..CELL_W {
                        if (bits >> px) & 1 != 0 {
                            let at = tcol * CELL_ROW_BYTES + px * gfxfb::FB_BPP;
                            out[at..at + gfxfb::FB_BPP].fill(0xff);
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
        self.paint_cell(
            s,
            self.row,
            self.col,
            self.text[self.row * COLS + self.col],
            true,
        );
    }

    /// Un-paint the cursor (repaint its cell in normal video).
    fn uncursor(&self, s: &mut impl Surface) {
        self.paint_cell(
            s,
            self.row,
            self.col,
            self.text[self.row * COLS + self.col],
            false,
        );
    }

    /// Full redraw: every cell from the shadow plus the cursor. Activation paints the
    /// initial (black) screen with this; the tests use it to pin the invariant that
    /// incremental painting always equals a fresh redraw of the same shadow state.
    pub fn redraw(&self, s: &mut impl Surface) {
        self.repaint(s);
        self.cursor(s);
    }

    /// Advance to the next row; on the last row, scroll by one (shadow memmove + full
    /// repaint — the framebuffer is never read).
    fn line_feed(&mut self, s: &mut impl Surface) {
        if self.row + 1 < ROWS {
            self.row += 1;
        } else {
            self.text.copy_within(COLS.., 0);
            self.text[(ROWS - 1) * COLS..].fill(0);
            self.repaint(s);
        }
    }

    /// Feed one console byte: strip escapes, handle LF/CR/BS/TAB, render printables
    /// (anything else printable-positioned renders the box glyph), keep the cursor
    /// painted. Bounded work: at most two cell paints, plus the repaint on a scroll.
    pub fn feed(&mut self, byte: u8, s: &mut impl Surface) {
        match self.esc {
            Esc::Escape => {
                // ESC [ opens a CSI; ESC ESC stays armed; any other byte completes a
                // two-byte sequence. All stripped.
                self.esc = match byte {
                    b'[' => Esc::Csi,
                    0x1b => Esc::Escape,
                    _ => Esc::Ground,
                };
                return;
            }
            Esc::Csi => match byte {
                // Parameter/intermediate bytes: keep stripping.
                0x20..=0x3f => return,
                // The final byte ends the sequence.
                0x40..=0x7e => {
                    self.esc = Esc::Ground;
                    return;
                }
                // A control byte inside a CSI: abort the strip and process it normally
                // (eating a stray LF would lose a line; malformed input, defensive).
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
            // Printable ASCII renders its glyph; 0x7F and the high half render the box
            // (the shadow keeps the raw byte so a repaint reproduces the box).
            _ => {
                self.text[self.row * COLS + self.col] = byte;
                // Painting the glyph overwrites the inverted cursor block in place.
                self.paint_cell(s, self.row, self.col, byte, false);
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

    use super::{COLS, Grid, ROWS, Surface};
    use crate::gfxfb;

    /// The grid behind the tee. Sole mutator is [`tee_byte`] under the [`BUSY`] guard
    /// (plus [`activate`] before [`ACTIVE`] is ever true); single boot core.
    struct GridCell(UnsafeCell<Grid>);
    // SAFETY: every `&mut` borrow is serialized by the BUSY try-enter guard below (and
    // activation happens-before the first guarded borrow via the ACTIVE release store).
    unsafe impl Sync for GridCell {}

    static GRID: GridCell = GridCell(UnsafeCell::new(Grid::new()));
    /// Whether the tee is live (the `fbcon` token was accepted and the surface located).
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    /// Try-enter guard: a print re-entering mid-tee (exception/panic on the boot core)
    /// skips the framebuffer instead of deadlocking or aliasing the grid.
    static BUSY: AtomicBool = AtomicBool::new(false);
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

    /// The per-byte tee, called from the console TX chokepoint (`uart::put_byte`).
    /// Inactive cost: one relaxed load. Never blocks: re-entry (an exception or panic
    /// printing while a tee is in progress) skips the framebuffer for those bytes —
    /// serial still carries them.
    pub fn tee_byte(byte: u8) {
        if !ACTIVE.load(Ordering::Relaxed) {
            return;
        }
        if BUSY.swap(true, Ordering::Acquire) {
            return;
        }
        let mut surface = FbSurface {
            base: FB_BASE.load(Ordering::Relaxed),
        };
        // SAFETY: the BUSY try-enter guard makes this the only live `&mut Grid` (single
        // core; re-entrant prints bailed out above), and ACTIVE's release store ordered
        // activation's initialization before this borrow.
        unsafe { (*GRID.0.get()).feed(byte, &mut surface) };
        BUSY.store(false, Ordering::Release);
    }
}

#[cfg(all(
    target_os = "none",
    target_arch = "aarch64",
    feature = "board-opi5plus"
))]
pub use tee::{activate, tee_byte};

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
}
