//! The eofs engine: format, mount, the namespace operations, transactions, snapshots,
//! verification, and garbage collection.
//!
//! Everything is copy-on-write: an operation writes new blocks for whatever it changes
//! (data blocks, the file's indirect tree, and every directory from the file up to the
//! root) and leaves all previously written blocks untouched. The new tree only becomes the
//! filesystem when [`AsyncEofs::commit`] writes a new uberblock; until then a crash or a
//! remount simply falls back to the last committed root. See `FORMAT.md`.
//!
//! The engine has **one implementation with two boundaries**: the core
//! ([`AsyncEofs`]) runs over an [`AsyncBlockDevice`] and awaits every device call, which is
//! what the guest provider needs (its `eo9:disk` import genuinely waits — see SPEC,
//! "Boundaries are honestly async"). The synchronous embedders — the kernel's storedisk
//! cache, `mkfs`, the test suite — use the [`Eofs`] facade at the bottom of this module: a
//! thin sync wrapper that adapts a [`BlockDevice`] via [`SyncDevice`] and drives each core
//! future with a single poll. Over a sync device every core future is ready on its first
//! poll (the only awaits in the core are device calls), so the facade is behaviorally
//! identical to the pre-async engine. No CoW, Merkle, or transaction logic exists twice.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::device::{AsyncBlockDevice, BlockDevice, SyncDevice, SyncReadRef, poll_now};
use crate::error::FsError;
use crate::format::{
    BLOCK_PTR_SIZE, BlockPtr, Codec, DATA_START, DirEntry, MAX_META_OBJECT_SIZE, MAX_NAME_LEN,
    NodeKind, ObjRef, SLOT_OFFSETS, SLOT_SIZE, SlotState, SnapEntry, Uberblock, parse_dir,
    parse_snapshots, serialize_dir, serialize_snapshots,
};
use crate::space::{Allocator, Extent};

/// Options for [`AsyncEofs::format`].
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

/// What [`AsyncEofs::stat`] reports about a node.
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

/// One entry of [`AsyncEofs::snapshot_list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub name: String,
    /// The transaction that made (or will make) the snapshot durable.
    pub txg: u64,
    /// Merkle root hash of the snapshot's directory tree.
    pub root_hash: [u8; 32],
}

/// What [`AsyncEofs::verify`] found while walking every reachable block.
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

/// What [`AsyncEofs::gc`] reclaimed.
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
    /// Bytes on the allocator's free list (populated by [`AsyncEofs::gc`]).
    pub free_bytes: u64,
    /// Device capacity in bytes.
    pub device_size: u64,
}

/// What [`AsyncEofs::mount_with_report`] observed while electing the uberblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountReport {
    /// The transaction the mount adopted.
    pub txg: u64,
    /// A slot held the *remains* of an uberblock (eofs magic present, checksum invalid)
    /// while another, valid slot was adopted. Either the invalid slot was newer — a commit
    /// that was acknowledged and has now been silently lost (rolled back) — or it was the
    /// older slot rotting in place. The two cannot be distinguished from the disk alone,
    /// so embedders must surface this loudly and let the operator decide (study 07, S7-1).
    pub fell_back_past_invalid_slot: bool,
}

/// What [`probe`] found on a device, without mounting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    /// The device holds an eofs filesystem; mounting it adopts `txg`. `degraded` is the
    /// [`MountReport::fell_back_past_invalid_slot`] condition.
    Eofs { txg: u64, degraded: bool },
    /// The device holds no eofs filesystem and its probed spans (the leading megabyte,
    /// the trailing 64 KiB, or the whole device when it is small — see [`probe`]) are all
    /// zero: a blank device, safe to format in place.
    Blank,
    /// The device holds no eofs filesystem but its probed spans are NOT all zero: it
    /// contains someone else's data. Formatting it would destroy that data, so nothing in
    /// Eo9 does so implicitly (study 07, S7-2) — formatting foreign data takes an
    /// explicit, forced `mkfs`.
    Foreign,
    /// The device carries eofs remains (magic present) but nothing mounts: every slot is
    /// invalid. Never reformatted implicitly; the data may be recoverable by hand.
    Unmountable,
}

