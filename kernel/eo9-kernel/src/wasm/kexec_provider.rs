//! Kernel-side root provider for `eo9:kexec` — stage a new kernel image in reserved RAM
//! and jump into it (network kexec; aarch64 only).
//!
//! **TOTAL AUTHORITY.** A holder of this capability replaces the running operating
//! system: `stage` writes arbitrary bytes into the staging region and a verified
//! `commit` hands them the CPU. There is no attenuation that makes that safe, so the
//! provider is **never linked by default** — it exists only behind the `kexec` boot
//! token (the `pci`/`platform`/`gfx` grant grammar, `runner::boot`), and even then the
//! loader rule applies: only a program that imports `eo9:kexec` links it.
//!
//! **The staging region** is the top 64 MiB of the DRAM window, carved out of the heap
//! at the memory-map level (`mmu::KEXEC_*` — QEMU `virt`: stub 0x5C00_0000, staging
//! 0x5C01_0000..0x6000_0000; Orange Pi 5 Plus: stub 0x1D00_0000, staging
//! 0x1D01_0000..0x2100_0000). Because it is outside the allocator, no other capability
//! can reach it: heap allocations, DMA buffers, JIT pages, and gfx/platform windows all
//! live elsewhere. (A bus-mastering PCI device could still DMA anywhere — the
//! machine-wide no-IOMMU posture `pci_provider` documents; the `kexec` token is in the
//! same total-authority class as `pci` for exactly that reason.)
//!
//! **The dance** (`commit`, after the CRC verifies — every step before the final asm
//! block is refusal-free):
//!  1. print the final `kexec: jumping to the staged image (N bytes, crc ok)` line;
//!  2. quiesce: clear bus mastering on every PCI function the machine enumerates
//!     (revoking every DMA licence — the per-task teardown discipline, applied
//!     machine-wide because the machine itself is about to end);
//!  3. write `bootargs` to the staged-bootargs page (USB-boot Option-1 format: one
//!     printable line, NUL-terminated; `fdt::staged_bootargs` is the board reader);
//!  4. pat the watchdog (board) — the dance is far quicker than the 22.4 s timeout and
//!     the new kernel re-arms at its own boot;
//!  5. copy the relocation stub into its reserved slot and sweep the staged image, the
//!     stub, and the bootargs page to the point of coherency while caches are still on;
//!  6. final asm block (no stack, no data accesses): mask interrupts, sweep the *target*
//!     window to PoC (evicting every dirty line the running kernel holds over its own
//!     image/heap — without this, the new kernel's own entry `dc civac` sweep would
//!     write stale lines back OVER the freshly copied bytes), drop the I-cache, switch
//!     MMU/D-cache/I-cache off, and branch to the stub;
//!  7. the stub (position-independent, ~14 instructions) copies staging → target with
//!     uncached 16-byte moves (stores land straight in DRAM — already at PoC), drops
//!     the I-cache again, and branches to the entry with x0 = 0 — deliberate junk, so
//!     the new kernel's bootargs fall through to the staged page (board) or the
//!     still-intact DTB at the RAM base (QEMU). x0-carrying loaders (the serial path)
//!     are unaffected.
//!
//! **First-instructions hazard** (the usb-boot-demo-plan §A.2 reasoning): the new
//! image's entry bytes must reach both instruction fetch and uncached data reads.
//! Here the copy itself runs with SCTLR.{M,C,I} = 0, so every store goes to DRAM (the
//! PoC) by construction — there is nothing left above the PoC to lose. The only stale
//! state is the *old* kernel's I-cache contents over the target addresses, which the
//! `ic iallu` in both the final asm block and the stub remove. The board kernel's
//! entry additionally self-sweeps its whole footprint (boot.rs), making the image
//! self-coherent no matter the loader — the same belt-and-braces as the serial path.
//!
//! **Loop safety** (board): a wild jump after a good CRC — a bad image that passes its
//! own checksum — is recovered by the hardware watchdog: ~22 s of silence, then reset
//! to U-Boot and the serial loader, exactly the serial path's recovery story. The
//! network never makes the bench less recoverable than the wire it replaces.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};

