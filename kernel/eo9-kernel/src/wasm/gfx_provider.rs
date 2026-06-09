//! Kernel-side root provider for `eo9:gfx` — `gfx.simplefb`, the board framebuffer.
//!
//! This is the dumb-framebuffer root the gfx API was designed for (wit/gfx package
//! docs, docs/board/gfx-simplefb.md): gfx.mem semantics over the firmware-configured
//! scanout the vendor U-Boot leaves running across the `go` handoff — an address plus
//! width/height/stride/format and nothing else. The rectangle/buffer validation and the
//! xrgb8888 ↔ packed-RGB888 boundary conversion are the pure `crate::gfxfb` module
//! (host-unit-tested: the cross-backend checksum identity with gfx.mem is pinned
//! there); this module is only the WIT plumbing and the device copies
//! (`crate::simplefb`'s single-register-width accessors over the Device-nGnRnE
//! mapping, which is RW + PXN + UXN — W^X without new tables).
//!
//! **Containment.** The framebuffer is raw physical memory, so the provider is **never
//! linked by default**: the operator grants it for a boot with the bare `gfx` token on
//! the kernel command line (the `pci` token's grammar). Without the token a program
//! importing `eo9:gfx` is refused at instantiation with the capability story
//! (`shellexec::missing_capability`); with it the loader rule still applies — only
//! programs that import `eo9:gfx` link it. Attenuators and stubs (`gfx.mem`,
//! `gfx.deny`, `gfx.none`) compose in front exactly as the WIT intends.
//!
//! **The surface gate.** The locator runs once, on first use (`simplefb::
//! provider_surface`): profile constants cross-checked against the live Esmart0
//! window, the evidence printed, the map checked before any touch, and any other
//! geometry refused with the API's typed `io` error — `mode()` never invents a mode.
//! `mode()` reports the surface in the API's own pixel model (xrgb8888, stride =
//! width×4, honoring the WIT's `stride >= width * 4` contract); the packed-RGB888
//! backing stride (2400) is a provider internal, printed in the `fb:` diagnostics.
//!
//! The board's HDMI link mangles chroma (RGB scanned, YCbCr interpreted); pixel VALUES
//! are stored and read back faithfully — the caveat lives in the docs, not the
//! contract (luma-first v1, plan doc).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};

use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use super::providers::KernelState;
use super::shellexec::KLock;
use super::shellfs::BufferRes;
use crate::gfxfb;

/// Boxed future shape for `func_wrap_concurrent` closures (same alias as the other
/// kernel providers).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

// -----------------------------------------------------------------------------------------
// Boot-time grant
// -----------------------------------------------------------------------------------------

/// Whether this boot granted the gfx capability (the bare `gfx` kernel command-line
/// token).
static GFX_GRANTED: AtomicBool = AtomicBool::new(false);

/// Record the boot-time grant decision (called once from `runner::boot`).
pub fn set_granted(granted: bool) {
    GFX_GRANTED.store(granted, Ordering::Relaxed);
}

/// Whether linkers built for this boot should include the `eo9:gfx` root provider.
pub fn granted() -> bool {
    GFX_GRANTED.load(Ordering::Relaxed)
}

/// One-time `gfx: first present (mapping=device)` diagnostic (the mapping A/B datum:
/// v1 presents through the Device-nGnRnE block; a Normal-NC mapping is the recorded
/// follow-up).
static FIRST_PRESENT: AtomicBool = AtomicBool::new(false);

/// The locate-once surface cache: `Ok(base)` after a successful locate + geometry
/// check, `Err(reason)` when the surface was refused (both outcomes are stable for the
/// life of the boot — the scanout configuration is firmware state we never write).
static SURFACE: KLock<Option<core::result::Result<usize, String>>> = KLock::new(None);

/// The surface gate every operation passes: locate on first use (printing the `fb:`
/// evidence lines once), then the cached verdict.
fn surface() -> core::result::Result<usize, WitGfxError> {
    let cached = SURFACE.with(|slot| slot.clone());
    let result = match cached {
        Some(result) => result,
        None => {
            let result = crate::simplefb::provider_surface();
            SURFACE.with(|slot| *slot = Some(result.clone()));
            result
        }
    };
    result.map_err(WitGfxError::Io)
}

