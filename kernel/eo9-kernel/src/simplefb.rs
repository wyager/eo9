//! The Orange Pi 5 Plus firmware framebuffer: locator, M1 first-light probe, and the
//! Device-mapping-safe accessors the `gfx.simplefb` provider copies through.
//!
//! Vendor U-Boot leaves VOP2's Esmart0 window scanning a live 800×480 RGB888 surface
//! across the `go` handoff (verified by paint probes on the bench, 2026-06-08 —
//! docs/board/hdmi-simplefb-plan.md). The kernel never touches CRU/VOP registers: it
//! adopts the running scanout and just writes pixels.
//!
//! **Locator.** Primary = the board-profile constants ([`gfxfb::PROFILE_BASE`], 800×480,
//! stride 2400). Cross-check = the live Esmart0 window registers (MST/VIR/ACT at
//! 0xfdd91800), read through the syndrome-valid `mmio` accessors; both are printed, a
//! disagreement warns `MISMATCH`, and a DRAM-plausible MST wins (the plan's risk 3:
//! U-Boot heap-allocates the buffer, so the base may move across power cycles).
//!
//! **Mapping.** The surface lives in the fourth-GiB Device-nGnRnE block of the identity
//! map (arch/aarch64/mmu.rs `DEVICE_L1` index 3), which is RW + PXN + UXN — W^X holds
//! without new tables. Device-nGnRnE is fine for 384 KiB frames (the plan's call: a
//! Normal-NC MAIR attribute is a recorded perf follow-up, not an M2 dependency); every
//! access below is pinned to single-register-width loads/stores (`crate::mmio`), which
//! Device memory always allows — ordinary slice copies could merge into unaligned or
//! SIMD accesses and fault. The map check runs BEFORE the first touch: a base outside
//! the device window prints `fb: unmapped 0x…` and the caller stops cleanly.

use crate::gfxfb;
use crate::mmio;

/// VOP2 Esmart0 window register block (RK3588 TRM; live readbacks verified Round-0).
const ESMART0_BASE: usize = 0xfdd9_1800;
/// Esmart0 MST: the scanout buffer base address.
const ESMART0_MST: usize = ESMART0_BASE + 0x14;
/// Esmart0 VIR: the virtual stride in 32-bit words.
const ESMART0_VIR: usize = ESMART0_BASE + 0x1c;
/// Esmart0 ACT_INFO: `(height-1) << 16 | (width-1)`.
const ESMART0_ACT: usize = ESMART0_BASE + 0x20;

/// The identity map's fourth-GiB Device block (arch/aarch64/mmu.rs `DEVICE_L1` = [3, 41]):
/// the only window the framebuffer can be reached through. The 42nd-GiB block holds PCIe
/// DBI registers, never DRAM, so it is deliberately not accepted here.
const DEVICE_WINDOW_START: usize = 0xC000_0000;
const DEVICE_WINDOW_END: usize = 0x1_0000_0000;

/// A located, map-checked framebuffer surface.
pub struct Located {
    /// The adopted scanout base (MST when DRAM-plausible, else the profile constant).
    pub base: usize,
    /// Live Esmart0 readbacks, for the provider's geometry check.
    pub vir: u32,
    pub act: u32,
}

/// Whether `[base, base + len)` lies wholly inside the identity map's device window.
fn mapped(base: usize, len: usize) -> bool {
    base >= DEVICE_WINDOW_START
        && base
            .checked_add(len)
            .is_some_and(|end| end <= DEVICE_WINDOW_END)
}

/// Locate the surface and print the evidence (the M1 diagnostic lines, also emitted on
/// the provider's first use). `Err` means the chosen base is outside the kernel's MMU
/// coverage — already reported as `fb: unmapped 0x…` — and nothing was touched.
pub fn locate_and_report() -> Result<Located, ()> {
    // SAFETY: the Esmart0 registers sit inside the identity-mapped fourth-GiB device
    // block; reads are side-effect-free status/configuration readbacks.
    let (mst, vir, act) = unsafe {
        (
            mmio::read_u32(ESMART0_MST),
            mmio::read_u32(ESMART0_VIR),
            mmio::read_u32(ESMART0_ACT),
        )
    };
    crate::kprintln!(
        "fb: profile base={:#x} {}x{} rgb888 stride={}",
        gfxfb::PROFILE_BASE,
        gfxfb::WIDTH,
        gfxfb::HEIGHT,
        gfxfb::STRIDE,
    );
    crate::kprintln!("fb: vop2 mst={mst:#010x} vir={vir:#x} act={act:#010x}");
    if mst as usize != gfxfb::PROFILE_BASE || vir != gfxfb::PROFILE_VIR || act != gfxfb::PROFILE_ACT
    {
        crate::kprintln!(
            "fb: MISMATCH between the profile constants and the live Esmart0 window \
             (expected mst={:#010x} vir={:#x} act={:#010x})",
            gfxfb::PROFILE_BASE,
            gfxfb::PROFILE_VIR,
            gfxfb::PROFILE_ACT,
        );
    }
    let base = if gfxfb::dram_plausible(u64::from(mst)) {
        mst as usize
    } else {
        if mst as usize != gfxfb::PROFILE_BASE {
            crate::kprintln!(
                "fb: vop2 mst {mst:#010x} is not DRAM-plausible; using the profile base"
            );
        }
        gfxfb::PROFILE_BASE
    };
    if !mapped(base, gfxfb::FB_BYTES) {
        crate::kprintln!("fb: unmapped {base:#x} (outside the identity map's device window)");
        return Err(());
    }
    Ok(Located { base, vir, act })
}