use wasmtime::component::{Accessor, ComponentType, Lift, Linker, Lower, Resource, ResourceType};
use wasmtime::{Result, StoreContextMut};

use super::providers::KernelState;
use crate::mmu::{KEXEC_STAGING_BASE, KEXEC_STAGING_LEN, KEXEC_STUB_BASE, KEXEC_STUB_LEN};

/// Boxed future shape for `func_wrap_concurrent` closures (same alias as the other
/// kernel providers).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

// -----------------------------------------------------------------------------------------
// Boot-time grant
// -----------------------------------------------------------------------------------------

/// Whether this boot granted the kexec capability (the bare `kexec` kernel command-line
/// token).
static KEXEC_GRANTED: AtomicBool = AtomicBool::new(false);

/// Record the boot-time grant decision (called once from `runner::boot`).
pub fn set_granted(granted: bool) {
    KEXEC_GRANTED.store(granted, Ordering::Relaxed);
}

/// Whether linkers built for this boot should include the `eo9:kexec` root provider.
pub fn granted() -> bool {
    KEXEC_GRANTED.load(Ordering::Relaxed)
}

// -----------------------------------------------------------------------------------------
// Profile constants: where the new image runs
// -----------------------------------------------------------------------------------------

/// Where the staged image is copied and entered: the kernel link address (entry at
/// offset 0 — the board image opens with the arm64 header whose `code0` branches to the
/// entry; the QEMU image's `.text.boot` puts `_start` first).
#[cfg(feature = "board-opi5plus")]
const TARGET_BASE: usize = 0x0020_0000;
#[cfg(not(feature = "board-opi5plus"))]
const TARGET_BASE: usize = 0x4020_0000;

/// Image length ceiling. Board: the structural 62 MiB cap (image at 0x0020_0000 must
/// stay below the serial-loader stub home at 0x0400_0000 — the loop-safety recovery
/// path's territory). QEMU: the staging region's own capacity.
#[cfg(feature = "board-opi5plus")]
const TARGET_CAP: usize = 0x0400_0000 - TARGET_BASE;
#[cfg(not(feature = "board-opi5plus"))]
const TARGET_CAP: usize = KEXEC_STAGING_LEN;

// -----------------------------------------------------------------------------------------
// WIT-shaped types
// -----------------------------------------------------------------------------------------

/// Host representation of `eo9:kexec/types.kexec-impl` (stateless token; the staging
/// region is the state).
struct KexecCap;

#[derive(Clone, ComponentType, Lift, Lower)]
#[component(variant)]
// `denied` belongs to a future deny stub's vocabulary; `unsupported` to the non-aarch64
// ports. This provider answers neither today, but the WIT error is the contract.
#[allow(dead_code)]
enum WitKexecError {
    #[component(name = "denied")]
    Denied,
    #[component(name = "out-of-range")]
    OutOfRange(String),
    #[component(name = "crc-mismatch")]
    CrcMismatch(String),
    #[component(name = "bad-image")]
    BadImage(String),
    #[component(name = "unsupported")]
    Unsupported,
    #[component(name = "io")]
    Io(String),
}

// -----------------------------------------------------------------------------------------
// CRC-32 (IEEE, reflected) — the serial-loader protocol's checksum, table-driven so a
// 50+ MiB commit verifies in well under a second even under TCG.
// -----------------------------------------------------------------------------------------

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = (c >> 8) ^ CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize];
    }
    !c
}

// -----------------------------------------------------------------------------------------
// Operation bodies
// -----------------------------------------------------------------------------------------

