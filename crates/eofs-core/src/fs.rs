//! The eofs engine: format, mount, the namespace operations, transactions, snapshots,
//! verification, and garbage collection.
//!
//! Everything is copy-on-write: an operation writes new blocks for whatever it changes
//! (data blocks, the file's indirect tree, and every directory from the file up to the
//! root) and leaves all previously written blocks untouched. The new tree only becomes the
//! filesystem when [`Eofs::commit`] writes a new uberblock; until then a crash or a remount
//! simply falls back to the last committed root. See `FORMAT.md`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::device::BlockDevice;
use crate::error::FsError;
use crate::format::{
    BLOCK_PTR_SIZE, BlockPtr, Codec, DATA_START, DirEntry, MAX_NAME_LEN, NodeKind, ObjRef,
    SLOT_OFFSETS, SLOT_SIZE, SnapEntry, Uberblock, parse_dir, parse_snapshots, serialize_dir,
    serialize_snapshots,
};
use crate::space::{Allocator, Extent};

/// Options for [`Eofs::format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOptions {
    /// Filesystem (logical) block size in bytes: a power of two between 512 bytes and 1 MiB.
    pub block_size: u32,
    /// Allocation granularity in bytes: a power of two between 64 and 4096, at most the
    /// block size. Compressed blocks occupy a whole number of allocation units.
    pub alloc_unit: u32,
    /// Compress newly written blocks with lz4 (incompressible blocks fall back to raw).
    pub compression: bool,
}

impl Default for FormatOptions {
    fn default() -> FormatOptions {
        FormatOptions {
            block_size: 4096,
            alloc_unit: 512,
            compression: true,
        }
    }
}

/// What [`Eofs::stat`] reports about a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStat {
    pub kind: NodeKind,
    /// Logical size in bytes (for a directory: the size of its serialized entry list).
    pub size: u64,
    /// The node's Merkle root hash: the blake3 hash of its root block, which transitively
    /// covers all of its content (and, for a directory, all of its descendants). All zeros
    /// for an empty file or empty directory.
    pub hash: [u8; 32],
}

/// One entry of [`Eofs::snapshot_list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub name: String,
    /// The transaction that made (or will make) the snapshot durable.
    pub txg: u64,
    /// Merkle root hash of the snapshot's directory tree.
    pub root_hash: [u8; 32],
}

/// What [`Eofs::verify`] found while walking every reachable block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Blocks read and checked against their pointers (data + indirect).
    pub blocks: u64,
    /// Logical bytes across those blocks.
    pub logical_bytes: u64,
    /// Physical bytes those blocks occupy on the device (allocation-unit rounded).
    pub physical_bytes: u64,
    /// How many of the blocks are lz4-compressed.
    pub compressed_blocks: u64,
    pub files: u64,
    pub directories: u64,
    pub snapshots: u64,
}

/// What [`Eofs::gc`] reclaimed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Bytes below the allocation frontier that no retained root references; they are now
    /// available for reuse by this mount.
    pub reclaimed_bytes: u64,
    /// Number of free extents found.
    pub free_extents: usize,
}

/// Space accounting for the current mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceReport {
    /// First never-allocated byte.
    pub frontier: u64,
    /// Bytes on the allocator's free list (populated by [`Eofs::gc`]).
    pub free_bytes: u64,
    /// Device capacity in bytes.
    pub device_size: u64,
}

/// A mounted eofs filesystem over a block device.
pub struct Eofs<D: BlockDevice> {
    pub(crate) dev: D,
    pub(crate) block_size: u32,
    pub(crate) alloc_unit: u32,
    pub(crate) codec: Codec,
    /// Device capacity as seen by this mount.
    pub(crate) device_size: u64,
    /// Device size recorded at format time (written back into every uberblock).
    format_device_size: u64,
    committed_txg: u64,
    committed_live_root: ObjRef,
    committed_snapshots: ObjRef,
    /// The pending (possibly uncommitted) roots.
    live_root: ObjRef,
    snapshots: ObjRef,
    pub(crate) alloc: Allocator,
    dirty: bool,
}

/// A read-only view of one snapshot.
pub struct SnapshotView<'a, D: BlockDevice> {
    fs: &'a Eofs<D>,
    root: ObjRef,
}

