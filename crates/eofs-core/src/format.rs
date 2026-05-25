//! On-disk format: constants and (de)serialization of every stored structure.
//!
//! The authoritative description lives in `FORMAT.md`; this module is its code twin. All
//! integers are little-endian, all structures are fixed-layout byte strings built and parsed
//! by hand (no serde, no `unsafe`, no target-dependent layout).

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::FsError;

/// Magic at the start of each uberblock slot.
pub const MAGIC: [u8; 8] = *b"EOFS-UB\0";

/// On-disk format version.
pub const FORMAT_VERSION: u32 = 1;

/// Size of one uberblock slot in bytes. Fixed regardless of the filesystem block size so a
/// mount can find the slots before it knows anything else.
pub const SLOT_SIZE: u64 = 4096;

/// Byte offsets of the two uberblock slots.
pub const SLOT_OFFSETS: [u64; 2] = [0, SLOT_SIZE];

/// First byte of the data region (everything the allocator hands out lives at or above this).
pub const DATA_START: u64 = 2 * SLOT_SIZE;

/// Serialized size of a [`BlockPtr`].
pub const BLOCK_PTR_SIZE: usize = 56;

/// Serialized size of an [`ObjRef`].
pub const OBJ_REF_SIZE: usize = 72;

/// Serialized size of the checksummed portion of an uberblock (checksum follows it).
pub const UBERBLOCK_BODY_SIZE: usize = 192;

/// Serialized size of an uberblock including its checksum.
pub const UBERBLOCK_SIZE: usize = UBERBLOCK_BODY_SIZE + 32;

/// Fixed-size prefix of a directory entry (the name follows it).
pub const DIR_ENTRY_FIXED: usize = 4 + OBJ_REF_SIZE;

/// Fixed-size prefix of a snapshot-table entry (the name follows it).
pub const SNAP_ENTRY_FIXED: usize = 16 + OBJ_REF_SIZE;

/// Longest accepted file, directory, or snapshot name, in bytes.
pub const MAX_NAME_LEN: usize = 255;

/// Largest accepted *metadata* object (a serialized directory or the snapshot table), in
/// bytes. Metadata objects are read into memory whole, so their size is bounded tightly —
/// independently of the device size — to keep a corrupted or hostile image from driving
/// huge allocations. 16 MiB is room for over two hundred thousand directory entries.
pub const MAX_META_OBJECT_SIZE: u64 = 1 << 24;

/// How a block's bytes are stored on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Stored exactly as the logical bytes.
    Raw,
    /// lz4 block format (`lz4_flex`); physical size is the compressed size.
    Lz4,
}

impl Codec {
    pub fn to_tag(self) -> u8 {
        match self {
            Codec::Raw => 0,
            Codec::Lz4 => 1,
        }
    }

    pub fn from_tag(tag: u8) -> Result<Codec, FsError> {
        match tag {
            0 => Ok(Codec::Raw),
            1 => Ok(Codec::Lz4),
            _ => Err(FsError::Corrupt("unknown codec tag")),
        }
    }
}

/// What a directory entry points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Directory,
}

impl NodeKind {
    fn to_tag(self) -> u8 {
        match self {
            NodeKind::File => 1,
            NodeKind::Directory => 2,
        }
    }

    fn from_tag(tag: u8) -> Result<NodeKind, FsError> {
        match tag {
            1 => Ok(NodeKind::File),
            2 => Ok(NodeKind::Directory),
            _ => Err(FsError::Corrupt("unknown node kind")),
        }
    }
}

/// A pointer to one stored block: where it lives, how big it is logically and physically,
/// how it is encoded, and the blake3 hash of its *logical* (uncompressed) bytes.
///
/// The hash field is what makes the whole filesystem a Merkle tree: indirect blocks and
/// directories contain block pointers, so their own hashes cover their children's hashes,
/// all the way up to the uberblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPtr {
    /// Byte offset of the stored bytes on the device. `0` is never a valid data address
    /// (the uberblock slots live there), so an all-zero pointer means "null".
    pub addr: u64,
    /// Logical (uncompressed) size in bytes.
    pub lsize: u32,
    /// Physical (stored) size in bytes.
    pub psize: u32,
    /// How the stored bytes are encoded.
    pub codec: Codec,
    /// blake3 hash of the logical bytes.
    pub hash: [u8; 32],
}

impl BlockPtr {
    /// The null pointer: no block.
    pub const NULL: BlockPtr = BlockPtr {
        addr: 0,
        lsize: 0,
        psize: 0,
        codec: Codec::Raw,
        hash: [0; 32],
    };

    pub fn is_null(&self) -> bool {
        self.addr == 0
    }