// -----------------------------------------------------------------------------------------
// Host resource representation and WIT-shaped types
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:gfx/types.gfx-impl` (stateless token; the framebuffer is
/// the state).
struct GfxCap;

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
enum WitPixelFormat {
    #[component(name = "xrgb8888")]
    Xrgb8888,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitModeInfo {
    width: u32,
    height: u32,
    stride: u32,
    format: WitPixelFormat,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
struct WitRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
// `denied` belongs to gfx.deny's vocabulary; this provider never refuses by policy.
#[allow(dead_code)]
enum WitGfxError {
    #[component(name = "denied")]
    Denied,
    #[component(name = "out-of-bounds")]
    OutOfBounds,
    #[component(name = "bad-buffer")]
    BadBuffer(String),
    #[component(name = "io")]
    Io(String),
}

/// Return shape of the owned-buffer operations (`present` / `read`).
type GfxBufferReturn = (Resource<BufferRes>, core::result::Result<(), WitGfxError>);

// -----------------------------------------------------------------------------------------
// Operation bodies (synchronous device copies under the store access)
// -----------------------------------------------------------------------------------------

/// The gfx.mem `bad-buffer` message, byte for byte (the cross-backend contract covers
/// the error vocabulary too).
fn bad_buffer(rect: &WitRect, needed: u64, held: u64) -> WitGfxError {
    WitGfxError::BadBuffer(format!(
        "the rectangle needs {needed} bytes ({}x{} xrgb8888), the buffer holds {held}",
        rect.width, rect.height
    ))
}

fn rect_of(rect: &WitRect) -> gfxfb::Rect {
    gfxfb::Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// `present`: validate (rect first, then buffer — gfx.mem's precedence), pack each
/// tightly-packed xrgb8888 row to RGB888, and copy it through the device accessors at
/// `base + y*2400 + x*3`.
fn present_impl(
    state: &mut KernelState,
    dst: &WitRect,
    buffer_rep: u32,
) -> Result<core::result::Result<(), WitGfxError>> {
    let base = match surface() {
        Ok(base) => base,
        Err(error) => return Ok(Err(error)),
    };
    let Ok(plan) = gfxfb::check_rect(&rect_of(dst)) else {
        return Ok(Err(WitGfxError::OutOfBounds));
    };
    let shell = state
        .shell
        .as_mut()
        .ok_or_else(|| wasmtime::Error::msg("no shell session state"))?;
    let bytes = shell.buffers.bytes(buffer_rep)?;
    if let Err(needed) = gfxfb::check_buffer(&rect_of(dst), bytes.len() as u64) {
        return Ok(Err(bad_buffer(dst, needed, bytes.len() as u64)));
    }
    let mut scratch = vec![0u8; plan.fb_row_bytes];
    for row in 0..plan.rows {
        gfxfb::pack_xrgb_row(
            &bytes[row * plan.buf_row_bytes..(row + 1) * plan.buf_row_bytes],
            &mut scratch,
        );
        // SAFETY: `base` is the locate-time map-checked surface and the plan keeps the
        // row inside it (check_rect against the fixed 800×480 mode).
        unsafe { crate::simplefb::copy_out(base + plan.fb_start + row * gfxfb::STRIDE, &scratch) };
    }
    if !FIRST_PRESENT.swap(true, Ordering::Relaxed) {
        crate::kprintln!("gfx: first present (mapping=device)");
    }
    Ok(Ok(()))
}

/// `read`: copy each framebuffer row out through the device accessors and unpack it to
/// tightly-packed xrgb8888 (X = 0) straight into the destination buffer.
fn read_impl(
    state: &mut KernelState,
    src: &WitRect,
    buffer_rep: u32,
) -> Result<core::result::Result<(), WitGfxError>> {
    let base = match surface() {
        Ok(base) => base,
        Err(error) => return Ok(Err(error)),
    };
    let Ok(plan) = gfxfb::check_rect(&rect_of(src)) else {
        return Ok(Err(WitGfxError::OutOfBounds));
    };
    let shell = state
        .shell
        .as_mut()
        .ok_or_else(|| wasmtime::Error::msg("no shell session state"))?;
    let bytes = shell.buffers.bytes(buffer_rep)?;
    if let Err(needed) = gfxfb::check_buffer(&rect_of(src), bytes.len() as u64) {
        return Ok(Err(bad_buffer(src, needed, bytes.len() as u64)));
    }
    let mut scratch = vec![0u8; plan.fb_row_bytes];
    for row in 0..plan.rows {
        // SAFETY: as in `present_impl` — the locate-time map check plus check_rect.
        unsafe {
            crate::simplefb::copy_in(base + plan.fb_start + row * gfxfb::STRIDE, &mut scratch)
        };
        gfxfb::unpack_rgb888_row(
            &scratch,
            &mut bytes[row * plan.buf_row_bytes..(row + 1) * plan.buf_row_bytes],
        );
    }
    Ok(Ok(()))
}

/// `clear`: one packed row of the color, copied to every rectangle row (a present of a
/// constant buffer without materializing one).
fn clear_impl(dst: &WitRect, color: u32) -> core::result::Result<(), WitGfxError> {
    let base = surface()?;
    let Ok(plan) = gfxfb::check_rect(&rect_of(dst)) else {
        return Err(WitGfxError::OutOfBounds);
    };
    let pixel = gfxfb::color_rgb888(color);
    let mut scratch = vec![0u8; plan.fb_row_bytes];
    for chunk in scratch.chunks_exact_mut(gfxfb::FB_BPP) {
        chunk.copy_from_slice(&pixel);
    }
    for row in 0..plan.rows {
        // SAFETY: as in `present_impl` — the locate-time map check plus check_rect.
        unsafe { crate::simplefb::copy_out(base + plan.fb_start + row * gfxfb::STRIDE, &scratch) };
    }
    Ok(())
}

// -----------------------------------------------------------------------------------------
// Linker registration
// -----------------------------------------------------------------------------------------

/// Register the `eo9:gfx` root provider: the `types` resource, the full `gfx`
/// interface, and the `gfx-optional` flavor (answering `some` — the capability IS
/// granted when this is linked at all). Only call this when the boot granted gfx
/// ([`granted`]); the capability must never be linked by default.
pub fn add_gfx(linker: &mut Linker<KernelState>) -> Result<()> {
    linker.instance("eo9:gfx/types@0.1.0")?.resource(
        "gfx-impl",
        ResourceType::host::<GfxCap>(),
        |_, _| Ok(()),
    )?;

    let mut interface = linker.instance("eo9:gfx/gfx@0.1.0")?;

    interface.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<GfxCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;

    interface.func_wrap(
        "mode",
        |_store: StoreContextMut<'_, KernelState>,
         (_cap,): (Resource<GfxCap>,)|
         -> Result<(core::result::Result<WitModeInfo, WitGfxError>,)> {
            Ok((surface().map(|_base| WitModeInfo {
                width: gfxfb::WIDTH,
                height: gfxfb::HEIGHT,
                // The surface in the API's pixel model (xrgb8888): width × 4, honoring
                // the WIT's documented `stride >= width * 4`. The packed-RGB888 backing
                // stride (2400) is internal — `present`/`read` buffers are tightly
                // packed and this provider does the stride math.
                stride: gfxfb::WIDTH * gfxfb::BUF_BPP as u32,
                format: WitPixelFormat::Xrgb8888,
            }),))
        },
    )?;

    interface.func_wrap_concurrent(
        "present",
        |accessor: &Accessor<KernelState>,
         (_cap, dst, src): (Resource<GfxCap>, WitRect, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (GfxBufferReturn,)> {
            Box::pin(async move {
                let buffer_rep = src.rep();
                let result = accessor
                    .with(|mut access| present_impl(access.data_mut(), &dst, buffer_rep))?;
                Ok(((Resource::new_own(buffer_rep), result),))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "read",
        |accessor: &Accessor<KernelState>,
         (_cap, src, dst): (Resource<GfxCap>, WitRect, Resource<BufferRes>)|
         -> ConcurrentFuture<'_, (GfxBufferReturn,)> {
            Box::pin(async move {
                let buffer_rep = dst.rep();
                let result =
                    accessor.with(|mut access| read_impl(access.data_mut(), &src, buffer_rep))?;
                Ok(((Resource::new_own(buffer_rep), result),))
            })
        },
    )?;

    interface.func_wrap_concurrent(
        "clear",
        |_accessor: &Accessor<KernelState>,
         (_cap, dst, color): (Resource<GfxCap>, WitRect, u32)|
         -> ConcurrentFuture<'_, (core::result::Result<(), WitGfxError>,)> {
            Box::pin(async move { Ok((clear_impl(&dst, color),)) })
        },
    )?;

    let mut optional = linker.instance("eo9:gfx/gfx-optional@0.1.0")?;
    optional.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Option<Resource<GfxCap>>,)> {
            Ok((Some(Resource::new_own(0)),))
        },
    )?;

    Ok(())
}