/// An edit applied to one directory entry somewhere under the root.
enum DirOp<'a> {
    Insert {
        name: &'a str,
        kind: NodeKind,
        obj: ObjRef,
    },
    Replace {
        name: &'a str,
        obj: ObjRef,
    },
    Remove {
        name: &'a str,
    },
}

impl<D: BlockDevice> Eofs<D> {
    // --- format & mount ----------------------------------------------------------------

    /// Create a fresh filesystem on `dev` and mount it. The initial (empty) state is
    /// committed as transaction 1 before this returns.
    pub fn format(dev: D, opts: &FormatOptions) -> Result<Eofs<D>, FsError> {
        if !opts.block_size.is_power_of_two() || !(512..=1 << 20).contains(&opts.block_size) {
            return Err(FsError::InvalidConfig("block_size"));
        }
        if !opts.alloc_unit.is_power_of_two()
            || !(64..=4096).contains(&opts.alloc_unit)
            || opts.alloc_unit > opts.block_size
        {
            return Err(FsError::InvalidConfig("alloc_unit"));
        }
        let device_size = dev.size();
        if device_size < DATA_START + 4 * opts.block_size as u64 {
            return Err(FsError::InvalidConfig("device too small"));
        }
        let mut fs = Eofs {
            dev,
            block_size: opts.block_size,
            alloc_unit: opts.alloc_unit,
            codec: if opts.compression {
                Codec::Lz4
            } else {
                Codec::Raw
            },
            device_size,
            format_device_size: device_size,
            committed_txg: 0,
            committed_live_root: ObjRef::EMPTY,
            committed_snapshots: ObjRef::EMPTY,
            live_root: ObjRef::EMPTY,
            snapshots: ObjRef::EMPTY,
            alloc: Allocator::new(opts.alloc_unit as u64, device_size, DATA_START),
            dirty: true,
        };
        // Clear both uberblock slots so stale uberblocks from a previous filesystem can
        // never win the mount-time election.
        fs.dev.write_at(0, &[0u8; (2 * SLOT_SIZE) as usize])?;
        fs.commit()?;
        Ok(fs)
    }

    /// Mount an existing filesystem: read both uberblock slots and adopt the valid one with
    /// the highest transaction number.
    pub fn mount(dev: D) -> Result<Eofs<D>, FsError> {
        let device_size = dev.size();
        if device_size < DATA_START {
            return Err(FsError::Corrupt("device too small to hold an eofs image"));
        }
        let mut best: Option<Uberblock> = None;
        let mut deferred: Option<FsError> = None;
        for offset in SLOT_OFFSETS {
            let mut slot = vec![0u8; SLOT_SIZE as usize];
            dev.read_at(offset, &mut slot)?;
            match Uberblock::from_slot_bytes(&slot) {
                Ok(Some(ub)) => {
                    if best.as_ref().is_none_or(|b| ub.txg > b.txg) {
                        best = Some(ub);
                    }
                }
                Ok(None) => {}
                Err(err) => deferred = Some(err),
            }
        }
        let Some(ub) = best else {
            return Err(deferred.unwrap_or(FsError::Corrupt("no valid uberblock")));
        };
        if !ub.block_size.is_power_of_two() || !(512..=1 << 20).contains(&ub.block_size) {
            return Err(FsError::Corrupt("uberblock block size"));
        }
        if !ub.alloc_unit.is_power_of_two()
            || !(64..=4096).contains(&ub.alloc_unit)
            || ub.alloc_unit > ub.block_size
        {
            return Err(FsError::Corrupt("uberblock allocation unit"));
        }
        if ub.device_size > device_size || ub.frontier > device_size || ub.frontier < DATA_START {
            return Err(FsError::Corrupt("device smaller than the filesystem on it"));
        }
        Ok(Eofs {
            dev,
            block_size: ub.block_size,
            alloc_unit: ub.alloc_unit,
            codec: ub.codec,
            device_size,
            format_device_size: ub.device_size,
            committed_txg: ub.txg,
            committed_live_root: ub.live_root,
            committed_snapshots: ub.snapshots,
            live_root: ub.live_root,
            snapshots: ub.snapshots,
            alloc: Allocator::new(ub.alloc_unit as u64, device_size, ub.frontier),
            dirty: false,
        })
    }

    /// Give the device back. Uncommitted changes are discarded (they were never part of the
    /// on-disk filesystem to begin with).
    pub fn unmount(self) -> D {
        self.dev
    }

