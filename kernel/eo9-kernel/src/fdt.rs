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

/// Fallback probe address for when the boot protocol did not hand the DTB address over.
/// aarch64 QEMU `virt` always places its DTB at the base of RAM; on riscv64 OpenSBI always
/// passes the address in `a1`, so there is no fixed fallback (a null probe yields `None`).
#[cfg(target_arch = "aarch64")]
const FALLBACK_DTB: *const u8 = 0x4000_0000 as *const u8;
#[cfg(not(target_arch = "aarch64"))]
const FALLBACK_DTB: *const u8 = core::ptr::null();

/// Return the `/chosen/bootargs` string from the device tree, if present.
///
/// Tries the address the boot protocol passed first, then falls back to probing the
/// architecture's fixed DTB location, if it has one (always validated by the FDT magic and
/// size checks before anything is read).
pub fn bootargs(dtb: *const u8) -> Option<&'static str> {
    bootargs_at(dtb).or_else(|| bootargs_at(FALLBACK_DTB))
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