/// `stage`: bounds-check, then copy the chunk into the staging region. All-or-nothing:
/// a write that would cross the region's end writes no byte.
fn stage_impl(offset: u64, chunk: &[u8]) -> core::result::Result<(), WitKexecError> {
    let len = chunk.len() as u64;
    let capacity = KEXEC_STAGING_LEN as u64;
    let end = offset.checked_add(len).ok_or_else(|| {
        WitKexecError::OutOfRange(format!("offset {offset} + {len} bytes overflows"))
    })?;
    if end > capacity {
        return Err(WitKexecError::OutOfRange(format!(
            "offset {offset} + {len} bytes exceeds the {capacity}-byte staging region"
        )));
    }
    if !chunk.is_empty() {
        // SAFETY: [KEXEC_STAGING_BASE, +KEXEC_STAGING_LEN) is identity-mapped Normal RAM
        // reserved out of the heap for exactly this writer (mmu.rs); the bounds were
        // checked above and the source is an ordinary guest-lifted slice.
        unsafe {
            core::ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                (KEXEC_STAGING_BASE + offset as usize) as *mut u8,
                chunk.len(),
            );
        }
    }
    Ok(())
}

/// `commit`: verify, then jump. Returns only on refusal — every check happens before
/// any side effect, so a refused commit leaves the machine exactly as it was.
fn commit_impl(len: u64, crc_expected: u32, bootargs: &str) -> WitKexecError {
    // ---- verification (refusal-free zone: no side effects yet) ----------------------
    if len == 0 {
        return WitKexecError::BadImage("a zero-length image cannot boot".into());
    }
    if len > KEXEC_STAGING_LEN as u64 || len > TARGET_CAP as u64 {
        return WitKexecError::OutOfRange(format!(
            "{len} bytes exceeds the image ceiling ({} staged / {} at the target)",
            KEXEC_STAGING_LEN, TARGET_CAP
        ));
    }
    let len = len as usize;
    // The reservation puts staging above the target window by construction; assert the
    // copy ranges are disjoint anyway (the stub's forward copy requires it).
    let target_end = TARGET_BASE + len;
    if target_end > KEXEC_STUB_BASE {
        return WitKexecError::OutOfRange(format!(
            "the target window {TARGET_BASE:#x}..{target_end:#x} would overlap the \
             reserved kexec region at {KEXEC_STUB_BASE:#x}"
        ));
    }
    if bootargs.len() >= crate::mmu::BOOTARGS_PAGE_LEN {
        return WitKexecError::BadImage(format!(
            "bootargs are {} bytes; the staged page holds {}",
            bootargs.len(),
            crate::mmu::BOOTARGS_PAGE_LEN - 1
        ));
    }
    if bootargs
        .bytes()
        .any(|byte| !(0x20..=0x7e).contains(&byte))
    {
        return WitKexecError::BadImage(
            "bootargs must be one printable-ASCII line (the staged-page format)".into(),
        );
    }
    // SAFETY: the staging region is identity-mapped Normal RAM; `len` was bounded above.
    let staged = unsafe { core::slice::from_raw_parts(KEXEC_STAGING_BASE as *const u8, len) };
    let crc_actual = crc32(staged);
    if crc_actual != crc_expected {
        return WitKexecError::CrcMismatch(format!(
            "staged bytes crc {crc_actual:08x}, commit said {crc_expected:08x} — \
             nothing was touched; re-stage and commit again"
        ));
    }

    // ---- the point of no return ------------------------------------------------------
    crate::kprintln!("kexec: jumping to the staged image ({len} bytes, crc ok)");

    // Quiesce: revoke every PCI function's DMA licence (bus mastering off, machine-wide
    // — root ports included on the board, which also stops forwarding). The new kernel
    // re-enables what its own drivers claim.
    let mut cleared = 0usize;
    for function in crate::pci::enumerate() {
        if crate::pci::set_bus_master(function.address, false) {
            cleared += 1;
        }
    }
    crate::kprintln!("kexec: bus mastering cleared on {cleared} PCI function(s)");

    // The staged-bootargs page (Option-1 format: the line, NUL-terminated).
    {
        let page = crate::mmu::BOOTARGS_PAGE as *mut u8;
        // SAFETY: the page is identity-mapped Normal RAM below the kernel image,
        // reserved for exactly this handoff (mmu.rs); length checked above.
        unsafe {
            core::ptr::copy_nonoverlapping(bootargs.as_ptr(), page, bootargs.len());
            core::ptr::write_volatile(page.add(bootargs.len()), 0);
        }
    }

    // The watchdog gap across the jump is the dance below (sub-second sweeps + an
    // uncached copy, well under a second per 50 MiB) against the 22.4 s timeout; the
    // new kernel re-arms at its own 'G' milestone. Pat once so the window starts full.
    crate::wdt::pat();

    // Copy the relocation stub into its reserved slot.
    let stub_len = {
        unsafe extern "C" {
            static __kexec_stub_start: u8;
            static __kexec_stub_end: u8;
        }
        let start = (&raw const __kexec_stub_start).addr();
        let end = (&raw const __kexec_stub_end).addr();
        let stub_len = end - start;
        debug_assert!(stub_len <= KEXEC_STUB_LEN);
        // SAFETY: the stub slot is the reserved region's first 64 KiB (mmu.rs); the
        // source is the kernel's own text.
        unsafe {
            core::ptr::copy_nonoverlapping(
                start as *const u8,
                KEXEC_STUB_BASE as *mut u8,
                stub_len,
            );
        }
        stub_len
    };

    // Push the bytes the post-MMU world will read out to the point of coherency while
    // the caches are still on: the staged image (the stub's uncached reads), the stub
    // itself (uncached instruction fetch), and the bootargs page (the next kernel's
    // uncached read — its reader sweeps too; this is the writer's half).
    crate::mmu::clean_invalidate_to_poc(KEXEC_STAGING_BASE, len);
    crate::mmu::clean_invalidate_to_poc(KEXEC_STUB_BASE, stub_len);
    crate::mmu::clean_invalidate_to_poc(crate::mmu::BOOTARGS_PAGE, crate::mmu::BOOTARGS_PAGE_LEN);

    let len16 = (len + 15) & !15;
    // SAFETY: the final transfer — interrupts masked, the target window swept (so no
    // dirty line of the dying kernel can write back over the copied image), caches and
    // MMU off, then the stub. The asm block touches no memory (registers only) between
    // the sweep and the branch; every address it takes is identity-mapped.
    unsafe {
        jump_to_stub(
            KEXEC_STUB_BASE,
            KEXEC_STAGING_BASE,
            TARGET_BASE,
            len16,
            TARGET_BASE,
        )
    }
}

