//! Sv39 identity-mapped MMU setup for QEMU's riscv64 `virt` machine, with W^X for JIT code.
//!
//! RAM starts at 0x8000_0000. OpenSBI occupies (and PMP-protects) the first 2 MiB, so the
//! kernel is linked at 0x8020_0000 (linker-riscv64.ld); QEMU places its device tree near the
//! top of RAM (2 MiB-aligned), so the heap stops [`FDT_RESERVATION`] short of the top to
//! leave it readable.
//!
//! The map identity-maps the `virt` machine's MMIO window and DRAM (mirroring the aarch64
//! layout in src/arch/aarch64/mmu.rs):
//!
//! * `0x0000_0000..0x4000_0000` → one 1 GiB leaf "gigapage": read/write, non-executable.
//!   Covers the UART (0x1000_0000), PLIC (0x0c00_0000), the PCIe ECAM (0x3000_0000),
//!   Goldfish RTC and the test finisher.
//! * `0x4000_0000..0x8000_0000` → a second RW-NX gigapage: the 32-bit PCIe BAR window,
//!   where the `eo9:pci` provider places device registers when a driver opens a BAR
//!   (src/pci.rs).
//! * `0x8000_0000..0xa000_0000` → the 512 MiB of DRAM, mapped at **4 KiB page granularity**
//!   (root → one mid-level table → 256 leaf tables) so per-page permissions exist:
//!     - `[0x8000_0000, __kernel_start)` (OpenSBI's reservation; PMP-protected anyway) →
//!       read/write, non-executable.
//!     - `[__kernel_start, __heap_start)` (the kernel image + boot stack) → RWX (the kernel
//!       runs from here; trusted).
//!     - `[__heap_start, top of RAM)` (the heap and the device-tree reservation) →
//!       read/write, **non-executable** by default.
//! * everything else → unmapped (accesses fault, which is what we want).
//!
//! **W^X for on-target code.** Cranelift-emitted guest code lands in a heap allocation,
//! which is writable-but-non-executable by default — so it cannot be executed while being
//! written. When wasmtime publishes it (`wasm::BareMetalCodeMemory::publish_executable`),
//! the kernel orders the writes and synchronizes the instruction stream
//! ([`flush_code_range`]: `fence` + `fence.i`) and then flips those pages to
//! executable-and-read-only via [`set_range_permissions`]. So a code page is never
//! simultaneously writable and executable. (The kernel image itself is left RWX — internal
//! W^X for `.text`/`.data` is a further hardening; the JIT'd *guest* code is the threat
//! this addresses.)

use core::arch::asm;

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

const PAGE_SIZE: usize = 4096;
/// Number of 2 MiB mid-level entries needed to cover DRAM (= number of leaf tables).
const DRAM_MID_ENTRIES: usize = RAM_SIZE / (2 * 1024 * 1024); // 256

unsafe extern "C" {
    static __kernel_start: u8;
    static __heap_start: u8;
}

/// One 4 KiB translation table: 512 eight-byte entries.
#[repr(C, align(4096))]
struct TranslationTable([u64; 512]);

/// The Sv39 root table (VPN[2]; 1 GiB per entry; lives in `.bss`, zeroed at boot).
static mut ROOT_TABLE: TranslationTable = TranslationTable([0; 512]);
/// The mid-level table covering the DRAM gigabyte at 0x8000_0000 (VPN[1]; 2 MiB per entry).
static mut MID_TABLE: TranslationTable = TranslationTable([0; 512]);
/// 256 leaf tables, one per 2 MiB, covering the 512 MiB of DRAM at 4 KiB granularity.
static mut LEAF_TABLES: [TranslationTable; DRAM_MID_ENTRIES] =
    [const { TranslationTable([0; 512]) }; DRAM_MID_ENTRIES];

// Sv39 PTE bits.
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_G: u64 = 1 << 5;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

/// Common bits for every leaf entry: valid, global, and pre-set A/D so an implementation
/// without hardware A/D updating never takes the corresponding page faults.
const LEAF: u64 = PTE_V | PTE_G | PTE_A | PTE_D;
/// Read/write, non-executable (heap data, the OpenSBI area, the DTB reservation, devices).
const T_RW_NX: u64 = LEAF | PTE_R | PTE_W;
/// Read/write/execute (the kernel image + boot stack).
const T_RWX: u64 = LEAF | PTE_R | PTE_W | PTE_X;
/// Read-only, executable (published JIT code).
const T_RX_RO: u64 = LEAF | PTE_R | PTE_X;

/// `satp` MODE field for Sv39.
const SATP_MODE_SV39: u64 = 8 << 60;

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

/// The leaf PTE for an identity-mapped address with the given permission template.
const fn leaf_entry(pa: usize, template: u64) -> u64 {
    (((pa >> 12) as u64) << 10) | template
}

/// A non-leaf entry pointing at the next-level table at `table_pa`.
const fn table_entry(table_pa: usize) -> u64 {
    (((table_pa >> 12) as u64) << 10) | PTE_V
}