    pub fn write_to(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), BLOCK_PTR_SIZE);
        out[0..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..12].copy_from_slice(&self.lsize.to_le_bytes());
        out[12..16].copy_from_slice(&self.psize.to_le_bytes());
        out[16] = self.codec.to_tag();
        out[17..24].fill(0);
        out[24..56].copy_from_slice(&self.hash);
    }

    pub fn read_from(bytes: &[u8]) -> Result<BlockPtr, FsError> {
        debug_assert_eq!(bytes.len(), BLOCK_PTR_SIZE);
        let addr = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let lsize = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let psize = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let codec = Codec::from_tag(bytes[16])?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[24..56]);
        Ok(BlockPtr {
            addr,
            lsize,
            psize,
            codec,
            hash,
        })
    }
}

/// A reference to a byte object (a file's contents, a serialized directory, the snapshot
/// table): its logical length, the height of its block tree, and the root block pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjRef {
    /// Logical length in bytes.
    pub size: u64,
    /// Number of indirect levels above the data blocks (0 = root points at the single data
    /// block; irrelevant when `size == 0`).
    pub level: u8,
    /// Root of the block tree; null when `size == 0`.
    pub root: BlockPtr,
}

impl ObjRef {
    /// The empty object (zero bytes, no blocks).
    pub const EMPTY: ObjRef = ObjRef {
        size: 0,
        level: 0,
        root: BlockPtr::NULL,
    };

    pub fn write_to(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), OBJ_REF_SIZE);
        out[0..8].copy_from_slice(&self.size.to_le_bytes());
        out[8] = self.level;
        out[9..16].fill(0);
        self.root.write_to(&mut out[16..16 + BLOCK_PTR_SIZE]);
    }

    pub fn read_from(bytes: &[u8]) -> Result<ObjRef, FsError> {
        debug_assert_eq!(bytes.len(), OBJ_REF_SIZE);
        let size = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let level = bytes[8];
        let root = BlockPtr::read_from(&bytes[16..16 + BLOCK_PTR_SIZE])?;
        Ok(ObjRef { size, level, root })
    }
}

/// The uberblock: the root of everything, written alternately to the two fixed slots.
/// The slot with the highest transaction number and a valid checksum wins at mount time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uberblock {
    /// Filesystem block size in bytes (fixed at format time).
    pub block_size: u32,
    /// Allocation granularity in bytes (fixed at format time).
    pub alloc_unit: u32,
    /// Codec applied to newly written blocks (fixed at format time).
    pub codec: Codec,
    /// Transaction number: 1 at format, +1 per commit.
    pub txg: u64,
    /// Allocation frontier (first never-allocated byte) at the time of this commit.
    pub frontier: u64,
    /// Device size recorded at format time.
    pub device_size: u64,
    /// The live directory tree root.
    pub live_root: ObjRef,
    /// The snapshot table object.
    pub snapshots: ObjRef,
}

