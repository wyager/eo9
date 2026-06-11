//! Minimal flattened-device-tree (FDT) reader: just enough to find `/chosen/bootargs`.
//!
//! The boot protocol hands the DTB address to `kmain` (aarch64: `x0` from QEMU's loader
//! or whatever the board's loader left there; riscv64: `a1` from OpenSBI). The kernel
//! command line (`-append "…"`) lands in the `bootargs` property of the `/chosen` node,
//! which is what program selection reads (plan/12-kernel.md). Everything else in the
//! tree is ignored, and any malformed or missing structure simply yields `None` — the
//! kernel then boots its default program.
//!
//! **The x0 choke point** ([`validate`], usb-boot A1 hardening): the boot pointer is
//! dereferenced ONLY after it passes null/alignment/window/header validation, decided
//! ONCE per boot in [`bootargs`]. The three live shapes of x0 on the board:
//!
//! * the serial path passes the real control-FDT address (~0xEB9F_xxxx) — validates,
//! * the kexec jump passes a deliberate 0 — rejected loudly, staged bootargs win,
//! * U-Boot's `go` invokes the entry as a C call, so **x0 = argc (a small integer)** —
//!   rejected loudly *before any read*. The pre-hardening code probed junk x0 as a
//!   plain cmdline string, and dereferencing x0=1 walked into the secure bottom MiB
//!   of DRAM (TF-A/OP-TEE behind the DDR firewall), which stalls the interconnect with
//!   no exception — the silent 22 s watchdog boot loop of USB-boot round A1.

/// FDT header magic (big-endian on the wire).
const FDT_MAGIC: u32 = 0xd00d_feed;
/// FDT header length in bytes (also the smallest believable `totalsize`).
const FDT_HEADER_LEN: usize = 40;
/// Token: begin node (followed by a NUL-terminated name, padded to 4 bytes).
const FDT_BEGIN_NODE: u32 = 1;
/// Token: end node.
const FDT_END_NODE: u32 = 2;
/// Token: property (u32 len, u32 name offset, then `len` bytes padded to 4).
const FDT_PROP: u32 = 3;
/// Token: no-op.
const FDT_NOP: u32 = 4;
/// Token: end of the structure block.
const FDT_END: u32 = 9;
/// Upper bound on a believable DTB size (QEMU's is ~1 MiB); rejects a corrupt
/// `totalsize` before anything walks (or sweeps) that many bytes.
#[cfg(not(feature = "board-opi5plus"))]
const MAX_FDT_SIZE: u32 = 16 * 1024 * 1024;
/// Tighter bound for the board's control FDT (measured ~170 KiB on the Orange Pi 5 Plus;
/// 1 MiB is a generous ceiling). The shadow path *cache-sweeps and copies* `totalsize`
/// bytes, so its cap must stay small enough that a corrupt-but-magic-valid header cannot
/// turn boot into a multi-second sweep of garbage addresses.
#[cfg(feature = "board-opi5plus")]
const MAX_FDT_SIZE: u32 = 1024 * 1024;