    /// Filesystem block size in bytes.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Whether new writes are lz4-compressed.
    pub fn compression(&self) -> bool {
        self.codec == Codec::Lz4
    }

    /// The last committed transaction number.
    pub fn txg(&self) -> u64 {
        self.committed_txg
    }

    /// Whether there are uncommitted changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Space accounting for this mount.
    pub fn space(&self) -> SpaceReport {
        SpaceReport {
            frontier: self.alloc.frontier(),
            free_bytes: self.alloc.free_bytes(),
            device_size: self.device_size,
        }
    }

    // --- transactions --------------------------------------------------------------------

    /// Commit every change made since the last commit: flush the device, write the next
    /// uberblock into the slot the previous commit did *not* use, and flush again. Returns
    /// the new transaction number (or the current one if there was nothing to commit).
    pub fn commit(&mut self) -> Result<u64, FsError> {
        if !self.dirty {
            return Ok(self.committed_txg);
        }
        // Everything the new root references must be durable before the root flip.
        self.dev.flush()?;
        let txg = self.committed_txg + 1;
        let ub = Uberblock {
            block_size: self.block_size,
            alloc_unit: self.alloc_unit,
            codec: self.codec,
            txg,
            frontier: self.alloc.frontier(),
            device_size: self.format_device_size,
            live_root: self.live_root,
            snapshots: self.snapshots,
        };
        let slot = SLOT_OFFSETS[(txg % 2) as usize];
        self.dev.write_at(slot, &ub.to_slot_bytes())?;
        self.dev.flush()?;
        self.committed_txg = txg;
        self.committed_live_root = self.live_root;
        self.committed_snapshots = self.snapshots;
        self.dirty = false;
        Ok(txg)
    }

    // --- namespace operations --------------------------------------------------------------

    /// Create an empty file.
    pub fn create_file(&mut self, path: &str) -> Result<(), FsError> {
        self.create_node(path, NodeKind::File)
    }

    /// Create an empty directory.
    pub fn mkdir(&mut self, path: &str) -> Result<(), FsError> {
        self.create_node(path, NodeKind::Directory)
    }

    fn create_node(&mut self, path: &str, kind: NodeKind) -> Result<(), FsError> {
        let segments = split_path(path)?;
        let Some((name, parent)) = segments.split_last() else {
            return Err(FsError::InvalidPath);
        };
        let name = *name;
        let root = self.live_root;
        let op = DirOp::Insert {
            name,
            kind,
            obj: ObjRef::EMPTY,
        };
        self.live_root = self.apply_in_dir(&root, parent, &op)?;
        self.dirty = true;
        Ok(())
    }

