//! Identity-mapped MMU setup for QEMU's aarch64 `virt` machine, with W^X for JIT code.
//!
//! The spike ran with the MMU off, which works for the kernel itself (Rust for
//! `aarch64-unknown-none` is built with `+strict-align`) but not for Cranelift-generated
//! wasm code: with translation disabled every data access is Device-nGnRnE, and the
//! ordinary unaligned loads/stores compiled wasm programs perform take alignment faults.
//! Enabling the MMU with a flat identity map makes RAM Normal (cacheable) memory.
//!
//! The map identity-maps the `virt` machine's MMIO and DRAM windows:
//!
//! * `0x0000_0000..0x4000_0000` → Device-nGnRnE, non-executable (UART, RTC, GIC, …), one
//!   level-1 block.
//! * `0x4000_0000..0x6000_0000` → the 512 MiB of DRAM, mapped at **4 KiB page granularity**
//!   (level-1 → one level-2 table → 256 level-3 tables) so per-page permissions exist:
//!     - `[0x4000_0000, __kernel_start)` (the DTB area QEMU leaves at the base of RAM) →
//!       Normal RW, non-executable.
//!     - `[__kernel_start, __heap_start)` (the kernel image + boot stack) → Normal RW,
//!       executable at EL1 (the kernel runs from here; trusted).
//!     - `[__heap_start, 0x6000_0000)` (the heap) → Normal RW, **non-executable** by default.
//! * everything else → unmapped (accesses fault, which is what we want).
//!
//! **W^X for on-target code.** Cranelift-emitted guest code lands in a heap allocation, which
//! is writable-but-non-executable by default — so it cannot be executed while being written.
//! When wasmtime publishes it (`wasm::BareMetalCodeMemory::publish_executable`), the kernel
//! cleans D / invalidates I over the range (real cache maintenance for physical hardware;
//! QEMU's TCG keeps coherency anyway) and then flips those pages to executable-and-read-only
//! via [`set_range_permissions`]. So a code page is never simultaneously writable and
//! executable. (The kernel image itself is left RWX — internal W^X for `.text`/`.data` is a
//! further hardening; the JIT'd *guest* code is the threat this addresses.)

use core::arch::asm;

/// One 4 KiB translation table: 512 eight-byte descriptors.
#[repr(C, align(4096))]
struct TranslationTable([u64; 512]);

/// The single level-1 table (T0SZ=32 → 4 used entries; lives in `.bss`, zeroed at boot).
static mut LEVEL1_TABLE: TranslationTable = TranslationTable([0; 512]);
/// The level-2 table covering the 1 GiB at level-1 entry 1 (512 × 2 MiB).
static mut LEVEL2_TABLE: TranslationTable = TranslationTable([0; 512]);
/// 256 level-3 tables, one per 2 MiB, covering the 512 MiB of DRAM at 4 KiB granularity.
static mut LEVEL3_TABLES: [TranslationTable; DRAM_L2_ENTRIES] =
    [const { TranslationTable([0; 512]) }; DRAM_L2_ENTRIES];

const RAM_BASE: usize = 0x4000_0000;
const RAM_SIZE: usize = 512 * 1024 * 1024;
const RAM_END: usize = RAM_BASE + RAM_SIZE;
/// Number of 2 MiB level-2 entries needed to cover DRAM (= number of level-3 tables).
const DRAM_L2_ENTRIES: usize = RAM_SIZE / (2 * 1024 * 1024); // 256

const PAGE_SIZE: usize = 4096;

unsafe extern "C" {
    static __kernel_start: u8;
    static __heap_start: u8;
}

// Descriptor type bits.
const BLOCK: u64 = 0b01; // level-1/2 block
const TABLE: u64 = 0b11; // level-1/2 table descriptor (points to the next level)
const PAGE: u64 = 0b11; // level-3 page descriptor

// Memory-attribute / shareability / access bits (level-3 page descriptors).
const ATTR_INDEX_DEVICE: u64 = 0 << 2;
const ATTR_INDEX_NORMAL: u64 = 1 << 2;
const ACCESS_FLAG: u64 = 1 << 10;
const INNER_SHAREABLE: u64 = 0b11 << 8;
const AP_RO: u64 = 1 << 7; // AP[2]=1 → read-only (AP[1]=0 → no EL0 access either)
const PXN: u64 = 1 << 53; // privileged (EL1) execute-never
const UXN: u64 = 1 << 54; // unprivileged (EL0) execute-never

/// Common Normal-cacheable page bits (attr 1, access flag set, inner shareable).
const NORMAL: u64 = ATTR_INDEX_NORMAL | ACCESS_FLAG | INNER_SHAREABLE;
/// Read/write, non-executable (heap data, the DTB area).
const T_RW_NX: u64 = NORMAL | PXN | UXN;
/// Read/write, executable at EL1 (the kernel image).
const T_RWX: u64 = NORMAL | UXN;
/// Read-only, executable at EL1 (published JIT code).
const T_RX_RO: u64 = NORMAL | AP_RO | UXN;

// MAIR_EL1: attribute 0 = Device-nGnRnE (0x00), attribute 1 = Normal write-back R/W-allocate.
const MAIR_VALUE: u64 = 0xFF << 8;

// TCR_EL1: 32-bit VA from TTBR0 (T0SZ=32 → walk starts at level 1), 4 KiB granule, write-back
// inner-shareable walks, TTBR1 disabled, 40-bit physical addresses.
const TCR_VALUE: u64 = 32 | 0b01 << 8 | 0b01 << 10 | 0b11 << 12 | 1 << 23 | 0b010 << 32;