/// Address windows a boot-handed FDT may legitimately occupy; [`validate`] dereferences
/// x0 ONLY inside one of these (half-open `[lo, hi)`, and `totalsize` must fit before
/// `hi` too). Everything else — small integers, MMIO, unmapped holes, the board's
/// secure DRAM — is rejected without a single read.
#[cfg(all(target_arch = "aarch64", feature = "board-opi5plus"))]
const FDT_WINDOWS: &[(usize, usize)] = &[
    // Non-secure low DRAM (identity-mapped Normal): from the staged-bootargs page up to
    // the top of the kernel's DRAM window. The bottom MiB is deliberately OUT: TF-A owns
    // it (BL31 at 0x0004_0000) behind the DDR firewall, and a non-secure read there
    // stalls the interconnect with no exception — dereferencing `go`'s argc (x0=1) did
    // exactly that (the USB-boot A1 infinite boot loop).
    (crate::mmu::BOOTARGS_PAGE, crate::mmu::RAM_END),
    // U-Boot's runtime DRAM behind the fourth-GiB Device mapping (its control FDT lives
    // ~0xEB9F_xxxx, relocated U-Boot below it). Capped at 0xF000_0000: above that sit
    // live RK3588 peripherals — a junk pointer must never turn into register reads.
    (0xC000_0000, 0xF000_0000),
];
#[cfg(all(target_arch = "aarch64", not(feature = "board-opi5plus")))]
const FDT_WINDOWS: &[(usize, usize)] = &[(crate::mmu::RAM_BASE, crate::mmu::RAM_END)];
#[cfg(target_arch = "riscv64")]
const FDT_WINDOWS: &[(usize, usize)] = &[(crate::mmu::RAM_BASE, crate::mmu::RAM_END)];
/// x86_64 (PVH) has no device tree at all; [`bootargs`] never reaches [`validate`] there,
/// and the empty window set keeps the shared code honest if it ever did.
#[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
const FDT_WINDOWS: &[(usize, usize)] = &[];

/// The board's fourth-GiB Device window base: a validated FDT at or above this address is
/// read through a non-cacheable Device mapping and must be byte-volatile-copied into the
/// heap before the slice-based walker touches it (see [`validate`]).
#[cfg(feature = "board-opi5plus")]
const DEVICE_WINDOW_BASE: usize = 0xC000_0000;

/// What replaces the FDT when x0 is rejected — the tail of the one loud line.
#[cfg(feature = "board-opi5plus")]
const ABSENT_PLAN: &str = "using the staged bootargs page + baked board constants";
#[cfg(all(target_arch = "aarch64", not(feature = "board-opi5plus")))]
const ABSENT_PLAN: &str = "probing the RAM-base DTB";
#[cfg(not(any(target_arch = "aarch64", feature = "board-opi5plus")))]
const ABSENT_PLAN: &str = "no FDT fallback on this machine - booting the default program";

/// Fallback probe address for when the boot protocol did not hand the DTB address over.
/// aarch64 QEMU `virt` always places its DTB at the base of RAM; on riscv64 OpenSBI always
/// passes the address in `a1`, so there is no fixed fallback (a null probe yields `None`).
/// The Orange Pi 5 Plus board profile must NOT probe it either: 0x4000_0000 is *unmapped*
/// there (the identity map covers DRAM 0..0x2100_0000 and the fourth-GiB device window
/// only), so the probe would be a guaranteed translation fault whenever the real FDT
/// yields no bootargs — a boot that should degrade to the default program would die.
#[cfg(all(target_arch = "aarch64", not(feature = "board-opi5plus")))]
const FALLBACK_DTB: *const u8 = 0x4000_0000 as *const u8;
#[cfg(not(all(target_arch = "aarch64", not(feature = "board-opi5plus"))))]
const FALLBACK_DTB: *const u8 = core::ptr::null();

/// Return the kernel command line, if present.
///
/// x86_64 (PVH) hands a plain NUL-terminated command-line pointer, never a device tree,
/// so it goes straight to the bounded string probe. Every device-tree architecture goes
/// through [`validate`] — the single x0 choke point — and on rejection the FDT is ABSENT
/// for the whole boot: one loud line states why and what replaces it, and the pointer is
/// never dereferenced. The fallbacks, in order: the RAM-base DTB probe (QEMU aarch64
/// only — the machine always places one there), then the board's staged-bootargs page.
/// A valid x0 device tree always wins (the serial path is unchanged).
pub fn bootargs(dtb: *const u8) -> Option<&'static str> {
    // `cfg!` rather than `#[cfg]` so every architecture type-checks all of this function.
    if cfg!(target_arch = "x86_64") {
        return cmdline_at(dtb);
    }
    let found = match validate(dtb) {
        Ok(fdt) => bootargs_at(fdt),
        Err(why) => {
            crate::kprintln!(
                "fdt: x0={:#x} is not an FDT - {why} - {ABSENT_PLAN}",
                dtb as usize
            );
            None
        }
    };
    let found = found.or_else(|| bootargs_at(FALLBACK_DTB));
    // Board profile: the staged-bootargs page fallback (usb-boot-demo-plan.md Part A,
    // Option 1) — LAST, so a valid x0 device tree always wins. Loud when it supplies
    // the line: the transcript must say where the cmdline came from.
    #[cfg(feature = "board-opi5plus")]
    let found = found.or_else(|| {
        let staged = staged_bootargs();
        if let Some(args) = staged {
            crate::kprintln!(
                "bootargs: staged page {:#x} -> {args:?}",
                crate::mmu::BOOTARGS_PAGE
            );
        }
        staged
    });
    found
}

