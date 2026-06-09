//! Pure math for the Orange Pi 5 Plus firmware framebuffer (`gfx.simplefb`).
//!
//! Geometry constants, rectangle/buffer validation, and the xrgb8888 ↔ packed-RGB888
//! boundary conversion for the board's vendor-U-Boot scanout surface — everything the
//! provider does that is not a device access. Kept free of hardware and alloc so it
//! compiles — and its unit tests run — on the host triple as well as on bare metal
//! (the `ticks` module's pattern).
//!
//! The surface (VERIFIED live on the bench, 2026-06-08, docs/board/hdmi-simplefb-plan.md):
//! vendor U-Boot leaves VOP2's Esmart0 window scanning **base 0xee01a000, 800×480,
//! packed RGB888 (3 bytes/px), stride 2400 bytes** across the `go` handoff. The eo9:gfx
//! API models the surface as xrgb8888 (the API's only v1 format); this module converts
//! at the boundary — present drops the X byte, read synthesizes X = 0 — so the consumer
//! contract is exactly `gfx.mem`'s and the cross-backend checksum identity holds for any
//! content with X = 0 (which is what the API requires writers to produce).
//!
//! Byte order: the framebuffer pixel is stored B, G, R in increasing memory (the
//! little-endian DRM `RGB888` convention, matching xrgb8888's B, G, R, X) — so the
//! conversion is a plain truncation/extension. The board's HDMI link mangles chroma
//! anyway (RGB scanned, YCbCr interpreted — see the plan doc), so this choice is
//! unverifiable on the monitor and irrelevant to the luma-first v1: grayscale (r=g=b)
//! renders identically under any channel order.

/// Visible width in pixels (VOP2 Esmart0 ACT_INFO, verified).
pub const WIDTH: u32 = 800;
/// Visible height in pixels.
pub const HEIGHT: u32 = 480;
/// Bytes per scanline of the backing surface (800 px × 3 bytes, = VIR 600 words).
pub const STRIDE: usize = 2400;
/// Bytes per packed-RGB888 framebuffer pixel.
pub const FB_BPP: usize = 3;
/// Bytes per xrgb8888 operation-buffer pixel (the API side).
pub const BUF_BPP: usize = 4;
/// Total framebuffer bytes (0x119400).
pub const FB_BYTES: usize = STRIDE * HEIGHT as usize;

/// The board profile's framebuffer base (Esmart0 MST at the U-Boot prompt, verified by
/// live paint probes; may move across power cycles — the locator cross-checks MST).
pub const PROFILE_BASE: usize = 0xee01_a000;
/// Expected Esmart0 VIR readback: the virtual stride in 32-bit words (2400 / 4 = 600).
pub const PROFILE_VIR: u32 = 0x258;
/// Expected Esmart0 ACT_INFO readback: `(height-1) << 16 | (width-1)` for 800×480.
pub const PROFILE_ACT: u32 = 0x01df_031f;

/// A rectangle in framebuffer coordinates (the WIT `rect`, dependency-free).
#[derive(Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Byte addressing for one validated rectangle: framebuffer side (packed RGB888 over
/// the backing stride) and operation-buffer side (tightly packed xrgb8888 rows).
#[derive(Clone, Copy)]
pub struct RectPlan {
    /// Byte offset of the rectangle's first pixel from the framebuffer base
    /// (`y * STRIDE + x * FB_BPP`).
    pub fb_start: usize,
    /// Rows in the rectangle.
    pub rows: usize,
    /// Bytes per rectangle row on the framebuffer side (`width * FB_BPP`).
    pub fb_row_bytes: usize,
    /// Bytes per rectangle row on the buffer side (`width * BUF_BPP`).
    pub buf_row_bytes: usize,
}