/// M1: the `gfxprobe` boot-token diagnostic. Locate (with the evidence lines), paint
/// four horizontal grayscale bands (0x00/0x55/0xAA/0xFF as r=g=b — the values that
/// survive the board's HDMI colorspace confusion), read the frame back, and print the
/// FNV-1a-64 next to the expected value. The capture stick closes the visual loop.
pub fn probe() {
    let Ok(located) = locate_and_report() else {
        return;
    };
    for y in 0..gfxfb::HEIGHT {
        let value = gfxfb::band_byte(y);
        let word = u32::from_le_bytes([value; 4]);
        // SAFETY: row `y` of the map-checked surface; STRIDE is a multiple of 4 and the
        // base is word-aligned (dram_plausible / the profile constant), so every store
        // is an aligned `str`.
        unsafe {
            fill_words(
                located.base + y as usize * gfxfb::STRIDE,
                word,
                gfxfb::STRIDE / 4,
            );
        }
    }
    let crc = device_crc(located.base, gfxfb::FB_BYTES);
    crate::kprintln!(
        "fb: painted crc={crc:#018x} (expected {:#018x})",
        gfxfb::bands_crc_expected(),
    );
}

// -----------------------------------------------------------------------------------------
// Device-mapping-safe bulk accessors (single-register-width, syndrome-valid)
// -----------------------------------------------------------------------------------------

/// Fill `count` aligned words at `addr` with `word`.
///
/// # Safety
/// `[addr, addr + count*4)` must lie inside the mapped, writable device window and
/// `addr` must be 4-byte aligned (the locator guarantees both for surface rows).
pub unsafe fn fill_words(mut addr: usize, word: u32, count: usize) {
    for _ in 0..count {
        // SAFETY: forwarded caller contract.
        unsafe { mmio::write_u32(addr, word) };
        addr += 4;
    }
}

/// Copy `src` to device memory at `addr`: byte stores up to the first word boundary,
/// aligned word stores for the body, byte stores for the tail.
///
/// # Safety
/// `[addr, addr + src.len())` must lie inside the mapped, writable device window.
pub unsafe fn copy_out(mut addr: usize, mut src: &[u8]) {
    while addr % 4 != 0 && !src.is_empty() {
        // SAFETY: forwarded caller contract.
        unsafe { mmio::write_u8(addr, src[0]) };
        addr += 1;
        src = &src[1..];
    }
    let mut words = src.chunks_exact(4);
    for chunk in &mut words {
        // SAFETY: forwarded caller contract; `addr` is now 4-byte aligned.
        unsafe {
            mmio::write_u32(
                addr,
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            )
        };
        addr += 4;
    }
    for &byte in words.remainder() {
        // SAFETY: forwarded caller contract.
        unsafe { mmio::write_u8(addr, byte) };
        addr += 1;
    }
}

/// Copy device memory at `addr` into `dst` (same head/body/tail discipline).
///
/// # Safety
/// `[addr, addr + dst.len())` must lie inside the mapped, readable device window.
pub unsafe fn copy_in(mut addr: usize, mut dst: &mut [u8]) {
    while addr % 4 != 0 && !dst.is_empty() {
        // SAFETY: forwarded caller contract.
        dst[0] = unsafe { mmio::read_u8(addr) };
        addr += 1;
        dst = &mut dst[1..];
    }
    let mut words = dst.chunks_exact_mut(4);
    for chunk in &mut words {
        // SAFETY: forwarded caller contract; `addr` is now 4-byte aligned.
        let word = unsafe { mmio::read_u32(addr) };
        chunk.copy_from_slice(&word.to_le_bytes());
        addr += 4;
    }
    for byte in words.into_remainder() {
        // SAFETY: forwarded caller contract.
        *byte = unsafe { mmio::read_u8(addr) };
        addr += 1;
    }
}

/// Stream `[base, base + len)` of device memory through FNV-1a-64 without allocating.
fn device_crc(base: usize, len: usize) -> u64 {
    let mut fnv = gfxfb::Fnv1a64::new();
    let mut chunk = [0u8; 64];
    let mut offset = 0;
    while offset < len {
        let take = (len - offset).min(chunk.len());
        // SAFETY: the caller located and map-checked `[base, base + len)`.
        unsafe { copy_in(base + offset, &mut chunk[..take]) };
        fnv.update(&chunk[..take]);
        offset += take;
    }
    fnv.value()
}

// -----------------------------------------------------------------------------------------
// The provider's surface gate (M2)
// -----------------------------------------------------------------------------------------

/// Locate the surface for the `gfx.simplefb` provider: the locator above plus the
/// geometry gate — the live window must scan exactly the supported 800×480 RGB888
/// surface, anything else is refused honestly (the WIT's fallible `mode` exists for
/// exactly this). The error string becomes the API's typed `io` payload.
#[cfg(feature = "wasm-store")]
pub fn provider_surface() -> Result<usize, alloc::string::String> {
    use alloc::format;
    match locate_and_report() {
        Err(()) => Err(alloc::string::String::from(
            "the framebuffer is outside the kernel's MMU coverage (see `fb: unmapped` above)",
        )),
        Ok(located) => {
            let (width, height) = gfxfb::act_geometry(located.act);
            if (width, height) != (gfxfb::WIDTH, gfxfb::HEIGHT) || located.vir != gfxfb::PROFILE_VIR
            {
                Err(format!(
                    "the live scanout geometry {width}x{height} (vir={:#x}) is not the supported \
                     {}x{} rgb888 stride-{} surface",
                    located.vir,
                    gfxfb::WIDTH,
                    gfxfb::HEIGHT,
                    gfxfb::STRIDE,
                ))
            } else {
                Ok(located.base)
            }
        }
    }
}
