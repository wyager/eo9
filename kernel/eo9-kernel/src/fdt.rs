//! Minimal flattened-device-tree (FDT) reader: just enough to find `/chosen/bootargs`.
//!
//! The boot protocol hands the DTB address to `kmain` (aarch64: `x0` from QEMU's loader;
//! riscv64: `a1` from OpenSBI). The kernel command line (`-append "…"`) lands in the `bootargs`
//! property of the `/chosen` node, which is what program selection reads
//! (plan/12-kernel.md). Everything else in the tree is ignored, and any malformed or
//! missing structure simply yields `None` — the kernel then boots its default program.

/// FDT header magic (big-endian on the wire).
const FDT_MAGIC: u32 = 0xd00d_feed;
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
/// Upper bound on a believable DTB size (QEMU's is ~1 MiB); guards the initial copy of
/// the header fields against a garbage pointer.
const MAX_FDT_SIZE: u32 = 16 * 1024 * 1024;
/// Tighter bound for the board's control FDT (measured ~170 KiB on the Orange Pi 5 Plus;
/// 1 MiB is a generous ceiling). The shadow path *cache-sweeps and copies* `totalsize`
/// bytes, so its cap must stay small enough that a corrupt-but-magic-valid header cannot
/// turn boot into a multi-second sweep of garbage addresses.
#[cfg(feature = "board-opi5plus")]
const MAX_BOARD_FDT_SIZE: u32 = 1024 * 1024;

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
/// Tries the address the boot protocol passed as a device tree first (`/chosen/bootargs`),
/// then the architecture's fixed DTB location, if it has one — always validated by the FDT
/// magic and size checks before anything is read. If neither holds a device tree, the
/// pointer is finally tried as a plain NUL-terminated command-line string, which is what
/// the x86_64 PVH boot path hands the kernel instead of a DTB; on the device-tree
/// architectures that fallback is unreachable in practice (their boot pointer is always a
/// valid FDT).
pub fn bootargs(dtb: *const u8) -> Option<&'static str> {
    #[cfg(feature = "board-opi5plus")]
    let dtb = shadow_device_fdt(dtb);
    let found = bootargs_at(dtb)
        .or_else(|| bootargs_at(FALLBACK_DTB))
        .or_else(|| cmdline_at(dtb));
    // Board profile: the staged-bootargs page fallback (usb-boot-demo-plan.md Part A,
    // Option 1) — LAST, so a valid x0 device tree always wins (the serial path is
    // unchanged). Only an x0 that yielded nothing (USB `go`'s junk argc, the kexec
    // jump's deliberate 0) reaches the page.
    #[cfg(feature = "board-opi5plus")]
    let found = found.or_else(staged_bootargs);
    found
}

/// Board profile: read the staged-bootargs page at 0x0010_0000 (the
/// `mmu::BOOTARGS_PAGE` reservation, below the image). Format: one printable-ASCII
/// command line, terminated by NUL or newline, bounded by the page — the bounded
/// first-line parse defends against warm-reset DRAM residue (random bytes fail the
/// printable check; an empty line yields `None`). The page is swept to the point of
/// coherency first: the writer may have been U-Boot's `fatload` (DMA, already at PoC)
/// or the previous kernel's cached stores (the kexec dance sweeps too — this is the
/// reader's matching belt-and-braces half).
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
        if byte == 0 || byte == b'\n' {
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
    // copy — the same one-time boot cost as the control-FDT shadow above.
    let mut copy = alloc::vec::Vec::with_capacity(len);
    for i in 0..len {
        // SAFETY: as above; `i < len < BOOTARGS_PAGE_LEN`.
        copy.push(unsafe { core::ptr::read_volatile(page.add(i)) });
    }
    let leaked: &'static [u8] = alloc::boxed::Box::leak(copy.into_boxed_slice());
    // Printable ASCII (checked above) is valid UTF-8.
    core::str::from_utf8(leaked).ok()
}