/// The no-return tail of the dance. See the module docs for the step-by-step rationale.
///
/// # Safety
/// Ends the operating system. `stub` must hold the relocation stub at the point of
/// coherency; `src`/`dst`/`len16` must describe disjoint, 16-aligned identity-mapped
/// ranges with `src` already at PoC; `entry` must be the new image's entry.
unsafe fn jump_to_stub(stub: usize, src: usize, dst: usize, len16: usize, entry: usize) -> ! {
    unsafe {
        core::arch::asm!(
            // No surprises from here: every interrupt masked.
            "msr daifset, #0xf",
            // Sweep the whole target window to PoC: clean+invalidate every line the
            // running kernel holds over [dst, dst+len) — its own image, stack, heap —
            // so nothing dirty can ever write back over the new image. After this
            // loop the block touches no memory until the stub runs.
            "mrs x2, ctr_el0",
            "ubfx x2, x2, #16, #4",
            "mov x3, #4",
            "lsl x2, x3, x2", // D-cache line size in bytes
            "sub x3, x2, #1",
            "bic x4, x11, x3", // cursor: dst aligned down to a line
            "add x5, x11, x12", // limit: dst + len16
            "2:",
            "dc civac, x4",
            "add x4, x4, x2",
            "cmp x4, x5",
            "b.lo 2b",
            "dsb sy",
            // Drop the I-cache (stale lines over both the target window and the stub
            // slot), then switch translation and both caches off.
            "ic iallu",
            "dsb sy",
            "isb",
            "mrs x4, sctlr_el1",
            "movz x3, #0x1005", // M (bit 0) | C (bit 2) | I (bit 12)
            "bic x4, x4, x3",
            "msr sctlr_el1, x4",
            "isb",
            "br x14",
            in("x10") src,
            in("x11") dst,
            in("x12") len16,
            in("x13") entry,
            in("x14") stub,
            options(noreturn),
        )
    }
}

