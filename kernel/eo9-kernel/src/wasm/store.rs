//! The kernel's baked-in, read-only store image.
//!
//! `cargo xtask build-kernel <arch>` collects a fixed set of guest components, host-AOT
//! precompiles each one for the bare-metal target, and packs them into a single image
//! (see `build_store_image` in xtask) that the kernel embeds and parses here. Entries are
//! keyed by their shell name (`hello`, `entropy.seeded`, `eosh`, …) — the same names the
//! usermode store binds when it seeds itself — and carry the original component bytes
//! (served read-only as `/bin/<name>.wasm` to eosh and used for content addressing), the
//! precompiled artifact (what actually runs, since on-target codegen is a later rung), and
//! a plain-text metadata block (the component's `describe` output, computed by xtask at
//! image-assembly time because the kernel has no component parser of its own yet).
//!
//! Format (all little-endian, no alignment requirements):
//! `"EO9STOR2"` magic, `u32` entry count, then per entry: `u16` name length + name bytes,
//! `u32` component length + component bytes, `u32` artifact length + artifact bytes,
//! `u32` metadata length + metadata bytes (UTF-8 text, one record per line:
//! `kind binary|provider`, `import required|optional <slot> <interface> <version>`,
//! `export <name> <interface> <version>`, `arg <name> <wit type text…>`).

use alloc::vec::Vec;

/// Magic bytes introducing the store image.
const MAGIC: &[u8; 8] = b"EO9STOR2";

/// Upper bound on the declared entry count, checked before any allocation sized by it.
/// Today's images carry well under a dozen entries; the cap only exists so a corrupt or
/// hostile image cannot force a huge up-front allocation.
const MAX_ENTRIES: u32 = 1024;

/// One named component in the baked-in store.
pub struct StoreEntry {
    /// The shell name (`hello`, `time.frozen`, `eosh`, …).
    pub name: &'static str,
    /// The original component bytes (component-model `.wasm`).
    pub component: &'static [u8],
    /// The host-AOT artifact for this machine, loadable via `Component::deserialize`.
    pub artifact: &'static [u8],
    /// The component's metadata block (its `describe` output, precomputed by xtask).
    pub metadata: &'static str,
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
        if count > MAX_ENTRIES {
            return Err("store image: entry count is implausibly large");
        }
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
            let (metadata_len, r) = read_u32(r).ok_or("store image: truncated metadata length")?;
            let (metadata, r) =
                split(r, metadata_len as usize).ok_or("store image: truncated metadata")?;
            let metadata = core::str::from_utf8(metadata)
                .map_err(|_| "store image: metadata is not UTF-8")?;
            entries.push(StoreEntry {
                name,
                component,
                artifact,
                metadata,
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

    /// Parse once and hand back a `'static` view of the entries (the backing image is
    /// already `'static`; the entry list itself lives for the rest of the boot). Used by
    /// the shell session, whose store data and host functions need to reference the
    /// entries without owning the `StoreImage`.
    pub fn parse_static(image: &'static [u8]) -> Result<&'static [StoreEntry], &'static str> {
        let store = StoreImage::parse(image)?;
        Ok(alloc::boxed::Box::leak(store.entries.into_boxed_slice()))
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
