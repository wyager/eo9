//! DMA buffers shared by the device root providers (`eo9:pci`, `eo9:platform`).
//!
//! One implementation of the allocation + cache-coherence discipline, so every device
//! capability gets the board-proven brackets (the round-2 RTL8125 lesson — plan/09 D46:
//! descriptor writes sat in dirty D-cache lines and the NIC fetched stale DRAM) without
//! copy-paste drift:
//!
//! * **allocate** sweeps the zeroing writes to the PoC immediately, so the memset's
//!   dirty lines can never write back *over* device-written bytes later;
//! * **dma-write** sweeps after the copy — the device's next fetch reads DRAM, and the
//!   sweep's `dsb sy` doubles as the reference drivers' `dma_wmb()` (a doorbell register
//!   write cannot overtake the descriptor);
//! * **dma-read** sweeps before the copy — the invalidate drops stale lines so the load
//!   observes what the device wrote.
//!
//! All of it is a no-op where DMA is coherent (QEMU); see `arch::dma_coherence`.
//!
//! Buffers are plain kernel-heap allocations: with the identity map the CPU address *is*
//! the bus address, and the alignment is a page — enough for every virtio structure, the
//! OHCI HCCA's 256-byte requirement, and friendly to a future IOMMU mapping path.

use alloc::vec::Vec;

/// Per-allocation ceiling, so one call cannot take a huge bite out of the kernel heap
/// (the buffer is host memory, not guest linear memory).
pub const MAX_DMA_ALLOC_BYTES: u64 = 4 * 1024 * 1024;
/// Ceiling on live DMA buffers per task.
pub const MAX_DMA_BUFFERS: usize = 64;
/// DMA buffers are aligned to a page.
pub const DMA_ALIGN: usize = 4096;

/// One DMA-able allocation. The page-aligned window `[offset, offset + len)` inside
/// `storage` is what the guest sees; with the identity map its CPU address is also the
/// bus address the device DMAs to.
pub struct DmaBuffer {
    storage: Vec<u8>,
    offset: usize,
    len: usize,
}

impl DmaBuffer {
    pub fn allocate(len: usize) -> DmaBuffer {
        let storage = alloc::vec![0u8; len + DMA_ALIGN];
        let misalignment = storage.as_ptr() as usize % DMA_ALIGN;
        let offset = if misalignment == 0 {
            0
        } else {
            DMA_ALIGN - misalignment
        };
        let buffer = DmaBuffer {
            storage,
            offset,
            len,
        };
        // The zeroing above went through the (cacheable) heap mapping: push it to the
        // PoC NOW, or those dirty lines could write back LATER — over bytes the device
        // has DMA'd in the meantime (arch::dma_coherence docs; a no-op on coherent
        // machines).
        crate::arch::dma_coherence::sync(buffer.bus_address() as usize, len);
        buffer
    }

    pub fn bus_address(&self) -> u64 {
        (self.storage.as_ptr() as usize + self.offset) as u64
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn bytes(&self) -> &[u8] {
        &self.storage[self.offset..self.offset + self.len]
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.offset..self.offset + self.len]
    }

    /// Cache-maintain `[start, end)` of the window around a CPU access — clean +
    /// invalidate to the PoC, then barrier. Called BEFORE the copy on `dma-read` (drop
    /// stale lines so the load sees what the device wrote) and AFTER the copy on
    /// `dma-write` (the device's next fetch reads the bytes from DRAM; the sweep's
    /// `dsb sy` is also the reference drivers' `dma_wmb()` — a doorbell written through
    /// a later register write cannot overtake the descriptor). No-op where DMA is
    /// coherent (QEMU); see `arch::dma_coherence`.
    pub fn sync_range(&self, start: usize, end: usize) {
        crate::arch::dma_coherence::sync(self.bus_address() as usize + start, end - start);
    }
}

/// Bounds check for the DMA copy accessors; out of range traps (same contract as the
/// `eo9:io` buffer accessors, documented in both WITs).
pub fn dma_byte_range(
    total: usize,
    offset: u64,
    len: u64,
) -> Result<(usize, usize), wasmtime::Error> {
    let end = offset.checked_add(len);
    match end {
        Some(end) if end <= total as u64 => Ok((offset as usize, end as usize)),
        _ => Err(wasmtime::Error::msg(
            "dma-buffer access out of bounds (this traps, as the WIT documents)",
        )),
    }
}