// The relocation stub: position-independent (register moves and one backward branch),
// assembled into the kernel image and *copied* to its reserved slot — never executed in
// place. Runs with MMU and caches off: the 16-byte copy moves DRAM to DRAM directly
// (the source was swept to PoC; the stores need no sweep), the `ic iallu` removes any
// stale instruction lines over the copied range, and the entry receives the Linux boot
// register contract with x0 = 0 (deliberate junk — the staged-bootargs fallback).
core::arch::global_asm!(
    r#"
    .section .text.kexecstub, "ax"
    .globl __kexec_stub_start
__kexec_stub_start:
1:  ldp     x2, x3, [x10], #16
    stp     x2, x3, [x11], #16
    subs    x12, x12, #16
    b.gt    1b
    dsb     sy
    ic      iallu
    dsb     sy
    isb
    mov     x0, xzr
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    br      x13
    .globl __kexec_stub_end
__kexec_stub_end:
"#
);

// -----------------------------------------------------------------------------------------
// Linker registration
// -----------------------------------------------------------------------------------------

/// Register the `eo9:kexec` root provider: the `types` resource, the full `kexec`
/// interface, and the `kexec-optional` flavor (answering `some` — the capability IS
/// granted when this is linked at all). Only call this when the boot granted kexec
/// ([`granted`]); the capability is total authority and must never be linked by
/// default.
pub fn add_kexec(linker: &mut Linker<KernelState>) -> Result<()> {
    linker.instance("eo9:kexec/types@0.1.0")?.resource(
        "kexec-impl",
        ResourceType::host::<KexecCap>(),
        |_, _| Ok(()),
    )?;

    let mut interface = linker.instance("eo9:kexec/kexec@0.1.0")?;

    interface.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<KexecCap>,)> {
            Ok((Resource::new_own(0),))
        },
    )?;

    interface.func_wrap_concurrent(
        "stage",
        |_accessor: &Accessor<KernelState>,
         (_cap, offset, chunk): (Resource<KexecCap>, u64, Vec<u8>)|
         -> ConcurrentFuture<'_, (core::result::Result<(), WitKexecError>,)> {
            Box::pin(async move { Ok((stage_impl(offset, &chunk),)) })
        },
    )?;

    interface.func_wrap_concurrent(
        "commit",
        |_accessor: &Accessor<KernelState>,
         (_cap, len, crc, bootargs): (Resource<KexecCap>, u64, u32, String)|
         -> ConcurrentFuture<'_, (core::result::Result<(), WitKexecError>,)> {
            Box::pin(async move {
                // A verified commit never comes back from `commit_impl`.
                Ok((Err(commit_impl(len, crc, &bootargs)),))
            })
        },
    )?;

    let mut optional = linker.instance("eo9:kexec/kexec-optional@0.1.0")?;
    optional.func_wrap(
        "default",
        |_store: StoreContextMut<'_, KernelState>,
         (): ()|
         -> Result<(Option<Resource<KexecCap>>,)> { Ok((Some(Resource::new_own(0)),)) },
    )?;

    Ok(())
}
