//! Memory management for the x86_64 port: the boot identity map, and the W^X surface the
//! shared code publisher expects.
//!
//! Long mode requires paging, so the boot stub (src/arch/x86_64/boot.rs) already runs the
//! kernel under a statically assembled identity map — 0..4 GiB as 2 MiB pages, present and
//! writable — covering RAM, the LAPIC/IOAPIC windows and the PCIe ECAM. This module keeps
//! that map as-is for milestones 1–2 (mirroring how the riscv64 port ran satp=Bare until its
//! codegen milestone): [`set_range_permissions`] is a documented no-op, because the boot
//! tables have neither 4 KiB granularity nor the NX bit enabled (EFER.NXE is off). The
//! on-target-codegen milestone replaces this with 4 KiB-granular tables + NXE so published
//! JIT code pages get the same write-xor-execute treatment as on aarch64 and riscv64.

/// RAM size the kernel assumes; must match the `-m` value in xtask's QEMU invocation.
const RAM_SIZE: usize = 512 * 1024 * 1024;
/// First byte past the heap (src/heap.rs): the top of RAM. The PVH start_info, command line
/// and firmware tables all live below 1 MiB — far below `__heap_start` — so no top-of-RAM
/// reservation is needed on this machine.
pub(crate) const HEAP_END: usize = RAM_SIZE;

/// Page permission a range may be set to after boot.
// Only used by the code publisher, which is behind the wasm runtime/codegen features; the
// featureless CI kernel build compiles the MMU but not the publisher.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum PagePerm {
    /// Read/write, never executable (the heap default; a code page being written, or freed).
    ReadWriteNoExec,
    /// Read-only, executable (a published code page).
    ReadExecOnly,
}

/// Report the translation regime the boot stub established. The map itself was built before
/// `kmain` ran (long mode cannot be entered without it), so there is nothing further to
/// switch on here.
pub fn init() {
    crate::kprintln!(
        "mmu: boot identity map active (0..4 GiB, 2 MiB pages, RWX); per-page W^X arrives \
         with the codegen milestone"
    );
}

/// Page-permission changes are deferred until the x86_64 port gains 4 KiB-granular tables
/// with NX (the codegen milestone); until then published code stays in the boot map's
/// writable+executable pages, exactly like the riscv64 port before its Sv39 step.
///
/// # Safety
/// The caller must own `[start, start+len)`; with the current single-permission boot map
/// there is nothing this call could violate.
#[allow(dead_code)] // used only by the feature-gated code publisher (see PagePerm)
pub unsafe fn set_range_permissions(_start: usize, _len: usize, _perm: PagePerm) {}

/// Make `[ptr, ptr+len)` coherent with the instruction-fetch path. x86 keeps instruction
/// fetch coherent with data stores on the same logical processor (and the publisher's jump
/// into freshly written code is a serializing control transfer), so nothing is required.
///
/// # Safety
/// `ptr`/`len` must describe a readable range that the caller owns; the call itself does
/// nothing.
#[allow(dead_code)] // only used by the feature-gated code publisher (see PagePerm)
pub unsafe fn flush_code_range(_ptr: *const u8, _len: usize) {}