/// THE x0 choke point: decide whether the boot pointer is a device tree, dereferencing
/// it only after every no-read check passes. Any future FDT consumer (an INTID lookup,
/// a memory-node read, …) MUST route through here rather than touching x0 itself —
/// rejection means the FDT is absent for the whole boot.
///
/// Checks, in order (the first four read nothing):
/// 1. non-null (the kexec jump passes a deliberate 0),
/// 2. 8-aligned (the devicetree spec's placement rule; `go`'s argc fails here),
/// 3. inside one of [`FDT_WINDOWS`] (mapped DRAM only — never MMIO, never the board's
///    secure bottom MiB, never an unmapped hole),
/// 4. (board) the header's cache lines swept to PoC before a byte is trusted,
/// 5. FDT magic present, read byte-volatile (single-byte loads are legal on both the
///    Normal and Device mappings),
/// 6. `totalsize` within `[40, MAX_FDT_SIZE]` *and* fitting inside the same window.
///
/// On the board, a candidate in the fourth-GiB Device window is then swept to PoC and
/// byte-volatile-copied into the heap (U-Boot's `fdt set` edits can still sit in its
/// dirty D-cache lines, and the slice walker's merged loads would alignment-fault on
/// Device memory — both proven live on the board, 2026-06-07); the heap shadow is what
/// the walker gets. A low-DRAM candidate is swept in place for the same staleness
/// reason. Runs after `heap::init` (kmain order), so allocation is available; the copy
/// is leaked (one-time boot cost, and the parser hands out `&'static str` slices into it).
pub(crate) fn validate(dtb: *const u8) -> Result<*const u8, &'static str> {
    if dtb.is_null() {
        return Err("null (the kexec jump's deliberate x0=0)");
    }
    let addr = dtb as usize;
    if !addr.is_multiple_of(8) {
        return Err("not 8-aligned (junk such as U-Boot go's argc)");
    }
    let Some(&(_, window_end)) = FDT_WINDOWS
        .iter()
        .find(|&&(lo, hi)| (lo..hi).contains(&addr))
    else {
        return Err("outside every mapped DRAM window (junk such as U-Boot go's argc)");
    };
    // The header's own lines may be stale (another agent's dirty D-cache); sweep the
    // 8 bytes holding magic+totalsize before trusting either.
    #[cfg(feature = "board-opi5plus")]
    crate::mmu::clean_invalidate_to_poc(addr, 8);
    // SAFETY: bounded single-byte volatile reads inside a mapped DRAM window (checked
    // above); byte loads are legal on both the Normal and Device mappings.
    let byte = |i: usize| unsafe { core::ptr::read_volatile(dtb.add(i)) };
    let be = |i: usize| u32::from_be_bytes([byte(i), byte(i + 1), byte(i + 2), byte(i + 3)]);
    if be(0) != FDT_MAGIC {
        return Err("no FDT magic (non-FDT DRAM bytes)");
    }
    let totalsize = be(4);
    if !(FDT_HEADER_LEN as u32..=MAX_FDT_SIZE).contains(&totalsize) {
        return Err("FDT magic but an unbelievable totalsize");
    }
    let totalsize = totalsize as usize;
    match addr.checked_add(totalsize) {
        Some(end) if end <= window_end => {}
        _ => return Err("FDT magic but totalsize runs past the mapped window"),
    }
    #[cfg(feature = "board-opi5plus")]
    {
        // Now that totalsize is trustworthy and bounded, push the whole tree out to DRAM
        // so the reads below (and the walker's) see the writer's bytes, not stale lines.
        crate::mmu::clean_invalidate_to_poc(addr, totalsize);
        if addr >= DEVICE_WINDOW_BASE {
            // Device-window candidate (the serial path's control FDT): copy into the
            // heap with byte-volatile reads and hand the walker the Normal-memory shadow.
            let mut copy = alloc::vec::Vec::with_capacity(totalsize);
            for i in 0..totalsize {
                copy.push(byte(i));
            }
            return Ok(alloc::boxed::Box::leak(copy.into_boxed_slice()).as_ptr());
        }
    }
    Ok(dtb)
}