/// How many leading bytes [`probe`] checks for zeroness when no eofs magic is present.
///
/// The span must clear every common foreign format's *primary* on-disk structure:
/// FAT/NTFS/exFAT boot sectors at 0, MBR at 446, GPT at 512, ext4 at 1024, bcachefs at
/// 4096, ZFS's first label, and — the furthest out of the lot — **btrfs at exactly
/// 64 KiB**. One MiB covers all of them with a wide margin (the previous 64 KiB span
/// ended exactly where btrfs begins; owner ruling on study 07, S7-2).
const PROBE_ZERO_PREFIX: u64 = 1024 * 1024;

/// How many trailing bytes [`probe`] checks for zeroness when no eofs magic is present:
/// backup structures live at the *end* of a device — the backup GPT header/entries in the
/// last ~17 KiB and ZFS's end-of-device uberblock arrays. A device whose start was wiped
/// but whose backups survive is damaged foreign data, not a blank device.
const PROBE_ZERO_SUFFIX: u64 = 64 * 1024;

/// Devices smaller than this are probed in full rather than by spans: at this size the
/// prefix and suffix windows nearly cover the device anyway, and checking everything
/// removes any gap between them.
const PROBE_WHOLE_DEVICE_BELOW: u64 = 2 * 1024 * 1024;

/// Inspect a device without mounting (or changing) anything: is there an eofs filesystem
/// here, a blank device, foreign data, or unmountable eofs remains?
///
/// This is the async-boundary form; synchronous embedders use [`probe`].
pub async fn probe_async<D: AsyncBlockDevice>(dev: &D) -> Result<ImageState, FsError> {
    let device_size = dev.size();
    let mut best: Option<u64> = None;
    let mut any_invalid = false;
    let mut any_magic = false;
    for offset in SLOT_OFFSETS {
        if offset + SLOT_SIZE > device_size {
            continue;
        }
        let mut slot = vec![0u8; SLOT_SIZE as usize];
        dev.read_at(offset, &mut slot).await?;
        match Uberblock::classify_slot(&slot) {
            Ok(SlotState::Valid(ub)) => {
                any_magic = true;
                if best.is_none_or(|txg| ub.txg > txg) {
                    best = Some(ub.txg);
                }
            }
            Ok(SlotState::Invalid) => {
                any_magic = true;
                any_invalid = true;
            }
            Ok(SlotState::NoMagic) => {}
            // classify_slot errors mean "valid checksum, unsupported contents" — that is
            // still unmistakably an eofs image.
            Err(_) => {
                any_magic = true;
                any_invalid = true;
            }
        }
    }
    if let Some(txg) = best {
        return Ok(ImageState::Eofs {
            txg,
            degraded: any_invalid,
        });
    }
    if any_magic {
        return Ok(ImageState::Unmountable);
    }
    // No magic anywhere: blank or foreign. A device only counts as blank when the spans
    // where any common foreign format would leave its mark are all zero — the leading
    // megabyte (primary superblocks, partition tables; btrfs sits at exactly 64 KiB), the
    // trailing 64 KiB (backup GPT, ZFS end labels), and, for small devices, simply all of
    // it. Anything non-zero in those spans is someone else's data and is never formatted
    // implicitly.
    let spans: [(u64, u64); 2] = if device_size <= PROBE_WHOLE_DEVICE_BELOW {
        [(0, device_size), (0, 0)]
    } else {
        [
            (0, PROBE_ZERO_PREFIX),
            (device_size - PROBE_ZERO_SUFFIX, device_size),
        ]
    };
    let mut chunk = vec![0u8; 4096];
    for (start, end) in spans {
        let mut offset = start;
        while offset < end {
            let len = core::cmp::min(4096, end - offset) as usize;
            dev.read_at(offset, &mut chunk[..len]).await?;
            if chunk[..len].iter().any(|byte| *byte != 0) {
                return Ok(ImageState::Foreign);
            }
            offset += len as u64;
        }
    }
    Ok(ImageState::Blank)
}