impl Uberblock {
    /// Serialize into a full zero-padded slot.
    pub fn to_slot_bytes(&self) -> Vec<u8> {
        let mut slot = alloc::vec![0u8; SLOT_SIZE as usize];
        slot[0..8].copy_from_slice(&MAGIC);
        slot[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        slot[12..16].copy_from_slice(&self.block_size.to_le_bytes());
        slot[16..20].copy_from_slice(&self.alloc_unit.to_le_bytes());
        slot[20] = self.codec.to_tag();
        slot[24..32].copy_from_slice(&self.txg.to_le_bytes());
        slot[32..40].copy_from_slice(&self.frontier.to_le_bytes());
        slot[40..48].copy_from_slice(&self.device_size.to_le_bytes());
        self.live_root.write_to(&mut slot[48..48 + OBJ_REF_SIZE]);
        self.snapshots.write_to(&mut slot[120..120 + OBJ_REF_SIZE]);
        let checksum = blake3::hash(&slot[0..UBERBLOCK_BODY_SIZE]);
        slot[UBERBLOCK_BODY_SIZE..UBERBLOCK_SIZE].copy_from_slice(checksum.as_bytes());
        slot
    }

    /// Parse a slot. `Ok(None)` means "not a valid uberblock" (blank, torn, or foreign
    /// bytes) — the caller falls back to the other slot. Errors are reserved for slots that
    /// checksum correctly but still make no sense.
    pub fn from_slot_bytes(slot: &[u8]) -> Result<Option<Uberblock>, FsError> {
        if slot.len() < UBERBLOCK_SIZE || slot[0..8] != MAGIC {
            return Ok(None);
        }
        let checksum = blake3::hash(&slot[0..UBERBLOCK_BODY_SIZE]);
        if checksum.as_bytes() != &slot[UBERBLOCK_BODY_SIZE..UBERBLOCK_SIZE] {
            return Ok(None);
        }
        let version = u32::from_le_bytes(slot[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(FsError::Corrupt("unsupported format version"));
        }
        let block_size = u32::from_le_bytes(slot[12..16].try_into().unwrap());
        let alloc_unit = u32::from_le_bytes(slot[16..20].try_into().unwrap());
        let codec = Codec::from_tag(slot[20])?;
        let txg = u64::from_le_bytes(slot[24..32].try_into().unwrap());
        let frontier = u64::from_le_bytes(slot[32..40].try_into().unwrap());
        let device_size = u64::from_le_bytes(slot[40..48].try_into().unwrap());
        let live_root = ObjRef::read_from(&slot[48..48 + OBJ_REF_SIZE])?;
        let snapshots = ObjRef::read_from(&slot[120..120 + OBJ_REF_SIZE])?;
        Ok(Some(Uberblock {
            block_size,
            alloc_unit,
            codec,
            txg,
            frontier,
            device_size,
            live_root,
            snapshots,
        }))
    }
}

/// One directory entry: a name and the object it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: NodeKind,
    pub obj: ObjRef,
}

/// Serialize a directory: entries sorted by name, each `name_len u16 | kind u8 | 0 | ObjRef
/// | name`. Sorting makes the encoding canonical, so identical directory contents always
/// hash identically.
pub fn serialize_dir(entries: &[DirEntry]) -> Vec<u8> {
    let mut sorted: Vec<&DirEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut out = Vec::new();
    for entry in sorted {
        let name = entry.name.as_bytes();
        debug_assert!(name.len() <= MAX_NAME_LEN);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.push(entry.kind.to_tag());
        out.push(0);
        let mut obj = [0u8; OBJ_REF_SIZE];
        entry.obj.write_to(&mut obj);
        out.extend_from_slice(&obj);
        out.extend_from_slice(name);
    }
    out
}

/// Parse a serialized directory.
pub fn parse_dir(bytes: &[u8]) -> Result<Vec<DirEntry>, FsError> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes.len() - at < DIR_ENTRY_FIXED {
            return Err(FsError::Corrupt("truncated directory entry"));
        }
        let name_len = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
        let kind = NodeKind::from_tag(bytes[at + 2])?;
        let obj = ObjRef::read_from(&bytes[at + 4..at + 4 + OBJ_REF_SIZE])?;
        let name_start = at + DIR_ENTRY_FIXED;
        if name_len > MAX_NAME_LEN || bytes.len() - name_start < name_len {
            return Err(FsError::Corrupt("truncated directory entry name"));
        }
        let name = core::str::from_utf8(&bytes[name_start..name_start + name_len])
            .map_err(|_| FsError::Corrupt("directory entry name is not utf-8"))?;
        entries.push(DirEntry {
            name: String::from(name),
            kind,
            obj,
        });
        at = name_start + name_len;
    }
    Ok(entries)
}

/// One snapshot: a retained root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapEntry {
    /// The transaction the snapshot belongs to (the commit that made/makes it durable).
    pub txg: u64,
    pub name: String,
    /// The live root as it was when the snapshot was taken.
    pub root: ObjRef,
}

/// Serialize the snapshot table: entries in creation order, each
/// `txg u64 | name_len u16 | 0*6 | ObjRef | name`.
pub fn serialize_snapshots(entries: &[SnapEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        let name = entry.name.as_bytes();
        debug_assert!(name.len() <= MAX_NAME_LEN);
        out.extend_from_slice(&entry.txg.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 6]);
        let mut obj = [0u8; OBJ_REF_SIZE];
        entry.root.write_to(&mut obj);
        out.extend_from_slice(&obj);
        out.extend_from_slice(name);
    }
    out
}

/// Parse the snapshot table.
pub fn parse_snapshots(bytes: &[u8]) -> Result<Vec<SnapEntry>, FsError> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes.len() - at < SNAP_ENTRY_FIXED {
            return Err(FsError::Corrupt("truncated snapshot entry"));
        }
        let txg = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let name_len = u16::from_le_bytes(bytes[at + 8..at + 10].try_into().unwrap()) as usize;
        let root = ObjRef::read_from(&bytes[at + 16..at + 16 + OBJ_REF_SIZE])?;
        let name_start = at + SNAP_ENTRY_FIXED;
        if name_len > MAX_NAME_LEN || bytes.len() - name_start < name_len {
            return Err(FsError::Corrupt("truncated snapshot entry name"));
        }
        let name = core::str::from_utf8(&bytes[name_start..name_start + name_len])
            .map_err(|_| FsError::Corrupt("snapshot name is not utf-8"))?;
        entries.push(SnapEntry {
            txg,
            name: String::from(name),
            root,
        });
        at = name_start + name_len;
    }
    Ok(entries)
}
