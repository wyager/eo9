//! Block I/O and byte-object trees.
//!
//! Everything stored on disk is written through [`Eofs::write_block`] (hash, compress,
//! allocate, write) and read back through [`Eofs::read_block`] (read, decompress, check the
//! hash). On top of single blocks sit *byte objects* — arbitrary-length byte strings stored
//! as a tree of data blocks under indirect blocks of block pointers. Files, serialized
//! directories, and the snapshot table are all byte objects; see `FORMAT.md`.

use alloc::vec;
use alloc::vec::Vec;

use crate::device::BlockDevice;
use crate::error::FsError;
use crate::format::{BLOCK_PTR_SIZE, BlockPtr, Codec, DATA_START, MAX_META_OBJECT_SIZE, ObjRef};
use crate::fs::Eofs;

impl<D: BlockDevice> Eofs<D> {
    /// Pointers per indirect block.
    pub(crate) fn fanout(&self) -> usize {
        self.block_size as usize / BLOCK_PTR_SIZE
    }

    /// Hash, (maybe) compress, allocate, and write one logical block. Returns the pointer.
    pub(crate) fn write_block(&mut self, logical: &[u8]) -> Result<BlockPtr, FsError> {
        debug_assert!(!logical.is_empty() && logical.len() <= self.block_size as usize);
        let hash = blake3::hash(logical);
        let compressed;
        let (codec, payload): (Codec, &[u8]) = if self.codec == Codec::Lz4 {
            compressed = lz4_flex::compress(logical);
            if compressed.len() < logical.len() {
                (Codec::Lz4, &compressed)
            } else {
                // Incompressible: store raw so reads never pay for a useless decompression.
                (Codec::Raw, logical)
            }
        } else {
            (Codec::Raw, logical)
        };
        let addr = self.alloc.allocate(payload.len() as u64)?;
        self.dev.write_at(addr, payload)?;
        Ok(BlockPtr {
            addr,
            lsize: logical.len() as u32,
            psize: payload.len() as u32,
            codec,
            hash: *hash.as_bytes(),
        })
    }

    /// Read one block: fetch the stored bytes, decompress if needed, and check the blake3
    /// hash against the pointer. Every read path in the filesystem goes through this, so a
    /// corrupted block can never be returned as data.
    pub(crate) fn read_block(&self, ptr: &BlockPtr) -> Result<Vec<u8>, FsError> {
        if ptr.is_null() {
            return Err(FsError::Corrupt("read through a null block pointer"));
        }
        if ptr.lsize == 0
            || ptr.lsize > self.block_size
            || ptr.psize == 0
            || ptr.psize > self.block_size
        {
            return Err(FsError::Corrupt("block pointer with impossible sizes"));
        }
        if ptr.addr < DATA_START
            || !ptr.addr.is_multiple_of(self.alloc_unit as u64)
            || ptr
                .addr
                .checked_add(ptr.psize as u64)
                .is_none_or(|end| end > self.device_size)
        {
            return Err(FsError::Corrupt("block pointer outside the data region"));
        }
        let mut payload = vec![0u8; ptr.psize as usize];
        self.dev.read_at(ptr.addr, &mut payload)?;
        let logical = match ptr.codec {
            Codec::Raw => {
                if ptr.psize != ptr.lsize {
                    return Err(FsError::Corrupt("raw block with differing sizes"));
                }
                payload
            }
            Codec::Lz4 => {
                let out = lz4_flex::decompress(&payload, ptr.lsize as usize)
                    .map_err(|_| FsError::ChecksumMismatch)?;
                if out.len() != ptr.lsize as usize {
                    return Err(FsError::Corrupt("decompressed size mismatch"));
                }
                out
            }
        };
        if blake3::hash(&logical).as_bytes() != &ptr.hash {
            return Err(FsError::ChecksumMismatch);
        }
        Ok(logical)
    }