/// Validate `rect` against the fixed 800×480 mode (the gfx.mem `check_rect` logic:
/// overflow-checked ends, strictly inside the mode; zero-area rectangles are valid).
pub fn check_rect(rect: &Rect) -> Result<RectPlan, ()> {
    let (Some(end_x), Some(end_y)) = (
        rect.x.checked_add(rect.width),
        rect.y.checked_add(rect.height),
    ) else {
        return Err(());
    };
    if end_x > WIDTH || end_y > HEIGHT {
        return Err(());
    }
    Ok(RectPlan {
        fb_start: rect.y as usize * STRIDE + rect.x as usize * FB_BPP,
        rows: rect.height as usize,
        fb_row_bytes: rect.width as usize * FB_BPP,
        buf_row_bytes: rect.width as usize * BUF_BPP,
    })
}

/// Validate that `buffer_len` covers the rectangle's tightly packed xrgb8888 pixels
/// (the gfx.mem `check_buffer` logic). `Ok` carries the needed byte count; `Err`
/// carries it too, for the caller's `bad-buffer` message.
pub fn check_buffer(rect: &Rect, buffer_len: u64) -> Result<u64, u64> {
    let needed = u64::from(rect.width) * u64::from(rect.height) * BUF_BPP as u64;
    if buffer_len < needed {
        Err(needed)
    } else {
        Ok(needed)
    }
}

/// Pack one tightly-packed xrgb8888 row (memory bytes B,G,R,X per pixel) into packed
/// RGB888 (B,G,R): drop the X byte. `src.len()` must be `4 * n`, `dst.len()` `3 * n`.
pub fn pack_xrgb_row(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len() / BUF_BPP, dst.len() / FB_BPP);
    for (px, out) in src.chunks_exact(BUF_BPP).zip(dst.chunks_exact_mut(FB_BPP)) {
        out.copy_from_slice(&px[..FB_BPP]);
    }
}

/// Unpack one packed-RGB888 row into tightly-packed xrgb8888: synthesize X = 0.
pub fn unpack_rgb888_row(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len() / FB_BPP, dst.len() / BUF_BPP);
    for (px, out) in src.chunks_exact(FB_BPP).zip(dst.chunks_exact_mut(BUF_BPP)) {
        out[..FB_BPP].copy_from_slice(px);
        out[FB_BPP] = 0;
    }
}

/// The packed-RGB888 memory bytes (B, G, R) of a `0x00RRGGBB` clear color.
pub fn color_rgb888(color: u32) -> [u8; FB_BPP] {
    [color as u8, (color >> 8) as u8, (color >> 16) as u8]
}

/// Decode an Esmart0 ACT_INFO readback into (width, height) pixels.
pub fn act_geometry(act: u32) -> (u32, u32) {
    ((act & 0xffff) + 1, (act >> 16) + 1)
}

/// Whether a live Esmart0 MST readback is a believable scanout base to adopt over the
/// profile constant (the plan's risk 3: U-Boot heap-allocates the buffer, so the base
/// may move across power cycles). Believable = word-aligned, above the kernel's own
/// 528 MiB DRAM window (we own that memory — a scanout there would mean the heap is
/// painting over itself), and wholly below the RK3588's MMIO region.
pub fn dram_plausible(base: u64) -> bool {
    /// First byte past the kernel's identity-mapped DRAM (arch/aarch64/mmu.rs RAM_END).
    const KERNEL_RAM_END: u64 = 0x2100_0000;
    /// Start of the RK3588 peripheral/MMIO space (PCIe windows upward).
    const MMIO_START: u64 = 0xF000_0000;
    base % 4 == 0
        && base >= KERNEL_RAM_END
        && base
            .checked_add(FB_BYTES as u64)
            .is_some_and(|end| end <= MMIO_START)
}

// -----------------------------------------------------------------------------------------
// The M1 first-light probe pattern: four horizontal grayscale bands
// -----------------------------------------------------------------------------------------

/// The grayscale band value for row `y`: 0x00, 0x55, 0xAA, 0xFF in quarters. Grayscale
/// because r=g=b is the invariant of the board's HDMI colorspace confusion — these bands
/// render faithfully through the mangled link (plan doc, Round-0).
pub fn band_byte(y: u32) -> u8 {
    [0x00, 0x55, 0xAA, 0xFF][((y / (HEIGHT / 4)) as usize).min(3)]
}