/// A mounted eofs filesystem over an asynchronous block device — the engine core.
///
/// Synchronous embedders use the [`Eofs`] facade instead.
pub struct AsyncEofs<D: AsyncBlockDevice> {
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

/// A read-only view of one snapshot (async boundary).
pub struct AsyncSnapshotView<'a, D: AsyncBlockDevice> {
    fs: &'a AsyncEofs<D>,
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

impl<D: AsyncBlockDevice> AsyncEofs<D> {
    // --- format & mount ----------------------------------------------------------------

    /// Create a fresh filesystem on `dev` and mount it. The initial (empty) state is
    /// committed as transaction 1 before this returns.
    pub async fn format(dev: D, opts: &FormatOptions) -> Result<AsyncEofs<D>, FsError> {
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
        let mut fs = AsyncEofs {
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
        fs.dev
            .write_at(0, &[0u8; (2 * SLOT_SIZE) as usize])
            .await?;
        fs.commit().await?;
        Ok(fs)
    }

    /// Mount an existing filesystem: read both uberblock slots and adopt the valid one with
    /// the highest transaction number.
    pub async fn mount(dev: D) -> Result<AsyncEofs<D>, FsError> {
        Ok(Self::mount_with_report(dev).await?.0)
    }

    /// [`mount`](AsyncEofs::mount), and also report what the uberblock election saw — in
    /// particular whether the mount had to fall back past a slot holding the *remains* of
    /// an uberblock, which can mean an acknowledged commit has been silently lost.
    /// Embedders that talk to a user should surface that loudly (study 07, S7-1).
    pub async fn mount_with_report(dev: D) -> Result<(AsyncEofs<D>, MountReport), FsError> {
        let device_size = dev.size();
        if device_size < DATA_START {
            return Err(FsError::Corrupt("device too small to hold an eofs image"));
        }
        let mut best: Option<Uberblock> = None;
        let mut deferred: Option<FsError> = None;
        let mut fell_back = false;
        for offset in SLOT_OFFSETS {
            let mut slot = vec![0u8; SLOT_SIZE as usize];
            dev.read_at(offset, &mut slot).await?;
            match Uberblock::classify_slot(&slot) {
                Ok(SlotState::Valid(ub)) => {
                    if best.as_ref().is_none_or(|b| ub.txg > b.txg) {
                        best = Some(ub);
                    }
                }
                Ok(SlotState::NoMagic) => {}
                Ok(SlotState::Invalid) => fell_back = true,
                Err(err) => {
                    fell_back = true;
                    deferred = Some(err);
                }
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
        let report = MountReport {
            txg: ub.txg,
            fell_back_past_invalid_slot: fell_back,
        };
        Ok((
            AsyncEofs {
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
            },
            report,
        ))
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
    pub async fn commit(&mut self) -> Result<u64, FsError> {
        if !self.dirty {
            return Ok(self.committed_txg);
        }
        // Everything the new root references must be durable before the root flip.
        self.dev.flush().await?;
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
        self.dev.write_at(slot, &ub.to_slot_bytes()).await?;
        self.dev.flush().await?;
        self.committed_txg = txg;
        self.committed_live_root = self.live_root;
        self.committed_snapshots = self.snapshots;
        self.dirty = false;
        Ok(txg)
    }

    /// Discard every change made since the last commit: the pending state returns to the
    /// last committed transaction, exactly as a remount would see it. The blocks the
    /// discarded changes wrote are unreferenced afterwards and are reclaimed by the next
    /// [`gc`](AsyncEofs::gc).
    ///
    /// This is what lets an embedder make a multi-step change (say, a truncate followed by
    /// a write) atomic: apply the steps without committing, and if any step fails, roll
    /// back — the on-disk filesystem never holds the half-applied state.
    pub fn rollback(&mut self) {
        self.live_root = self.committed_live_root;
        self.snapshots = self.committed_snapshots;
        self.dirty = false;
    }

    // --- namespace operations --------------------------------------------------------------

    /// Create an empty file.
    pub async fn create_file(&mut self, path: &str) -> Result<(), FsError> {
        self.create_node(path, NodeKind::File).await
    }

    /// Create an empty directory.
    pub async fn mkdir(&mut self, path: &str) -> Result<(), FsError> {
        self.create_node(path, NodeKind::Directory).await
    }

    async fn create_node(&mut self, path: &str, kind: NodeKind) -> Result<(), FsError> {
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
        self.live_root = self.apply_in_dir(&root, parent, &op).await?;
        self.dirty = true;
        Ok(())
    }

    /// Write `data` into a file at byte `offset`, growing it (zero-filling any gap) if the
    /// write reaches past the current end.
    pub async fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), FsError> {
        let segments = split_path(path)?;
        let Some((name, parent)) = segments.split_last() else {
            return Err(FsError::IsADirectory);
        };
        let name = *name;
        let root = self.live_root;
        let (kind, obj) = self.resolve(&root, &segments).await?;
        if kind != NodeKind::File {
            return Err(FsError::IsADirectory);
        }
        let new_obj = self.write_object_range(&obj, offset, data).await?;
        let op = DirOp::Replace { name, obj: new_obj };
        self.live_root = self.apply_in_dir(&root, parent, &op).await?;
        self.dirty = true;
        Ok(())
    }

    /// Read from a file at byte `offset` into `buf`; returns the number of bytes read
    /// (short only at end-of-file).
    pub async fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.read_at_root(&self.live_root, path, offset, buf).await
    }

    /// The entry names of a directory, in name order.
    pub async fn list(&self, path: &str) -> Result<Vec<String>, FsError> {
        self.list_at_root(&self.live_root, path).await
    }

    /// Kind, size, and Merkle root hash of a node.
    pub async fn stat(&self, path: &str) -> Result<NodeStat, FsError> {
        self.stat_at_root(&self.live_root, path).await
    }

    /// Remove a file or an empty directory.
    pub async fn remove(&mut self, path: &str) -> Result<(), FsError> {
        let segments = split_path(path)?;
        let Some((name, parent)) = segments.split_last() else {
            return Err(FsError::InvalidPath);
        };
        let name = *name;
        let root = self.live_root;
        let (kind, obj) = self.resolve(&root, &segments).await?;
        if kind == NodeKind::Directory && obj.size != 0 {
            return Err(FsError::DirectoryNotEmpty);
        }
        let op = DirOp::Remove { name };
        self.live_root = self.apply_in_dir(&root, parent, &op).await?;
        self.dirty = true;
        Ok(())
    }

    // --- snapshots -------------------------------------------------------------------------

    /// Retain the filesystem exactly as it is right now under `name`. Like every other
    /// change, the snapshot becomes durable at the next [`commit`](AsyncEofs::commit).
    pub async fn snapshot_create(&mut self, name: &str) -> Result<(), FsError> {
        check_name(name)?;
        let snapshots = self.snapshots;
        let mut entries = parse_snapshots(&self.read_object(&snapshots).await?)?;
        if entries.iter().any(|entry| entry.name == name) {
            return Err(FsError::AlreadyExists);
        }
        entries.push(SnapEntry {
            txg: self.committed_txg + 1,
            name: String::from(name),
            root: self.live_root,
        });
        let bytes = serialize_snapshots(&entries);
        if bytes.len() as u64 > MAX_META_OBJECT_SIZE {
            return Err(FsError::NoSpace);
        }
        self.snapshots = self.write_object(&bytes).await?;
        self.dirty = true;
        Ok(())
    }

    /// All snapshots, in creation order.
    pub async fn snapshot_list(&self) -> Result<Vec<SnapshotInfo>, FsError> {
        let entries = parse_snapshots(&self.read_object(&self.snapshots).await?)?;
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
    pub async fn snapshot(&self, name: &str) -> Result<AsyncSnapshotView<'_, D>, FsError> {
        let entries = parse_snapshots(&self.read_object(&self.snapshots).await?)?;
        let entry = entries
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(FsError::NotFound)?;
        Ok(AsyncSnapshotView {
            fs: self,
            root: entry.root,
        })
    }

    // --- verification ------------------------------------------------------------------------

    /// Walk every reachable block — the live tree, the snapshot table, and every snapshot's
    /// tree — re-reading each one and checking it against the blake3 hash in its pointer.
    pub async fn verify(&self) -> Result<VerifyReport, FsError> {
        let mut report = VerifyReport::default();
        self.verify_dir_tree(&self.live_root, &mut report).await?;
        let snapshots = self.snapshots;
        self.verify_object(&snapshots, &mut report).await?;
        for entry in parse_snapshots(&self.read_object(&snapshots).await?)? {
            report.snapshots += 1;
            self.verify_dir_tree(&entry.root, &mut report).await?;
        }
        Ok(report)
    }

    /// Walk one directory tree with an explicit worklist (no recursion, so a corrupted
    /// image cannot exhaust the stack with a deep chain) and a visited set (so repeated or
    /// cyclic references to the same directory object terminate with an error instead of
    /// multiplying the walk).
    async fn verify_dir_tree(
        &self,
        root: &ObjRef,
        report: &mut VerifyReport,
    ) -> Result<(), FsError> {
        let mut pending: Vec<ObjRef> = vec![*root];
        let mut visited: BTreeSet<u64> = BTreeSet::new();
        while let Some(dir) = pending.pop() {
            if !dir.root.is_null() && !visited.insert(dir.root.addr) {
                return Err(FsError::Corrupt("directory tree is not a tree"));
            }
            report.directories += 1;
            self.verify_object(&dir, report).await?;
            for entry in parse_dir(&self.read_object(&dir).await?)? {
                check_name(&entry.name)?;
                match entry.kind {
                    NodeKind::File => {
                        report.files += 1;
                        self.verify_object(&entry.obj, report).await?;
                    }
                    NodeKind::Directory => pending.push(entry.obj),
                }
            }
        }
        Ok(())
    }

    async fn verify_object(&self, obj: &ObjRef, report: &mut VerifyReport) -> Result<(), FsError> {
        let leaf_count = self.check_object(obj)?;
        if leaf_count == 0 {
            return Ok(());
        }
        let counted = self
            .verify_ptr(&obj.root, obj.level, obj.size, leaf_count, 0, report)
            .await?;
        if counted != leaf_count {
            return Err(FsError::Corrupt("object data-block count mismatch"));
        }
        Ok(())
    }

    /// Verify the subtree under `ptr` (at `level`), whose first data block is data block
    /// number `first_leaf` of an object `obj_size` bytes long with `leaf_count` data blocks
    /// in total; returns how many data blocks it covers. The walk fails as soon as it finds
    /// more data blocks than the object's declared size allows, so an inflated tree cannot
    /// drive unbounded work. (Boxed: async recursion bounded by the tree height, which
    /// [`check_object`](Self::check_object) has already validated.)
    fn verify_ptr<'a>(
        &'a self,
        ptr: &'a BlockPtr,
        level: u8,
        obj_size: u64,
        leaf_count: u64,
        first_leaf: u64,
        report: &'a mut VerifyReport,
    ) -> Pin<Box<dyn Future<Output = Result<u64, FsError>> + 'a>> {
        Box::pin(async move {
            let logical = self.read_block(ptr).await?;
            report.blocks += 1;
            report.logical_bytes += ptr.lsize as u64;
            report.physical_bytes += self.alloc.aligned(ptr.psize as u64);
            if ptr.codec == Codec::Lz4 {
                report.compressed_blocks += 1;
            }
            if level == 0 {
                if first_leaf >= leaf_count {
                    return Err(FsError::Corrupt(
                        "object has more data blocks than its size",
                    ));
                }
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
                covered += self
                    .verify_ptr(
                        &child,
                        level - 1,
                        obj_size,
                        leaf_count,
                        first_leaf + covered,
                        report,
                    )
                    .await?;
            }
            Ok(covered)
        })
    }