/// Build the Sv39 identity map and turn translation on. Called once, early in `kmain`, while
/// still executing from the identity-mapped kernel image (so enabling translation does not
/// move the program counter).
pub fn init() {
    let kernel_start = (&raw const __kernel_start).addr();
    let heap_start = (&raw const __heap_start).addr();

    let root = &raw mut ROOT_TABLE;
    let mid = &raw mut MID_TABLE;
    let leaves = &raw mut LEAF_TABLES;

    // SAFETY: the tables are only built here, once, on the single boot hart, before
    // translation is on; the entries below identity-map exactly the `virt` MMIO window and
    // DRAM with the per-region permissions described in the module docs.
    unsafe {
        // 4 KiB leaf entries for all of DRAM, choosing each page's permission by region.
        let mut mid_i = 0;
        while mid_i < DRAM_MID_ENTRIES {
            let mut leaf_i = 0;
            while leaf_i < 512 {
                let va = RAM_BASE + mid_i * (2 * 1024 * 1024) + leaf_i * PAGE_SIZE;
                let template = if va < kernel_start {
                    T_RW_NX // OpenSBI's PMP-protected reservation at the base of RAM
                } else if va < heap_start {
                    T_RWX // kernel image + boot stack
                } else {
                    T_RW_NX // heap (and the DTB reservation): writable, never executable
                };
                (*leaves)[mid_i].0[leaf_i] = leaf_entry(va, template);
                leaf_i += 1;
            }
            // Point this mid-level entry at its leaf table.
            let leaf_addr = (&raw const (*leaves)[mid_i]).addr();
            (*mid).0[mid_i] = table_entry(leaf_addr);
            mid_i += 1;
        }

        // Root: [0] the MMIO gigapage (UART, PLIC, RTC, PCIe ECAM, test finisher — never
        // executed); [1] the 32-bit PCIe BAR window 0x4000_0000..0x8000_0000, so registers
        // the eo9:pci provider assigns there are reachable (read/write, never executed);
        // [2] → the DRAM mid-level table (0x8000_0000 >> 30 == 2).
        (*root).0[0] = leaf_entry(0, T_RW_NX);
        (*root).0[1] = leaf_entry(0x4000_0000, T_RW_NX);
        (*root).0[2] = table_entry((&raw const *mid).addr());

        let satp = SATP_MODE_SV39 | (((&raw const *root).addr() as u64) >> 12);
        // Order the table writes before the walker can see them, switch satp, then flush
        // any translations cached while the tables were being built.
        asm!(
            "sfence.vma",
            "csrw satp, {satp}",
            "sfence.vma",
            satp = in(reg) satp,
            options(nostack, preserves_flags),
        );
    }
    crate::kprintln!(
        "mmu: Sv39 identity map enabled (MMIO + PCIe gigapages, DRAM 0x8000_0000..+512 MiB \
         at 4 KiB pages, heap W^X)"
    );
}

/// Set the page permissions of `[start, start+len)` (rounded out to page boundaries) within
/// the DRAM window. Used by the code publisher to flip freshly written code pages to
/// executable-read-only and back to writable-non-executable. Pages outside the mapped DRAM
/// window are ignored.
///
/// # Safety
/// The caller must own `[start, start+len)` and must not be executing from or writing to
/// those pages in a way that the new permission would violate (the publisher flips code to
/// read-only only after wasmtime has finished writing it, and back to writable only once it
/// is no longer executing).
#[allow(dead_code)] // used only by the feature-gated code publisher (see PagePerm)
pub unsafe fn set_range_permissions(start: usize, len: usize, perm: PagePerm) {
    if len == 0 {
        return;
    }
    let template = match perm {
        PagePerm::ReadWriteNoExec => T_RW_NX,
        PagePerm::ReadExecOnly => T_RX_RO,
    };
    let begin = start & !(PAGE_SIZE - 1);
    let end = (start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let leaves = &raw mut LEAF_TABLES;

    // Rewrite the leaf entries.
    let mut va = begin;
    while va < end {
        if (RAM_BASE..RAM_END).contains(&va) {
            let off = va - RAM_BASE;
            let mid_i = off >> 21;
            let leaf_i = (off >> 12) & 0x1ff;
            // SAFETY: mid_i < DRAM_MID_ENTRIES and leaf_i < 512 by construction; the tables
            // live in the kernel image's RW mapping, so writing them is permitted.
            unsafe { (*leaves)[mid_i].0[leaf_i] = leaf_entry(va, template) };
        }
        va += PAGE_SIZE;
    }

    // Order the table updates and drop the stale TLB entries for the range. `sfence.vma`
    // with a virtual address argument both orders the preceding stores to the tables and
    // invalidates cached translations for that page (all address spaces).
    va = begin;
    while va < end {
        if (RAM_BASE..RAM_END).contains(&va) {
            // SAFETY: TLB maintenance only.
            unsafe { asm!("sfence.vma {}, zero", in(reg) va, options(nostack, preserves_flags)) };
        }
        va += PAGE_SIZE;
    }
}

/// Make `[ptr, ptr+len)` coherent with the instruction-fetch path: order the stores that
/// wrote the code, then synchronize this hart's instruction stream (`fence.i`). Called by
/// the code publisher (src/wasm/mod.rs) before it flips the range executable.
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