/// The expected FNV-1a-64 of the full framebuffer after the band paint (every byte of a
/// row is its band value — r=g=b makes the row a constant byte run). The probe prints
/// this next to the read-back checksum; the host unit test pins it.
pub fn bands_crc_expected() -> u64 {
    let mut fnv = Fnv1a64::new();
    for y in 0..HEIGHT {
        let row = [band_byte(y); 64];
        let mut remaining = STRIDE;
        while remaining > 0 {
            let take = remaining.min(row.len());
            fnv.update(&row[..take]);
            remaining -= take;
        }
    }
    fnv.value()
}

// -----------------------------------------------------------------------------------------
// FNV-1a-64 (the gfx checksum convention — draw, the integration tests, xtask)
// -----------------------------------------------------------------------------------------

/// Incremental FNV-1a-64, so device read-backs can stream through without allocating.
pub struct Fnv1a64(u64);

impl Fnv1a64 {
    pub fn new() -> Fnv1a64 {
        Fnv1a64(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a-64 of the M1 grayscale-band frame. Pinned so a band-layout or checksum
    /// drift fails here before it confuses a bench round.
    const BANDS_CRC: u64 = 0x53c2_a266_24c0_2925;

    /// FNV-1a-64 of draw's frame-1 test pattern at 800×480 as tightly packed xrgb8888
    /// (X = 0) — the canonical gfx.mem read-back checksum at the board geometry. The
    /// SAME literal is pinned in tests/eo9-integration/tests/gfx.rs against the real
    /// `gfx.mem $ draw` run; if either side's copy of the pattern or packing drifts,
    /// its pin fails. `gfx.simplefb` reading back this number on the board is the
    /// cross-backend identity, M3's acceptance.
    const PATTERN_800X480_FRAME1_FNV: u64 = 0xd66b_49ee_575f_f0d9;

    fn fnv(bytes: &[u8]) -> u64 {
        let mut f = Fnv1a64::new();
        f.update(bytes);
        f.value()
    }

    // --- rect / buffer validation ----------------------------------------------------

    #[test]
    fn rects_inside_the_mode_plan_the_documented_addressing() {
        let plan = check_rect(&Rect {
            x: 3,
            y: 7,
            width: 10,
            height: 2,
        })
        .unwrap();
        // base + y*2400 + x*3 — the deliverable's addressing, byte for byte.
        assert_eq!(plan.fb_start, 7 * 2400 + 3 * 3);
        assert_eq!(plan.rows, 2);
        assert_eq!(plan.fb_row_bytes, 30);
        assert_eq!(plan.buf_row_bytes, 40);
        // The full mode and zero-area rectangles are valid.
        assert!(
            check_rect(&Rect {
                x: 0,
                y: 0,
                width: 800,
                height: 480
            })
            .is_ok()
        );
        assert!(
            check_rect(&Rect {
                x: 800,
                y: 480,
                width: 0,
                height: 0
            })
            .is_ok()
        );
    }

    #[test]
    fn rects_outside_the_mode_or_overflowing_are_rejected() {
        assert!(
            check_rect(&Rect {
                x: 800,
                y: 0,
                width: 1,
                height: 1
            })
            .is_err()
        );
        assert!(
            check_rect(&Rect {
                x: 0,
                y: 480,
                width: 1,
                height: 1
            })
            .is_err()
        );
        assert!(
            check_rect(&Rect {
                x: 799,
                y: 479,
                width: 2,
                height: 1
            })
            .is_err()
        );
        assert!(
            check_rect(&Rect {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 1
            })
            .is_err()
        );
        assert!(
            check_rect(&Rect {
                x: 0,
                y: 1,
                width: 1,
                height: u32::MAX
            })
            .is_err()
        );
    }

    #[test]
    fn buffers_must_cover_the_rectangle() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert_eq!(check_buffer(&rect, 400), Ok(400));
        assert_eq!(check_buffer(&rect, 500), Ok(400));
        assert_eq!(check_buffer(&rect, 399), Err(400));
        let zero = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(check_buffer(&zero, 0), Ok(0));
    }

    // --- boundary conversion ----------------------------------------------------------

    #[test]
    fn the_conversion_round_trip_preserves_rgb_and_zeroes_x() {
        let src = [0x11, 0x22, 0x33, 0xde, 0x44, 0x55, 0x66, 0xad];
        let mut packed = [0u8; 6];
        pack_xrgb_row(&src, &mut packed);
        assert_eq!(packed, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        let mut back = [0xffu8; 8];
        unpack_rgb888_row(&packed, &mut back);
        assert_eq!(back, [0x11, 0x22, 0x33, 0x00, 0x44, 0x55, 0x66, 0x00]);
    }

    #[test]
    fn clear_colors_pack_like_pixels() {
        // 0x00RRGGBB → memory B, G, R: the same bytes a presented pixel would carry.
        assert_eq!(color_rgb888(0x00aa_bbcc), [0xcc, 0xbb, 0xaa]);
        let mut packed = [0u8; 3];
        pack_xrgb_row(&[0xcc, 0xbb, 0xaa, 0x00], &mut packed);
        assert_eq!(packed, color_rgb888(0x00aa_bbcc));
    }

    // --- locator helpers ----------------------------------------------------------------

    #[test]
    fn the_verified_esmart0_readbacks_decode_to_the_profile() {
        assert_eq!(act_geometry(PROFILE_ACT), (800, 480));
        assert_eq!(PROFILE_VIR as usize * 4, STRIDE);
        assert_eq!(FB_BYTES, 0x119400);
        assert!(dram_plausible(PROFILE_BASE as u64));
    }

    #[test]
    fn implausible_mst_values_are_rejected() {
        assert!(!dram_plausible(0)); // window disabled / garbage
        assert!(!dram_plausible(0x0020_0000)); // inside the kernel's own DRAM
        assert!(!dram_plausible(0x2100_0000 - 4)); // straddles the kernel RAM end
        assert!(!dram_plausible(0xefff_f002)); // unaligned
        assert!(!dram_plausible(0xf000_0000)); // in MMIO space
        assert!(!dram_plausible(0xefff_0000)); // end would cross into MMIO
        assert!(dram_plausible(0x2100_0000)); // first plausible byte
    }

    // --- the M1 band pattern -------------------------------------------------------------

    #[test]
    fn the_band_paint_checksum_is_pinned() {
        assert_eq!(band_byte(0), 0x00);
        assert_eq!(band_byte(119), 0x00);
        assert_eq!(band_byte(120), 0x55);
        assert_eq!(band_byte(240), 0xaa);
        assert_eq!(band_byte(479), 0xff);
        assert_eq!(bands_crc_expected(), BANDS_CRC);
    }

    // --- cross-backend identity -----------------------------------------------------------
    //
    // A RAM-simulated RGB888 framebuffer driven through this module's plans and
    // conversions must read back exactly what gfx.mem (stride = width*4, byte-identical
    // storage) reads back, for any X = 0 content. That is the provider's whole contract:
    // gfx.mem semantics over base + y*2400 + x*3.

    /// Present `src` (tightly packed xrgb rows of `rect`) into a RAM RGB888 fb the way
    /// the kernel provider does.
    fn simplefb_present(fb: &mut [u8], rect: &Rect, src: &[u8]) {
        let plan = check_rect(rect).unwrap();
        assert!(check_buffer(rect, src.len() as u64).is_ok());
        let mut scratch = vec![0u8; plan.fb_row_bytes];
        for row in 0..plan.rows {
            pack_xrgb_row(
                &src[row * plan.buf_row_bytes..(row + 1) * plan.buf_row_bytes],
                &mut scratch,
            );
            let at = plan.fb_start + row * STRIDE;
            fb[at..at + plan.fb_row_bytes].copy_from_slice(&scratch);
        }
    }

    /// Read `rect` back out as tightly packed xrgb rows.
    fn simplefb_read(fb: &[u8], rect: &Rect) -> Vec<u8> {
        let plan = check_rect(rect).unwrap();
        let mut out = vec![0u8; plan.buf_row_bytes * plan.rows];
        for row in 0..plan.rows {
            let at = plan.fb_start + row * STRIDE;
            unpack_rgb888_row(
                &fb[at..at + plan.fb_row_bytes],
                &mut out[row * plan.buf_row_bytes..(row + 1) * plan.buf_row_bytes],
            );
        }
        out
    }

    /// gfx.mem's storage model: xrgb8888 at stride = width*4 (a verbatim transcription
    /// of guest/stubs/gfx-mem's present/read row math).
    fn mem_present(fb: &mut [u8], rect: &Rect, src: &[u8]) {
        let stride = WIDTH as usize * BUF_BPP;
        let row_bytes = rect.width as usize * BUF_BPP;
        let start = rect.y as usize * stride + rect.x as usize * BUF_BPP;
        for row in 0..rect.height as usize {
            fb[start + row * stride..start + row * stride + row_bytes]
                .copy_from_slice(&src[row * row_bytes..(row + 1) * row_bytes]);
        }
    }

    fn mem_read(fb: &[u8], rect: &Rect) -> Vec<u8> {
        let stride = WIDTH as usize * BUF_BPP;
        let row_bytes = rect.width as usize * BUF_BPP;
        let start = rect.y as usize * stride + rect.x as usize * BUF_BPP;
        let mut out = Vec::new();
        for row in 0..rect.height as usize {
            out.extend_from_slice(&fb[start + row * stride..start + row * stride + row_bytes]);
        }
        out
    }

    /// Deterministic X = 0 content (splitmix64 over the pixel index).
    fn content(rect: &Rect, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity((rect.width * rect.height) as usize * BUF_BPP);
        let mut state = seed;
        for _ in 0..rect.width * rect.height {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            let [b, g, r, ..] = z.to_le_bytes();
            out.extend_from_slice(&[b, g, r, 0]);
        }
        out
    }

    #[test]
    fn the_two_backends_read_back_identically_for_x_zero_content() {
        let mut simple = vec![0u8; FB_BYTES];
        let mut mem = vec![0u8; WIDTH as usize * HEIGHT as usize * BUF_BPP];
        let full = Rect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        };
        let frame = content(&full, 9);
        simplefb_present(&mut simple, &full, &frame);
        mem_present(&mut mem, &full, &frame);
        // A partial-damage overwrite at an x*3-unaligned-to-4 offset (the stride math).
        let damage = Rect {
            x: 13,
            y: 101,
            width: 333,
            height: 57,
        };
        let patch = content(&damage, 1234);
        simplefb_present(&mut simple, &damage, &patch);
        mem_present(&mut mem, &damage, &patch);

        let simple_back = simplefb_read(&simple, &full);
        assert_eq!(simple_back, mem_read(&mem, &full));
        assert_eq!(simple_back, {
            // Independent expectation: a full-frame tightly packed xrgb image IS a
            // stride-width*4 framebuffer, so the overlay math applies to it directly.
            let mut composite = frame;
            mem_present(&mut composite, &damage, &patch);
            composite
        });
        assert_eq!(simplefb_read(&simple, &damage), mem_read(&mem, &damage));
    }

    #[test]
    fn the_draw_pattern_checksum_at_the_board_geometry_is_pinned() {
        // The draw test pattern, frame 1 — a verbatim copy of guest/examples/draw
        // (and of tests/eo9-integration/tests/gfx.rs and xtask's gfx_pattern); the
        // pinned literal is what keeps this fourth copy honest: any drift in any copy
        // fails its own pin against the same number.
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
        let full = Rect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        };
        let mut pattern = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * BUF_BPP);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (r, g, b) = base_pixel(WIDTH, HEIGHT, x, y);
                pattern.extend_from_slice(&[b, g, r, 0]);
            }
        }
        assert_eq!(fnv(&pattern), PATTERN_800X480_FRAME1_FNV);

        // Present → read through the simplefb packing: the identity, at the canonical
        // number gfx.mem reports for the same frame.
        let mut fb = vec![0u8; FB_BYTES];
        simplefb_present(&mut fb, &full, &pattern);
        assert_eq!(fnv(&simplefb_read(&fb, &full)), PATTERN_800X480_FRAME1_FNV);
    }
}