/// Board profile: read the staged-bootargs page at 0x0010_0000 (the
/// `mmu::BOOTARGS_PAGE` reservation, below the image). Format: one printable-ASCII
/// command line, terminated by NUL, newline or carriage return (BOOTARGS.TXT may carry
/// a trailing `\n` or `\r\n` — both are line terminators, not failures), bounded by the
/// page — the bounded first-line parse defends against warm-reset DRAM residue (random
/// bytes fail the printable check; an empty line yields `None`). The page is swept to
/// the point of coherency first: the writer may have been U-Boot's `fatload` (DMA,
/// already at PoC) or the previous kernel's cached stores (the kexec dance sweeps too —
/// this is the reader's matching belt-and-braces half).
#[cfg(feature = "board-opi5plus")]
fn staged_bootargs() -> Option<&'static str> {
    use crate::mmu::{BOOTARGS_PAGE, BOOTARGS_PAGE_LEN};
    crate::mmu::clean_invalidate_to_poc(BOOTARGS_PAGE, BOOTARGS_PAGE_LEN);
    let page = BOOTARGS_PAGE as *const u8;
    let mut len = 0;
    while len < BOOTARGS_PAGE_LEN {
        // SAFETY: the page is identity-mapped Normal RAM inside the DRAM window,
        // reserved below the image (mmu.rs); bounded byte-volatile reads only.
        let byte = unsafe { core::ptr::read_volatile(page.add(len)) };
        if byte == 0 || byte == b'\n' || byte == b'\r' {
            break;
        }
        if !(0x20..=0x7e).contains(&byte) {
            return None;
        }
        len += 1;
    }
    if len == 0 || len >= BOOTARGS_PAGE_LEN {
        return None;
    }
    // Copy out of the page (it may be rewritten by a later kexec staging) and leak the
    // copy — the same one-time boot cost as the control-FDT shadow in `validate`.
    let mut copy = alloc::vec::Vec::with_capacity(len);
    for i in 0..len {
        // SAFETY: as above; `i < len < BOOTARGS_PAGE_LEN`.
        copy.push(unsafe { core::ptr::read_volatile(page.add(i)) });
    }
    let leaked: &'static [u8] = alloc::boxed::Box::leak(copy.into_boxed_slice());
    // Printable ASCII (checked above) is valid UTF-8.
    core::str::from_utf8(leaked).ok()
}

/// Upper bound on a believable plain command line (QEMU's `-append` is far shorter).
const MAX_CMDLINE: usize = 4096;