    /// Sanity-check an object reference *before* walking or allocating for it, so a
    /// corrupted or hostile reference cannot drive unbounded work. Returns the number of
    /// data blocks the object must have.
    ///
    /// Checks: an empty object has a null root and level 0; a non-empty object has a
    /// non-null root, needs no more data blocks than the device could possibly hold (every
    /// block occupies at least one allocation unit), and declares exactly the tree height
    /// the writer would have produced for its size (so an inflated `level` cannot multiply
    /// the walk's fan-out).
    pub(crate) fn check_object(&self, obj: &ObjRef) -> Result<u64, FsError> {
        if obj.size == 0 {
            if !obj.root.is_null() || obj.level != 0 {
                return Err(FsError::Corrupt("empty object with a root block"));
            }
            return Ok(0);
        }
        if obj.root.is_null() {
            return Err(FsError::Corrupt("non-empty object without a root block"));
        }
        let leaf_count = obj.size.div_ceil(self.block_size as u64);
        let max_leaves = (self.device_size - DATA_START) / self.alloc_unit as u64;
        if leaf_count > max_leaves {
            return Err(FsError::Corrupt("object larger than the device"));
        }
        let fanout = self.fanout() as u64;
        let mut canonical_level = 0u8;
        let mut capacity = 1u64;
        while capacity < leaf_count {
            capacity = capacity.saturating_mul(fanout);
            canonical_level += 1;
        }
        if obj.level != canonical_level {
            return Err(FsError::Corrupt("object level inconsistent with its size"));
        }
        Ok(leaf_count)
    }

    /// The ordered data-block pointers of an object (reads and checks every indirect block
    /// on the way down).
    pub(crate) fn collect_leaves(&self, obj: &ObjRef) -> Result<Vec<BlockPtr>, FsError> {
        let leaf_count = self.check_object(obj)?;
        if leaf_count == 0 {
            return Ok(Vec::new());
        }
        let mut leaves = Vec::new();
        self.collect_level(&obj.root, obj.level, leaf_count, &mut leaves)?;
        if leaves.len() as u64 != leaf_count {
            return Err(FsError::Corrupt("object data-block count mismatch"));
        }
        Ok(leaves)
    }

    fn collect_level(
        &self,
        ptr: &BlockPtr,
        level: u8,
        leaf_count: u64,
        out: &mut Vec<BlockPtr>,
    ) -> Result<(), FsError> {
        if level == 0 {
            if out.len() as u64 >= leaf_count {
                return Err(FsError::Corrupt(
                    "object has more data blocks than its size",
                ));
            }
            out.push(*ptr);
            return Ok(());
        }
        let block = self.read_block(ptr)?;
        if block.is_empty() || block.len() % BLOCK_PTR_SIZE != 0 {
            return Err(FsError::Corrupt("malformed indirect block"));
        }
        for chunk in block.chunks(BLOCK_PTR_SIZE) {
            let child = BlockPtr::read_from(chunk)?;
            self.collect_level(&child, level - 1, leaf_count, out)?;
        }
        Ok(())
    }

    /// Build the indirect-block tree over `leaves` and return the object reference.
    pub(crate) fn build_tree(&mut self, leaves: &[BlockPtr], size: u64) -> Result<ObjRef, FsError> {
        if leaves.is_empty() {
            debug_assert_eq!(size, 0);
            return Ok(ObjRef::EMPTY);
        }
        let fanout = self.fanout();
        let mut level = 0u8;
        let mut current: Vec<BlockPtr> = leaves.to_vec();
        while current.len() > 1 {
            let mut next = Vec::with_capacity(current.len().div_ceil(fanout));
            for group in current.chunks(fanout) {
                let mut bytes = vec![0u8; group.len() * BLOCK_PTR_SIZE];
                for (i, ptr) in group.iter().enumerate() {
                    ptr.write_to(&mut bytes[i * BLOCK_PTR_SIZE..(i + 1) * BLOCK_PTR_SIZE]);
                }
                next.push(self.write_block(&bytes)?);
            }
            current = next;
            level += 1;
        }
        Ok(ObjRef {
            size,
            level,
            root: current[0],
        })
    }