    // --- garbage collection -----------------------------------------------------------------

    /// Deferred reclamation: walk everything any retained root can reach (the committed
    /// root, the pending root, and every snapshot in both snapshot tables) and hand the
    /// gaps below the allocation frontier back to the allocator for reuse. The free list is
    /// not persisted; run `gc` again after a remount to rebuild it.
    pub async fn gc(&mut self) -> Result<GcReport, FsError> {
        let mut marked: Vec<Extent> = Vec::new();
        let roots = [self.committed_live_root, self.live_root];
        for root in roots {
            self.mark_dir_tree(&root, &mut marked).await?;
        }
        let tables = [self.committed_snapshots, self.snapshots];
        for table in tables {
            self.mark_object(&table, &mut marked).await?;
            for entry in parse_snapshots(&self.read_object(&table).await?)? {
                self.mark_dir_tree(&entry.root, &mut marked).await?;
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

    /// Same walk discipline as [`verify_dir_tree`](Self::verify_dir_tree): an explicit
    /// worklist and a visited set, so GC over a corrupted image cannot recurse without
    /// bound or loop on a cyclic directory structure.
    async fn mark_dir_tree(&self, root: &ObjRef, out: &mut Vec<Extent>) -> Result<(), FsError> {
        let mut pending: Vec<ObjRef> = vec![*root];
        let mut visited: BTreeSet<u64> = BTreeSet::new();
        while let Some(dir) = pending.pop() {
            if !dir.root.is_null() && !visited.insert(dir.root.addr) {
                return Err(FsError::Corrupt("directory tree is not a tree"));
            }
            self.mark_object(&dir, out).await?;
            for entry in parse_dir(&self.read_object(&dir).await?)? {
                match entry.kind {
                    NodeKind::File => self.mark_object(&entry.obj, out).await?,
                    NodeKind::Directory => pending.push(entry.obj),
                }
            }
        }
        Ok(())
    }

    async fn mark_object(&self, obj: &ObjRef, out: &mut Vec<Extent>) -> Result<(), FsError> {
        let leaf_count = self.check_object(obj)?;
        if leaf_count == 0 {
            return Ok(());
        }
        let mut leaves_seen = 0u64;
        self.mark_ptr(&obj.root, obj.level, leaf_count, &mut leaves_seen, out)
            .await
    }

    /// (Boxed: async recursion bounded by the validated tree height.)
    fn mark_ptr<'a>(
        &'a self,
        ptr: &'a BlockPtr,
        level: u8,
        leaf_count: u64,
        leaves_seen: &'a mut u64,
        out: &'a mut Vec<Extent>,
    ) -> Pin<Box<dyn Future<Output = Result<(), FsError>> + 'a>> {
        Box::pin(async move {
            out.push(Extent {
                addr: ptr.addr,
                len: self.alloc.aligned(ptr.psize as u64),
            });
            if level == 0 {
                *leaves_seen += 1;
                if *leaves_seen > leaf_count {
                    return Err(FsError::Corrupt(
                        "object has more data blocks than its size",
                    ));
                }
                return Ok(());
            }
            let logical = self.read_block(ptr).await?;
            if logical.is_empty() || logical.len() % BLOCK_PTR_SIZE != 0 {
                return Err(FsError::Corrupt("malformed indirect block"));
            }
            for chunk in logical.chunks(BLOCK_PTR_SIZE) {
                let child = BlockPtr::read_from(chunk)?;
                self.mark_ptr(&child, level - 1, leaf_count, leaves_seen, out)
                    .await?;
            }
            Ok(())
        })
    }

    // --- shared internals ----------------------------------------------------------------

    /// Walk `segments` down from `root`; returns the kind and object of the final node.
    async fn resolve(
        &self,
        root: &ObjRef,
        segments: &[&str],
    ) -> Result<(NodeKind, ObjRef), FsError> {
        let mut kind = NodeKind::Directory;
        let mut obj = *root;
        for segment in segments {
            if kind != NodeKind::Directory {
                return Err(FsError::NotADirectory);
            }
            let entries = parse_dir(&self.read_object(&obj).await?)?;
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
    /// (copy-on-write), everything else is shared with the old tree. (Boxed: async
    /// recursion bounded by the path depth.)
    fn apply_in_dir<'a>(
        &'a mut self,
        dir: &'a ObjRef,
        segments: &'a [&'a str],
        op: &'a DirOp<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ObjRef, FsError>> + 'a>> {
        Box::pin(async move {
            let mut entries = parse_dir(&self.read_object(dir).await?)?;
            if let Some((segment, rest)) = segments.split_first() {
                let index = entries
                    .iter()
                    .position(|entry| entry.name == *segment)
                    .ok_or(FsError::NotFound)?;
                if entries[index].kind != NodeKind::Directory {
                    return Err(FsError::NotADirectory);
                }
                let child = entries[index].obj;
                entries[index].obj = self.apply_in_dir(&child, rest, op).await?;
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
            if bytes.len() as u64 > MAX_META_OBJECT_SIZE {
                return Err(FsError::NoSpace);
            }
            self.write_object(&bytes).await
        })
    }

    async fn read_at_root(
        &self,
        root: &ObjRef,
        path: &str,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let segments = split_path(path)?;
        let (kind, obj) = self.resolve(root, &segments).await?;
        if kind != NodeKind::File {
            return Err(FsError::IsADirectory);
        }
        self.read_object_range(&obj, offset, buf).await
    }

    async fn list_at_root(&self, root: &ObjRef, path: &str) -> Result<Vec<String>, FsError> {
        let segments = split_path(path)?;
        let (kind, obj) = self.resolve(root, &segments).await?;
        if kind != NodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let entries = parse_dir(&self.read_object(&obj).await?)?;
        Ok(entries.into_iter().map(|entry| entry.name).collect())
    }

    async fn stat_at_root(&self, root: &ObjRef, path: &str) -> Result<NodeStat, FsError> {
        let segments = split_path(path)?;
        let (kind, obj) = self.resolve(root, &segments).await?;
        Ok(NodeStat {
            kind,
            size: obj.size,
            hash: obj.root.hash,
        })
    }
}

impl<D: AsyncBlockDevice> AsyncSnapshotView<'_, D> {
    /// Read from a file in the snapshot; same contract as [`AsyncEofs::read`].
    pub async fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.fs.read_at_root(&self.root, path, offset, buf).await
    }

    /// The entry names of a directory in the snapshot, in name order.
    pub async fn list(&self, path: &str) -> Result<Vec<String>, FsError> {
        self.fs.list_at_root(&self.root, path).await
    }

    /// Kind, size, and Merkle root hash of a node in the snapshot.
    pub async fn stat(&self, path: &str) -> Result<NodeStat, FsError> {
        self.fs.stat_at_root(&self.root, path).await
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

// --- the synchronous facade ------------------------------------------------------------------
//
// One implementation, two boundaries: everything below is a thin wrapper that drives the
// async core over a [`SyncDevice`] with single polls. Over a sync device the core's futures
// are ready on their first poll (its only awaits are device calls), so these wrappers are
// behaviorally identical to the engine before the async boundary existed — which is what
// keeps the kernel's storedisk cache, `mkfs`, and the whole test suite working unchanged.

/// Inspect a device without mounting (or changing) anything; the synchronous form of
/// [`probe_async`] for sync embedders (the provider's auto-format decision, `mkfs`).
pub fn probe<D: BlockDevice>(dev: &D) -> Result<ImageState, FsError> {
    poll_now(probe_async(&SyncReadRef(dev)))
}

/// A mounted eofs filesystem over a synchronous block device.
///
/// This is the boundary the kernel's storedisk cache, `mkfs`, and the test suite use; the
/// guest provider (whose disk genuinely waits) uses [`AsyncEofs`] directly.
pub struct Eofs<D: BlockDevice> {
    core: AsyncEofs<SyncDevice<D>>,
}

/// A read-only view of one snapshot (sync facade).
pub struct SnapshotView<'a, D: BlockDevice> {
    inner: AsyncSnapshotView<'a, SyncDevice<D>>,
}

impl<D: BlockDevice> Eofs<D> {
    /// See [`AsyncEofs::format`].
    pub fn format(dev: D, opts: &FormatOptions) -> Result<Eofs<D>, FsError> {
        Ok(Eofs {
            core: poll_now(AsyncEofs::format(SyncDevice(dev), opts))?,
        })
    }

    /// See [`AsyncEofs::mount`].
    pub fn mount(dev: D) -> Result<Eofs<D>, FsError> {
        Ok(Eofs {
            core: poll_now(AsyncEofs::mount(SyncDevice(dev)))?,
        })
    }

    /// See [`AsyncEofs::mount_with_report`].
    pub fn mount_with_report(dev: D) -> Result<(Eofs<D>, MountReport), FsError> {
        let (core, report) = poll_now(AsyncEofs::mount_with_report(SyncDevice(dev)))?;
        Ok((Eofs { core }, report))
    }

    /// See [`AsyncEofs::unmount`].
    pub fn unmount(self) -> D {
        self.core.unmount().into_inner()
    }

    /// See [`AsyncEofs::block_size`].
    pub fn block_size(&self) -> u32 {
        self.core.block_size()
    }

    /// See [`AsyncEofs::compression`].
    pub fn compression(&self) -> bool {
        self.core.compression()
    }

    /// See [`AsyncEofs::txg`].
    pub fn txg(&self) -> u64 {
        self.core.txg()
    }

    /// See [`AsyncEofs::is_dirty`].
    pub fn is_dirty(&self) -> bool {
        self.core.is_dirty()
    }

    /// See [`AsyncEofs::space`].
    pub fn space(&self) -> SpaceReport {
        self.core.space()
    }

    /// See [`AsyncEofs::commit`].
    pub fn commit(&mut self) -> Result<u64, FsError> {
        poll_now(self.core.commit())
    }

    /// See [`AsyncEofs::rollback`].
    pub fn rollback(&mut self) {
        self.core.rollback()
    }

    /// See [`AsyncEofs::create_file`].
    pub fn create_file(&mut self, path: &str) -> Result<(), FsError> {
        poll_now(self.core.create_file(path))
    }

    /// See [`AsyncEofs::mkdir`].
    pub fn mkdir(&mut self, path: &str) -> Result<(), FsError> {
        poll_now(self.core.mkdir(path))
    }

    /// See [`AsyncEofs::write`].
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), FsError> {
        poll_now(self.core.write(path, offset, data))
    }

    /// See [`AsyncEofs::read`].
    pub fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        poll_now(self.core.read(path, offset, buf))
    }

