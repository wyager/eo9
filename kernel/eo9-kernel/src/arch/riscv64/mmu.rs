//! Memory layout and (for now) bare-mode "MMU" handling for QEMU's riscv64 `virt` machine.
//!
//! RAM starts at 0x8000_0000. OpenSBI occupies (and PMP-protects) the first part of it, so
//! the kernel is linked 2 MiB in (0x8020_0000, linker-riscv64.ld); QEMU places its device
//! tree near the top of RAM (2 MiB-aligned), so the heap stops [`FDT_RESERVATION`] short of
//! the top to leave it untouched.
//!
//! Translation stays off for this stage of the port (`satp` = Bare): under QEMU, RAM is
//! ordinary cacheable memory and misaligned accesses from Cranelift-generated code are
//! handled, so nothing forces paging the way the aarch64 Device-memory alignment rules did.
//! The cost is that W^X for published JIT code pages is not enforced yet —
//! [`set_range_permissions`] is a documented no-op until an Sv39 identity map mirroring the
//! aarch64 one lands (plan/12-kernel.md); [`flush_code_range`] still performs the required
//! instruction-stream synchronization.

/// Start of RAM on the QEMU `virt` machine.
const RAM_BASE: usize = 0x8000_0000;
/// RAM size the kernel assumes; must match the `-m` value in xtask's QEMU invocation.
const RAM_SIZE: usize = 512 * 1024 * 1024;
/// First byte past RAM.
const RAM_END: usize = RAM_BASE + RAM_SIZE;
/// Reservation at the top of RAM for QEMU's device tree (placed 2 MiB-aligned at the top).
const FDT_RESERVATION: usize = 2 * 1024 * 1024;
/// First byte past the heap (src/heap.rs): the top of RAM minus the device-tree reservation.
pub(crate) const HEAP_END: usize = RAM_END - FDT_RESERVATION;

/// Page permission a range may be set to after boot. Mirrors the aarch64 surface; with
/// translation off there is nothing to apply it to yet.
#[allow(dead_code)] // only used by the feature-gated code publisher
#[derive(Clone, Copy)]
pub enum PagePerm {
    /// Read/write, never executable (the heap default; a code page being written, or freed).
    ReadWriteNoExec,
    /// Read-only, executable (a published code page).
    ReadExecOnly,
}

/// Bring up whatever translation this stage of the port needs — currently nothing: the hart
/// stays in Bare mode. Says so honestly on the console (the aarch64 port prints its identity
/// map summary here).
pub fn init() {
    crate::kprintln!(
        "mmu: translation off (satp = Bare); W^X for published code is deferred to the Sv39 step"
    );
}

/// Set the page permissions of `[start, start+len)`. With translation off this is a no-op —
/// recorded as a known gap of the riscv64 port (plan/12-kernel.md) rather than silently
/// pretending W^X holds; the aarch64 port enforces it for real.
///
/// # Safety
/// The caller must own `[start, start+len)`; with the current Bare-mode implementation the
/// call has no effect.
#[allow(dead_code)] // only used by the feature-gated code publisher (see PagePerm)
pub unsafe fn set_range_permissions(_start: usize, _len: usize, _perm: PagePerm) {}

/// Make `[ptr, ptr+len)` coherent with the instruction-fetch path: order the stores that
/// wrote the code, then synchronize this hart's instruction stream (`fence.i`). Called by
/// the code publisher (src/wasm/mod.rs) before it would flip the range executable.
///
/// # Safety
/// `ptr`/`len` must describe a readable range that the caller owns; the ops are otherwise
/// side-effect-free.
#[allow(dead_code)] // only used by the feature-gated code publisher (see PagePerm)
pub unsafe fn flush_code_range(_ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: ordering and instruction-stream synchronization only; single hart, so a local
    // `fence.i` is sufficient (no remote harts to notify).
    unsafe {
        core::arch::asm!("fence rw, rw", "fence.i", options(nostack, preserves_flags));
    }
}