const SCTLR_MMU: u64 = 1;
const SCTLR_DCACHE: u64 = 1 << 2;
const SCTLR_ICACHE: u64 = 1 << 12;

/// Page permission a range may be set to after boot.
// Only used by the code publisher, which is behind the wasm runtime/codegen feature; the
// featureless CI kernel build compiles the MMU but not the publisher.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum PagePerm {
    /// Read/write, never executable (the heap default; a code page being written, or freed).
    ReadWriteNoExec,
    /// Read-only, executable at EL1 (a published code page).
    ReadExecOnly,
}

/// The level-3 page descriptor for an identity-mapped VA with the given attribute template.
const fn page_descriptor(va: usize, template: u64) -> u64 {
    (va as u64) | PAGE | template
}

/// Build the identity map and enable the MMU and caches. Called once, early in `kmain`, while
/// still executing from the identity-mapped kernel image (so enabling translation does not
/// move the program counter).
pub fn enable_identity_map() {
    let kernel_start = (&raw const __kernel_start).addr();
    let heap_start = (&raw const __heap_start).addr();

    let l1 = &raw mut LEVEL1_TABLE;
    let l2 = &raw mut LEVEL2_TABLE;
    let l3 = &raw mut LEVEL3_TABLES;

    // SAFETY: the tables are only built here, once, on the single boot core, before the MMU
    // is on; the descriptors below identity-map exactly the `virt` MMIO and DRAM windows with
    // the per-region attributes described in the module docs.
    unsafe {
        // Fill the 4 KiB page descriptors for all of DRAM, choosing each page's permission by
        // which region it falls in.
        let mut l2i = 0;
        while l2i < DRAM_L2_ENTRIES {
            let mut l3i = 0;
            while l3i < 512 {
                let va = RAM_BASE + l2i * (2 * 1024 * 1024) + l3i * PAGE_SIZE;
                let template = if va < kernel_start {
                    T_RW_NX // DTB area
                } else if va < heap_start {
                    T_RWX // kernel image + stack
                } else {
                    T_RW_NX // heap: writable, non-executable until code is published
                };
                (*l3)[l2i].0[l3i] = page_descriptor(va, template);
                l3i += 1;
            }
            // Point this level-2 entry at its level-3 table.
            let l3_addr = (&raw const (*l3)[l2i]).addr() as u64;
            (*l2).0[l2i] = l3_addr | TABLE;
            l2i += 1;
        }

        // Level-1: [0] MMIO device block (output 0, never executed); [1] → the level-2 table.
        (*l1).0[0] = BLOCK | ATTR_INDEX_DEVICE | ACCESS_FLAG | PXN | UXN;
        (*l1).0[1] = ((&raw const *l2).addr() as u64) | TABLE;

        let table_address = (&raw const *l1).addr() as u64;
        asm!(
            "dsb ishst",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr0}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "mrs {sctlr}, sctlr_el1",
            "orr {sctlr}, {sctlr}, {enable}",
            "msr sctlr_el1, {sctlr}",
            "isb",
            mair = in(reg) MAIR_VALUE,
            tcr = in(reg) TCR_VALUE,
            ttbr0 = in(reg) table_address,
            sctlr = out(reg) _,
            enable = in(reg) (SCTLR_MMU | SCTLR_DCACHE | SCTLR_ICACHE),
            options(nostack, preserves_flags),
        );
    }
    crate::kprintln!(
        "mmu: identity map enabled (device 0..1 GiB, DRAM 1..1.5 GiB at 4 KiB pages, \
         heap W^X, caches on)"
    );
}

/// Set the page permissions of `[start, start+len)` (rounded out to page boundaries) within
/// the DRAM window. Used by the code publisher to flip freshly written code pages to
/// executable-read-only and back to writable-non-executable. Pages outside the mapped DRAM
/// window are ignored.
///
/// # Safety
/// The caller must own `[start, start+len)` and must not be executing from or writing to those
/// pages in a way that the new permission would violate (the publisher flips code to
/// read-only only after wasmtime has finished writing it, and back to writable only once it is
/// no longer executing).
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
    let l3 = &raw mut LEVEL3_TABLES;

    // Rewrite the descriptors.
    let mut va = begin;
    while va < end {
        if (RAM_BASE..RAM_END).contains(&va) {
            let off = va - RAM_BASE;
            let l2i = off >> 21;
            let l3i = (off >> 12) & 0x1ff;
            // SAFETY: l2i < DRAM_L2_ENTRIES and l3i < 512 by construction; the tables are in
            // the RWX kernel-image region, so writing them is permitted.
            unsafe { (*l3)[l2i].0[l3i] = page_descriptor(va, template) };
        }
        va += PAGE_SIZE;
    }

    // Publish the table changes and drop the stale TLB entries for the range.
    // SAFETY: maintenance/barrier ops only.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    va = begin;
    while va < end {
        if (RAM_BASE..RAM_END).contains(&va) {
            // SAFETY: invalidate the unified TLB by VA (all ASID) at EL1 for this page.
            unsafe {
                asm!("tlbi vaae1, {}", in(reg) (va >> 12) as u64, options(nostack, preserves_flags))
            };
        }
        va += PAGE_SIZE;
    }
    // SAFETY: ordering + context synchronization so subsequent fetches/accesses see the new
    // permissions.
    unsafe { asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
}