    /// See [`AsyncEofs::list`].
    pub fn list(&self, path: &str) -> Result<Vec<String>, FsError> {
        poll_now(self.core.list(path))
    }

    /// See [`AsyncEofs::stat`].
    pub fn stat(&self, path: &str) -> Result<NodeStat, FsError> {
        poll_now(self.core.stat(path))
    }

    /// See [`AsyncEofs::remove`].
    pub fn remove(&mut self, path: &str) -> Result<(), FsError> {
        poll_now(self.core.remove(path))
    }

    /// See [`AsyncEofs::snapshot_create`].
    pub fn snapshot_create(&mut self, name: &str) -> Result<(), FsError> {
        poll_now(self.core.snapshot_create(name))
    }

    /// See [`AsyncEofs::snapshot_list`].
    pub fn snapshot_list(&self) -> Result<Vec<SnapshotInfo>, FsError> {
        poll_now(self.core.snapshot_list())
    }

    /// See [`AsyncEofs::snapshot`].
    pub fn snapshot(&self, name: &str) -> Result<SnapshotView<'_, D>, FsError> {
        Ok(SnapshotView {
            inner: poll_now(self.core.snapshot(name))?,
        })
    }

    /// See [`AsyncEofs::verify`].
    pub fn verify(&self) -> Result<VerifyReport, FsError> {
        poll_now(self.core.verify())
    }

    /// See [`AsyncEofs::gc`].
    pub fn gc(&mut self) -> Result<GcReport, FsError> {
        poll_now(self.core.gc())
    }
}

impl<D: BlockDevice> SnapshotView<'_, D> {
    /// Read from a file in the snapshot; same contract as [`Eofs::read`].
    pub fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        poll_now(self.inner.read(path, offset, buf))
    }

    /// The entry names of a directory in the snapshot, in name order.
    pub fn list(&self, path: &str) -> Result<Vec<String>, FsError> {
        poll_now(self.inner.list(path))
    }

    /// Kind, size, and Merkle root hash of a node in the snapshot.
    pub fn stat(&self, path: &str) -> Result<NodeStat, FsError> {
        poll_now(self.inner.stat(path))
    }
}
