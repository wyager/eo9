//! The kernel's baked-in, read-only store image.
//!
//! `cargo xtask build-kernel <arch>` collects a fixed set of guest components, host-AOT
//! precompiles each one for the bare-metal target, and packs them into a single image
//! (see `build_store_image` in xtask) that the kernel embeds and parses here. Entries are
//! keyed by their shell name (`hello`, `entropy.seeded`, `eosh`, …) — the same names the
//! usermode store binds when it seeds itself — and carry both the original component
//! bytes (for the filesystem `/bin` view and content addressing later) and the
//! precompiled artifact (what actually runs, since on-target codegen is a later rung).
//!
//! Format (all little-endian, no alignment requirements):
//! `"EO9STOR1"` magic, `u32` entry count, then per entry: `u16` name length + name bytes,
//! `u32` component length + component bytes, `u32` artifact length + artifact bytes.

use alloc::vec::Vec;

/// Magic bytes introducing the store image.
const MAGIC: &[u8; 8] = b"EO9STOR1";

/// One named component in the baked-in store.
pub struct StoreEntry {
    /// The shell name (`hello`, `time.frozen`, `eosh`, …).
    pub name: &'static str,
    /// The original component bytes (component-model `.wasm`).
    pub component: &'static [u8],
    /// The host-AOT artifact for this machine, loadable via `Component::deserialize`.
    pub artifact: &'static [u8],
}

/// The parsed store image: a flat list of named entries.
pub struct StoreImage {
    entries: Vec<StoreEntry>,
}

impl StoreImage {
    /// Parse the embedded image. Errors name the first structural problem found.
    pub fn parse(image: &'static [u8]) -> Result<StoreImage, &'static str> {
        let rest = image
            .strip_prefix(MAGIC.as_slice())
            .ok_or("store image: bad magic")?;
        let (count, mut rest) = read_u32(rest).ok_or("store image: truncated entry count")?;
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let (name_len, r) = read_u16(rest).ok_or("store image: truncated name length")?;
            let (name, r) = split(r, name_len as usize).ok_or("store image: truncated name")?;
            let name = core::str::from_utf8(name).map_err(|_| "store image: name is not UTF-8")?;
            let (component_len, r) =
                read_u32(r).ok_or("store image: truncated component length")?;
            let (component, r) =
                split(r, component_len as usize).ok_or("store image: truncated component")?;
            let (artifact_len, r) = read_u32(r).ok_or("store image: truncated artifact length")?;
            let (artifact, r) =
                split(r, artifact_len as usize).ok_or("store image: truncated artifact")?;
            entries.push(StoreEntry {
                name,
                component,
                artifact,
            });
            rest = r;
        }
        if !rest.is_empty() {
            return Err("store image: trailing bytes after the last entry");
        }
        Ok(StoreImage { entries })
    }

    /// Look an entry up by its shell name.
    pub fn find(&self, name: &str) -> Option<&StoreEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// All entries, in image order.
    pub fn entries(&self) -> &[StoreEntry] {
        &self.entries
    }
}

fn split(bytes: &'static [u8], len: usize) -> Option<(&'static [u8], &'static [u8])> {
    if bytes.len() < len {
        return None;
    }
    Some(bytes.split_at(len))
}

fn read_u16(bytes: &'static [u8]) -> Option<(u16, &'static [u8])> {
    let (raw, rest) = split(bytes, 2)?;
    Some((u16::from_le_bytes([raw[0], raw[1]]), rest))
}

fn read_u32(bytes: &'static [u8]) -> Option<(u32, &'static [u8])> {
    let (raw, rest) = split(bytes, 4)?;
    Some((u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]), rest))
}
