//! Space allocation: append at a frontier, with free extents (found by a manual GC walk)
//! reused first.
//!
//! Nothing here is persisted except the frontier (it is recorded in each uberblock). The
//! free-extent list is an in-memory result of [`crate::Eofs::gc`]; after a remount it is
//! empty until `gc` is run again. Reuse is safe because GC only frees extents that are
//! unreachable from every retained root (committed root, pending root, and all snapshots),
//! so "never overwrite in place" still holds for every byte any root can reach.

use alloc::vec::Vec;

use crate::error::FsError;

/// A free extent: `len` bytes starting at `addr`, both multiples of the allocation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub addr: u64,
    pub len: u64,
}

/// The allocator: allocation unit, device capacity, frontier, and the free list.
#[derive(Debug, Clone)]
pub struct Allocator {
    alloc_unit: u64,
    device_size: u64,
    frontier: u64,
    /// Free extents sorted by address, non-overlapping, all below the frontier.
    free: Vec<Extent>,
}

impl Allocator {
    pub fn new(alloc_unit: u64, device_size: u64, frontier: u64) -> Allocator {
        Allocator {
            alloc_unit,
            device_size,
            frontier,
            free: Vec::new(),
        }
    }

    /// First never-allocated byte.
    pub fn frontier(&self) -> u64 {
        self.frontier
    }

    /// Round `len` up to the allocation unit.
    pub fn aligned(&self, len: u64) -> u64 {
        len.div_ceil(self.alloc_unit) * self.alloc_unit
    }

    /// Total bytes currently on the free list.
    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|e| e.len).sum()
    }

    /// Allocate room for `len` bytes (rounded up to the allocation unit): first fit from
    /// the free list, otherwise append at the frontier.
    pub fn allocate(&mut self, len: u64) -> Result<u64, FsError> {
        if len == 0 {
            return Err(FsError::Corrupt("zero-length allocation"));
        }
        let need = self.aligned(len);
        for i in 0..self.free.len() {
            if self.free[i].len >= need {
                let addr = self.free[i].addr;
                self.free[i].addr += need;
                self.free[i].len -= need;
                if self.free[i].len == 0 {
                    self.free.remove(i);
                }
                return Ok(addr);
            }
        }
        let addr = self.frontier;
        let end = addr.checked_add(need).ok_or(FsError::NoSpace)?;
        if end > self.device_size {
            return Err(FsError::NoSpace);
        }
        self.frontier = end;
        Ok(addr)
    }

    /// Replace the free list. `extents` must be sorted, non-overlapping, aligned, and below
    /// the frontier — [`crate::Eofs::gc`] constructs it that way.
    pub fn set_free(&mut self, extents: Vec<Extent>) {
        self.free = extents;
    }
}