/// Treat `ptr` as a NUL-terminated command-line string — the x86_64 PVH boot protocol's
/// format, and the ONLY caller is the x86_64 arm of [`bootargs`] (the PVH pointer comes
/// from firmware and points at identity-mapped low RAM). Returns `None` for a null
/// pointer, an empty string, anything unreasonably long, or bytes outside printable
/// ASCII. The device-tree architectures must never reach this with a junk boot pointer:
/// on the board, probing x0=1 here is what walked into the secure bottom MiB of DRAM
/// and hung USB-boot round A1 (`validate` now rejects junk before any read).
fn cmdline_at(ptr: *const u8) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while len < MAX_CMDLINE {
        // SAFETY: the boot protocol hands a readable, NUL-terminated string in
        // identity-mapped RAM; reads stop at the terminator or the size bound.
        // Plain volatile is correct: this is RAM, not device MMIO (crate::mmio is
        // for device registers only).
        let byte = unsafe { core::ptr::read_volatile(ptr.add(len)) };
        if byte == 0 {
            break;
        }
        if !(0x20..=0x7e).contains(&byte) {
            return None;
        }
        len += 1;
    }
    if len == 0 || len >= MAX_CMDLINE {
        return None;
    }
    // SAFETY: the bytes were just validated as printable ASCII (hence valid UTF-8) and the
    // backing memory is never modified or freed for the lifetime of the kernel.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    core::str::from_utf8(bytes).ok()
}

/// [`bootargs`] for one candidate DTB address. Returns `None` for a null pointer, a
/// missing/garbled tree, or a missing property.
///
/// The walk is structurally bounded even on a valid-magic-but-corrupt blob: every read
/// goes through [`be32`]/[`cstr`], which bounds-check against the `totalsize`-long
/// slice, and every token arm advances `offset` by at least 4 bytes — so the cursor
/// either reaches a terminating condition or runs off the end of the slice and yields
/// `None`. No unbounded pointer chasing exists for garbage to exploit.
fn bootargs_at(dtb: *const u8) -> Option<&'static str> {
    if dtb.is_null() || !(dtb as usize).is_multiple_of(4) {
        return None;
    }
    // SAFETY: the header is 40 bytes; we only trust it after the magic and size checks
    // below, and all subsequent reads are bounded by `totalsize`. Callers pass either a
    // `validate`d pointer (already header-checked — these checks are cheap defense in
    // depth) or the QEMU profile's fixed RAM-base probe address.
    let header = unsafe { core::slice::from_raw_parts(dtb, FDT_HEADER_LEN) };
    if be32(header, 0)? != FDT_MAGIC {
        return None;
    }
    let totalsize = be32(header, 4)?;
    if !(FDT_HEADER_LEN as u32..=MAX_FDT_SIZE).contains(&totalsize) {
        return None;
    }
    let off_dt_struct = be32(header, 8)? as usize;
    let off_dt_strings = be32(header, 12)? as usize;
    // SAFETY: bounded by `totalsize`, which we just sanity-checked; the DTB sits in
    // identity-mapped RAM for the whole run (the kernel never moves or frees it).
    let fdt = unsafe { core::slice::from_raw_parts(dtb, totalsize as usize) };

    let mut offset = off_dt_struct;
    let mut depth: u32 = 0;
    let mut in_chosen = false;
    loop {
        let token = be32(fdt, offset)?;
        offset += 4;
        match token {
            FDT_BEGIN_NODE => {
                let name = cstr(fdt, offset)?;
                offset += align4(name.len() + 1);
                depth += 1;
                in_chosen = depth == 2 && name == b"chosen";
            }
            FDT_END_NODE => {
                if in_chosen {
                    // Left /chosen without finding bootargs.
                    return None;
                }
                depth = depth.checked_sub(1)?;
            }
            FDT_PROP => {
                let len = be32(fdt, offset)? as usize;
                let name_off = be32(fdt, offset + 4)? as usize;
                let value_start = offset + 8;
                let value = fdt.get(value_start..value_start.checked_add(len)?)?;
                offset = value_start + align4(len);
                if in_chosen && cstr(fdt, off_dt_strings + name_off)? == b"bootargs" {
                    // The property value is NUL-terminated; trim it and require UTF-8.
                    let value = value.strip_suffix(&[0]).unwrap_or(value);
                    return core::str::from_utf8(value).ok();
                }
            }
            FDT_NOP => {}
            FDT_END => return None,
            _ => return None,
        }
    }
}

/// Big-endian u32 at `offset`, bounds-checked.
fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
}