    /// Store `data` as a fresh byte object.
    pub(crate) fn write_object(&mut self, data: &[u8]) -> Result<ObjRef, FsError> {
        self.write_object_range(&ObjRef::EMPTY, 0, data)
    }

    /// Read a whole byte object into memory. This is the *metadata* read path (serialized
    /// directories and the snapshot table), so the object's declared size is capped at
    /// [`MAX_META_OBJECT_SIZE`] before anything is allocated; file contents go through
    /// [`read_object_range`](Self::read_object_range) into caller-provided buffers instead.
    pub(crate) fn read_object(&self, obj: &ObjRef) -> Result<Vec<u8>, FsError> {
        if obj.size > MAX_META_OBJECT_SIZE {
            return Err(FsError::Corrupt("metadata object impossibly large"));
        }
        self.check_object(obj)?;
        let mut buf = vec![0u8; obj.size as usize];
        let got = self.read_object_range(obj, 0, &mut buf)?;
        debug_assert_eq!(got as u64, obj.size);
        Ok(buf)
    }

    /// Read up to `buf.len()` bytes of an object starting at `offset`; returns how many
    /// bytes were read (short only at end-of-object).
    pub(crate) fn read_object_range(
        &self,
        obj: &ObjRef,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        if offset >= obj.size || buf.is_empty() {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, obj.size - offset) as usize;
        let bs = self.block_size as u64;
        let leaves = self.collect_leaves(obj)?;
        let mut done = 0usize;
        while done < want {
            let at = offset + done as u64;
            let block_index = (at / bs) as usize;
            let within = (at % bs) as usize;
            let logical = self.read_block(&leaves[block_index])?;
            let take = core::cmp::min(want - done, logical.len() - within);
            buf[done..done + take].copy_from_slice(&logical[within..within + take]);
            done += take;
        }
        Ok(want)
    }

    /// Copy-on-write range update: returns a new object that equals `obj` with `data`
    /// spliced in at `offset` (growing it, zero-filling any gap, if the write reaches past
    /// the old end). Untouched data blocks are reused by pointer; the indirect tree is
    /// rebuilt.
    pub(crate) fn write_object_range(
        &mut self,
        obj: &ObjRef,
        offset: u64,
        data: &[u8],
    ) -> Result<ObjRef, FsError> {
        if data.is_empty() {
            return Ok(*obj);
        }
        let write_end = offset
            .checked_add(data.len() as u64)
            .ok_or(FsError::NoSpace)?;
        let new_size = core::cmp::max(obj.size, write_end);
        let bs = self.block_size as u64;
        let old_leaves = self.collect_leaves(obj)?;
        let block_count = new_size.div_ceil(bs);
        let mut new_leaves = Vec::with_capacity(block_count as usize);
        for b in 0..block_count {
            let block_start = b * bs;
            let new_len = core::cmp::min(bs, new_size - block_start) as usize;
            let old_len = if (b as usize) < old_leaves.len() {
                core::cmp::min(bs, obj.size - block_start) as usize
            } else {
                0
            };
            let overlap_start = core::cmp::max(block_start, offset);
            let overlap_end = core::cmp::min(block_start + new_len as u64, write_end);
            let overlaps = overlap_start < overlap_end;
            if !overlaps && new_len == old_len {
                new_leaves.push(old_leaves[b as usize]);
                continue;
            }
            let mut content = vec![0u8; new_len];
            if old_len > 0 {
                let old = self.read_block(&old_leaves[b as usize])?;
                if old.len() != old_len {
                    return Err(FsError::Corrupt("data block size mismatch"));
                }
                content[..old_len].copy_from_slice(&old);
            }
            if overlaps {
                let src = (overlap_start - offset) as usize..(overlap_end - offset) as usize;
                let dst =
                    (overlap_start - block_start) as usize..(overlap_end - block_start) as usize;
                content[dst].copy_from_slice(&data[src]);
            }
            new_leaves.push(self.write_block(&content)?);
        }
        self.build_tree(&new_leaves, new_size)
    }
}