    /// Write `data` into a file at byte `offset`, growing it (zero-filling any gap) if the
    /// write reaches past the current end.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), FsError> {
        let segments = split_path(path)?;
        let Some((name, parent)) = segments.split_last() else {
            return Err(FsError::IsADirectory);
        };
        let name = *name;
        let root = self.live_root;
        let (kind, obj) = self.resolve(&root, &segments)?;
        if kind != NodeKind::File {
            return Err(FsError::IsADirectory);
        }
        let new_obj = self.write_object_range(&obj, offset, data)?;
        let op = DirOp::Replace { name, obj: new_obj };
        self.live_root = self.apply_in_dir(&root, parent, &op)?;
        self.dirty = true;
        Ok(())
    }

    /// Read from a file at byte `offset` into `buf`; returns the number of bytes read
    /// (short only at end-of-file).
    pub fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.read_at_root(&self.live_root, path, offset, buf)
    }

    /// The entry names of a directory, in name order.
    pub fn list(&self, path: &str) -> Result<Vec<String>, FsError> {
        self.list_at_root(&self.live_root, path)
    }

    /// Kind, size, and Merkle root hash of a node.
    pub fn stat(&self, path: &str) -> Result<NodeStat, FsError> {
        self.stat_at_root(&self.live_root, path)
    }

    /// Remove a file or an empty directory.
    pub fn remove(&mut self, path: &str) -> Result<(), FsError> {
        let segments = split_path(path)?;
        let Some((name, parent)) = segments.split_last() else {
            return Err(FsError::InvalidPath);
        };
        let name = *name;
        let root = self.live_root;
        let (kind, obj) = self.resolve(&root, &segments)?;
        if kind == NodeKind::Directory && obj.size != 0 {
            return Err(FsError::DirectoryNotEmpty);
        }
        let op = DirOp::Remove { name };
        self.live_root = self.apply_in_dir(&root, parent, &op)?;
        self.dirty = true;
        Ok(())
    }

    // --- snapshots -------------------------------------------------------------------------

    /// Retain the filesystem exactly as it is right now under `name`. Like every other
    /// change, the snapshot becomes durable at the next [`commit`](Eofs::commit).
    pub fn snapshot_create(&mut self, name: &str) -> Result<(), FsError> {
        check_name(name)?;
        let snapshots = self.snapshots;
        let mut entries = parse_snapshots(&self.read_object(&snapshots)?)?;
        if entries.iter().any(|entry| entry.name == name) {
            return Err(FsError::AlreadyExists);
        }
        entries.push(SnapEntry {
            txg: self.committed_txg + 1,
            name: String::from(name),
            root: self.live_root,
        });
        let bytes = serialize_snapshots(&entries);
        self.snapshots = self.write_object(&bytes)?;
        self.dirty = true;
        Ok(())
    }

    /// All snapshots, in creation order.
    pub fn snapshot_list(&self) -> Result<Vec<SnapshotInfo>, FsError> {
        let entries = parse_snapshots(&self.read_object(&self.snapshots)?)?;
        Ok(entries
            .into_iter()
            .map(|entry| SnapshotInfo {
                name: entry.name,
                txg: entry.txg,
                root_hash: entry.root.root.hash,
            })
            .collect())
    }

    /// A read-only view of one snapshot.
    pub fn snapshot(&self, name: &str) -> Result<SnapshotView<'_, D>, FsError> {
        let entries = parse_snapshots(&self.read_object(&self.snapshots)?)?;
        let entry = entries
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(FsError::NotFound)?;
        Ok(SnapshotView {
            fs: self,
            root: entry.root,
        })
    }

    // --- verification ------------------------------------------------------------------------

    /// Walk every reachable block — the live tree, the snapshot table, and every snapshot's
    /// tree — re-reading each one and checking it against the blake3 hash in its pointer.
    pub fn verify(&self) -> Result<VerifyReport, FsError> {
        let mut report = VerifyReport::default();
        self.verify_dir_tree(&self.live_root, &mut report)?;
        let snapshots = self.snapshots;
        self.verify_object(&snapshots, &mut report)?;
        for entry in parse_snapshots(&self.read_object(&snapshots)?)? {
            report.snapshots += 1;
            self.verify_dir_tree(&entry.root, &mut report)?;
        }
        Ok(report)
    }

    fn verify_dir_tree(&self, dir: &ObjRef, report: &mut VerifyReport) -> Result<(), FsError> {
        report.directories += 1;
        self.verify_object(dir, report)?;
        for entry in parse_dir(&self.read_object(dir)?)? {
            check_name(&entry.name)?;
            match entry.kind {
                NodeKind::File => {
                    report.files += 1;
                    self.verify_object(&entry.obj, report)?;
                }
                NodeKind::Directory => self.verify_dir_tree(&entry.obj, report)?,
            }
        }
        Ok(())
    }

    fn verify_object(&self, obj: &ObjRef, report: &mut VerifyReport) -> Result<(), FsError> {
        if obj.size == 0 {
            if !obj.root.is_null() {
                return Err(FsError::Corrupt("empty object with a root block"));
            }
            return Ok(());
        }
        if obj.root.is_null() {
            return Err(FsError::Corrupt("non-empty object without a root block"));
        }
        let expected = obj.size.div_ceil(self.block_size as u64);
        let counted = self.verify_ptr(&obj.root, obj.level, obj.size, 0, report)?;
        if counted != expected {
            return Err(FsError::Corrupt("object data-block count mismatch"));
        }
        Ok(())
    }

    /// Verify the subtree under `ptr` (at `level`), whose first data block is data block
    /// number `first_leaf` of an object `obj_size` bytes long; returns how many data blocks
    /// it covers.
    fn verify_ptr(
        &self,
        ptr: &BlockPtr,
        level: u8,
        obj_size: u64,
        first_leaf: u64,
        report: &mut VerifyReport,
    ) -> Result<u64, FsError> {
        let logical = self.read_block(ptr)?;
        report.blocks += 1;
        report.logical_bytes += ptr.lsize as u64;
        report.physical_bytes += self.alloc.aligned(ptr.psize as u64);
        if ptr.codec == Codec::Lz4 {
            report.compressed_blocks += 1;
        }
        if level == 0 {
            let bs = self.block_size as u64;
            let expected = core::cmp::min(bs, obj_size - first_leaf * bs);
            if ptr.lsize as u64 != expected {
                return Err(FsError::Corrupt("data block size mismatch"));
            }
            return Ok(1);
        }
        if logical.is_empty() || logical.len() % BLOCK_PTR_SIZE != 0 {
            return Err(FsError::Corrupt("malformed indirect block"));
        }
        let mut covered = 0u64;
        for chunk in logical.chunks(BLOCK_PTR_SIZE) {
            let child = BlockPtr::read_from(chunk)?;
            covered +=
                self.verify_ptr(&child, level - 1, obj_size, first_leaf + covered, report)?;
        }
        Ok(covered)
    }

    // --- garbage collection -----------------------------------------------------------------

    /// Deferred reclamation: walk everything any retained root can reach (the committed
    /// root, the pending root, and every snapshot in both snapshot tables) and hand the
    /// gaps below the allocation frontier back to the allocator for reuse. The free list is
    /// not persisted; run `gc` again after a remount to rebuild it.
    pub fn gc(&mut self) -> Result<GcReport, FsError> {
        let mut marked: Vec<Extent> = Vec::new();
        let roots = [self.committed_live_root, self.live_root];
        for root in roots {
            self.mark_dir_tree(&root, &mut marked)?;
        }
        let tables = [self.committed_snapshots, self.snapshots];
        for table in tables {
            self.mark_object(&table, &mut marked)?;
            for entry in parse_snapshots(&self.read_object(&table)?)? {
                self.mark_dir_tree(&entry.root, &mut marked)?;
            }
        }
        marked.sort_by_key(|extent| extent.addr);
        let mut free: Vec<Extent> = Vec::new();
        let mut cursor = DATA_START;
        for extent in marked {
            if extent.addr > cursor {
                free.push(Extent {
                    addr: cursor,
                    len: extent.addr - cursor,
                });
            }
            cursor = core::cmp::max(cursor, extent.addr + extent.len);
        }
        let frontier = self.alloc.frontier();
        if frontier > cursor {
            free.push(Extent {
                addr: cursor,
                len: frontier - cursor,
            });
        }
        let reclaimed_bytes = free.iter().map(|extent| extent.len).sum();
        let free_extents = free.len();
        self.alloc.set_free(free);
        Ok(GcReport {
            reclaimed_bytes,
            free_extents,
        })
    }

    fn mark_dir_tree(&self, dir: &ObjRef, out: &mut Vec<Extent>) -> Result<(), FsError> {
        self.mark_object(dir, out)?;
        for entry in parse_dir(&self.read_object(dir)?)? {
            match entry.kind {
                NodeKind::File => self.mark_object(&entry.obj, out)?,
                NodeKind::Directory => self.mark_dir_tree(&entry.obj, out)?,
            }
        }
        Ok(())
    }

    fn mark_object(&self, obj: &ObjRef, out: &mut Vec<Extent>) -> Result<(), FsError> {
        if obj.size == 0 {
            return Ok(());
        }
        self.mark_ptr(&obj.root, obj.level, out)
    }

    fn mark_ptr(&self, ptr: &BlockPtr, level: u8, out: &mut Vec<Extent>) -> Result<(), FsError> {
        out.push(Extent {
            addr: ptr.addr,
            len: self.alloc.aligned(ptr.psize as u64),
        });
        if level == 0 {
            return Ok(());
        }
        let logical = self.read_block(ptr)?;
        if logical.is_empty() || logical.len() % BLOCK_PTR_SIZE != 0 {
            return Err(FsError::Corrupt("malformed indirect block"));
        }
        for chunk in logical.chunks(BLOCK_PTR_SIZE) {
            let child = BlockPtr::read_from(chunk)?;
            self.mark_ptr(&child, level - 1, out)?;
        }
        Ok(())
    }

    // --- shared internals ----------------------------------------------------------------

    /// Walk `segments` down from `root`; returns the kind and object of the final node.
    fn resolve(&self, root: &ObjRef, segments: &[&str]) -> Result<(NodeKind, ObjRef), FsError> {
        let mut kind = NodeKind::Directory;
        let mut obj = *root;
        for segment in segments {
            if kind != NodeKind::Directory {
                return Err(FsError::NotADirectory);
            }
            let entries = parse_dir(&self.read_object(&obj)?)?;
            let entry = entries
                .into_iter()
                .find(|entry| entry.name == *segment)
                .ok_or(FsError::NotFound)?;
            kind = entry.kind;
            obj = entry.obj;
        }
        Ok((kind, obj))
    }

    /// Apply `op` inside the directory reached by walking `segments` down from `dir`, and
    /// return the new root of that walk: every directory along the path is rewritten
    /// (copy-on-write), everything else is shared with the old tree.
    fn apply_in_dir(
        &mut self,
        dir: &ObjRef,
        segments: &[&str],
        op: &DirOp<'_>,
    ) -> Result<ObjRef, FsError> {
        let mut entries = parse_dir(&self.read_object(dir)?)?;
        if let Some((segment, rest)) = segments.split_first() {
            let index = entries
                .iter()
                .position(|entry| entry.name == *segment)
                .ok_or(FsError::NotFound)?;
            if entries[index].kind != NodeKind::Directory {
                return Err(FsError::NotADirectory);
            }
            let child = entries[index].obj;
            entries[index].obj = self.apply_in_dir(&child, rest, op)?;
        } else {
            match op {
                DirOp::Insert { name, kind, obj } => {
                    if entries.iter().any(|entry| entry.name == *name) {
                        return Err(FsError::AlreadyExists);
                    }
                    entries.push(DirEntry {
                        name: String::from(*name),
                        kind: *kind,
                        obj: *obj,
                    });
                }
                DirOp::Replace { name, obj } => {
                    let entry = entries
                        .iter_mut()
                        .find(|entry| entry.name == *name)
                        .ok_or(FsError::NotFound)?;
                    entry.obj = *obj;
                }
                DirOp::Remove { name } => {
                    let index = entries
                        .iter()
                        .position(|entry| entry.name == *name)
                        .ok_or(FsError::NotFound)?;
                    entries.remove(index);
                }
            }
        }
        let bytes = serialize_dir(&entries);
        self.write_object(&bytes)
    }

    fn read_at_root(
        &self,
        root: &ObjRef,
        path: &str,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let segments = split_path(path)?;
        let (kind, obj) = self.resolve(root, &segments)?;
        if kind != NodeKind::File {
            return Err(FsError::IsADirectory);
        }
        self.read_object_range(&obj, offset, buf)
    }

    fn list_at_root(&self, root: &ObjRef, path: &str) -> Result<Vec<String>, FsError> {
        let segments = split_path(path)?;
        let (kind, obj) = self.resolve(root, &segments)?;
        if kind != NodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let entries = parse_dir(&self.read_object(&obj)?)?;
        Ok(entries.into_iter().map(|entry| entry.name).collect())
    }

    fn stat_at_root(&self, root: &ObjRef, path: &str) -> Result<NodeStat, FsError> {
        let segments = split_path(path)?;
        let (kind, obj) = self.resolve(root, &segments)?;
        Ok(NodeStat {
            kind,
            size: obj.size,
            hash: obj.root.hash,
        })
    }
}

impl<D: BlockDevice> SnapshotView<'_, D> {
    /// Read from a file in the snapshot; same contract as [`Eofs::read`].
    pub fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.fs.read_at_root(&self.root, path, offset, buf)
    }

    /// The entry names of a directory in the snapshot, in name order.
    pub fn list(&self, path: &str) -> Result<Vec<String>, FsError> {
        self.fs.list_at_root(&self.root, path)
    }

    /// Kind, size, and Merkle root hash of a node in the snapshot.
    pub fn stat(&self, path: &str) -> Result<NodeStat, FsError> {
        self.fs.stat_at_root(&self.root, path)
    }
}

/// Split a path into its segments. `/` (or the empty path) is the root directory; leading,
/// trailing, and doubled slashes are tolerated; `.`, `..`, embedded NUL, and over-long names
/// are not.
fn split_path(path: &str) -> Result<Vec<&str>, FsError> {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        check_name(segment)?;
        segments.push(segment);
    }
    Ok(segments)
}

/// Validate one path segment or snapshot name.
fn check_name(name: &str) -> Result<(), FsError> {
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || name == "."
        || name == ".."
        || name.contains(['/', '\0'])
    {
        return Err(FsError::InvalidPath);
    }
    Ok(())
}