/// The NUL-terminated byte string starting at `offset` (exclusive of the NUL).
fn cstr(bytes: &[u8], offset: usize) -> Option<&[u8]> {
    let rest = bytes.get(offset..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    Some(&rest[..len])
}

/// Round `len` up to the FDT's 4-byte alignment.
fn align4(len: usize) -> usize {
    len.div_ceil(4) * 4
}

/// The junk-x0 boot matrix (the `x0matrix` boot token; xtask `check-x0` drives it under
/// QEMU). QEMU's `-kernel` loader always passes a valid DTB in x0, so the field failure
/// shapes are replayed in-kernel through the SAME [`validate`] choke point the board
/// boots through: x0 = 0 (kexec), 1/2 (U-Boot `go`'s argc), 8 (aligned junk), an
/// unaligned pointer, aligned DRAM bytes with no magic, a valid-magic header declaring
/// an insane totalsize, and a truncated tree (valid magic+totalsize, structure that
/// runs off the end). Every case must come back — bounded, no hang — with exactly the
/// canonical absent-x0 recovery (on QEMU aarch64 the RAM-base DTB probe, i.e. the live
/// cmdline; on the board the staged page), printing the loud rejection line on the way.
pub fn x0_matrix_selftest(live: Option<&'static str>) {
    use alloc::vec::Vec;

    // The canonical absent-x0 recovery: what a boot with no usable x0 must land on.
    let expected = bootargs(core::ptr::null());
    crate::kprintln!("fdt-x0-matrix: live x0 parse {live:?}; absent-x0 recovery {expected:?}");

    // 8-aligned heap blobs (u64 backing guarantees the alignment `validate` requires,
    // so each case fails for the reason under test, not an earlier check).
    let to_words = |be_u32s: &[u32]| -> Vec<u64> {
        let mut raw: Vec<u8> = be_u32s.iter().flat_map(|w| w.to_be_bytes()).collect();
        while !raw.len().is_multiple_of(8) {
            raw.push(0);
        }
        raw.chunks(8)
            .map(|c| u64::from_ne_bytes(c.try_into().unwrap()))
            .collect()
    };
    // Aligned DRAM garbage: no FDT magic anywhere.
    let garbage: Vec<u64> = alloc::vec![0xA5A5_A5A5_A5A5_A5A5; 64];
    // Valid magic, totalsize far beyond the believable cap.
    let insane = to_words(&[FDT_MAGIC, 0xFFFF_FFF0, 0, 0, 0, 17, 16, 0, 0, 0]);
    // Truncated/corrupt: valid magic, sane totalsize (64), structure at offset 40 holding
    // six NOPs and never an FDT_END — the walker's totalsize-bounded cursor must run off
    // the end and yield None (the structurally-bounded-walk guarantee, exercised).
    let corrupt = to_words(&[
        FDT_MAGIC, 64, 40, 64, 0, 17, 16, 0, 0, 0, // header
        FDT_NOP, FDT_NOP, FDT_NOP, FDT_NOP, FDT_NOP, FDT_NOP, // structure, no END
    ]);

    let garbage_addr = garbage.as_ptr() as usize;
    let cases: [(&str, usize); 8] = [
        ("null-kexec", 0),
        ("go-argc-1", 1),
        ("go-argc-2", 2),
        ("aligned-low-8", 8),
        ("unaligned-junk", garbage_addr + 1),
        ("dram-garbage", garbage_addr),
        ("insane-totalsize", insane.as_ptr() as usize),
        ("corrupt-fdt", corrupt.as_ptr() as usize),
    ];
    let mut pass = true;
    for (name, addr) in cases {
        let got = bootargs(addr as *const u8);
        let ok = got == expected;
        pass &= ok;
        crate::kprintln!(
            "fdt-x0-matrix: case {name} x0={addr:#x} -> {got:?} ({})",
            if ok { "ok" } else { "MISMATCH" }
        );
    }
    crate::kprintln!(
        "fdt-x0-matrix: {} ({} cases)",
        if pass { "PASS" } else { "FAIL" },
        cases.len()
    );
}