/// Board profile: if the FDT pointer lands outside the identity-mapped Normal-RAM window
/// (U-Boot's control FDT, ~0xEB9F_xxxx, lives under the fourth-GiB *Device* mapping),
/// cache-sweep it to the Point of Coherency, then copy it into the heap with byte-volatile
/// reads and parse the copy instead.
///
/// Why the sweep (PROVEN live on the board, 2026-06-07): U-Boot edits this FDT in place
/// (`fdt set /chosen bootargs …`) through its own *cacheable* mapping, and those writes
/// can still be sitting in dirty D-cache lines when it jumps away — DRAM still holds the
/// pre-edit bytes. This kernel reads the same physical bytes through its *Device*
/// (non-cacheable) window, which goes straight to DRAM, so it saw a `/chosen` with no
/// `bootargs` at all; the interim bench workaround was a `crc32 0x10000000 0x1000000`
/// cache-pressure eviction in U-Boot before `go`. The durable fix: `dc civac` by VA
/// operates on the physical address behind the translation, so sweeping through our
/// Device VAs evicts exactly the lines U-Boot dirtied under its cacheable mapping of the
/// same PAs ([`crate::mmu::clean_invalidate_to_poc`] documents the mechanism). Order
/// matters: the *header itself* may be stale, so `totalsize` cannot be trusted until its
/// own line is swept — sweep the first 8 header bytes, read magic+totalsize byte-volatile,
/// bound-check them (1 MiB cap), and only then sweep the full `[dtb, dtb+totalsize)`
/// range and copy.
///
/// Why the copy: the shared walker reads the tree through ordinary slices, and the
/// compiler is free to merge or vectorise those loads — fine on Normal memory, but an
/// unaligned multi-byte access on Device-nGnRnE memory takes an alignment fault.
/// Byte-volatile reads are pinned to single-byte loads, which Device memory always
/// allows. Every header field is sanity-checked before the sweep/copy (magic, totalsize
/// cap), and any failure simply returns the original pointer for the normal
/// validate-and-reject path — never a hang. Runs after `heap::init` (kmain order), so
/// allocation is available; the copy is leaked (one-time boot cost, and the parser hands
/// out `&'static str` slices into it).
#[cfg(feature = "board-opi5plus")]
fn shadow_device_fdt(dtb: *const u8) -> *const u8 {
    let addr = dtb as usize;
    if dtb.is_null() || !addr.is_multiple_of(4) || addr < crate::mmu::HEAP_END {
        return dtb;
    }
    // Evict any dirty lines over the header before trusting a single byte of it
    // (magic at +0, totalsize at +4 — 8 bytes, one or two lines).
    crate::mmu::clean_invalidate_to_poc(addr, 8);
    // SAFETY: bounded byte-volatile reads of the firmware-provided FDT; the fourth-GiB
    // device window is identity-mapped, and U-Boot keeps its control FDT alive there.
    let byte = |i: usize| unsafe { core::ptr::read_volatile(dtb.add(i)) };
    let be = |i: usize| u32::from_be_bytes([byte(i), byte(i + 1), byte(i + 2), byte(i + 3)]);
    if be(0) != FDT_MAGIC {
        return dtb;
    }
    let totalsize = be(4);
    if !(40..=MAX_BOARD_FDT_SIZE).contains(&totalsize) {
        return dtb;
    }
    if addr.checked_add(totalsize as usize).is_none() {
        return dtb;
    }
    // Now that totalsize is trustworthy and bounded, push the whole tree out to DRAM so
    // the byte-volatile copy below reads U-Boot's edits, not the stale pre-edit bytes.
    crate::mmu::clean_invalidate_to_poc(addr, totalsize as usize);
    let mut copy = alloc::vec::Vec::with_capacity(totalsize as usize);
    for i in 0..totalsize as usize {
        copy.push(byte(i));
    }
    alloc::boxed::Box::leak(copy.into_boxed_slice()).as_ptr()
}

/// Upper bound on a believable plain command line (QEMU's `-append` is far shorter).
const MAX_CMDLINE: usize = 4096;

/// Treat `ptr` as a NUL-terminated command-line string (the x86_64 PVH boot protocol's
/// format). Returns `None` for a null pointer, an empty string, anything unreasonably long,
/// or bytes outside printable ASCII — so a garbage pointer cannot be misread as arguments.
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
fn bootargs_at(dtb: *const u8) -> Option<&'static str> {
    if dtb.is_null() || !(dtb as usize).is_multiple_of(4) {
        return None;
    }
    // SAFETY: the header is 40 bytes; we only trust it after the magic and size checks
    // below, and all subsequent reads are bounded by `totalsize`.
    let header = unsafe { core::slice::from_raw_parts(dtb, 40) };
    if be32(header, 0)? != FDT_MAGIC {
        return None;
    }
    let totalsize = be32(header, 4)?;
    if !(40..=MAX_FDT_SIZE).contains(&totalsize) {
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
