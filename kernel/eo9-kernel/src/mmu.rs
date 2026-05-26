//! Identity-mapped MMU setup for QEMU's aarch64 `virt` machine.
//!
//! The spike ran with the MMU off, which works for the kernel itself (Rust for
//! `aarch64-unknown-none` is built with `+strict-align`) but not for Cranelift-generated
//! wasm code: with translation disabled every data access is Device-nGnRnE, and the
//! ordinary unaligned loads/stores that compiled wasm programs perform take alignment
//! faults. Enabling the MMU with a flat identity map makes RAM Normal (cacheable) memory —
//! unaligned accesses become legal, and the machine behaves like the hardware any real
//! kernel would set up anyway.
//!
//! The map is deliberately tiny: one level-1 table with 1 GiB block entries covering the
//! first 4 GiB of the address space —
//!
//! * `0x0000_0000..0x4000_0000` → Device-nGnRnE, non-executable (UART, RTC, GIC, …)
//! * `0x4000_0000..0x8000_0000` → Normal write-back cacheable RAM (the `virt` DRAM window
//!   that holds the kernel, heap, and DTB)
//! * everything else → unmapped (accesses fault, which is what we want)
//!
//! Caveat carried from the spike: with caches now on, publishing freshly written code
//! would need D-cache clean + I-cache invalidate on real hardware; QEMU's TCG keeps the
//! instruction stream coherent with memory writes, so the kernel only issues barriers
//! (see `wasm::BareMetalCodeMemory`). That maintenance is a prerequisite for running on
//! physical machines, not for this milestone.

use core::arch::asm;

/// One 4 KiB translation table: 512 eight-byte descriptors.
#[repr(C, align(4096))]
struct TranslationTable([u64; 512]);

/// The single level-1 table (lives in `.bss`, zeroed by the boot stub).
static mut LEVEL1_TABLE: TranslationTable = TranslationTable([0; 512]);

// Descriptor fields for level-1 block entries.
const BLOCK: u64 = 0b01;
const ATTR_INDEX_DEVICE: u64 = 0 << 2;
const ATTR_INDEX_NORMAL: u64 = 1 << 2;
const ACCESS_FLAG: u64 = 1 << 10;
const INNER_SHAREABLE: u64 = 0b11 << 8;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

// MAIR_EL1: attribute 0 = Device-nGnRnE (0x00), attribute 1 = Normal write-back
// read/write-allocate (0xFF).
const MAIR_VALUE: u64 = 0xFF << 8;

// TCR_EL1: 32-bit VA space from TTBR0 (T0SZ = 32, so the walk starts at level 1 with a
// four-entry table), 4 KiB granule (TG0 = 0), write-back inner-shareable walks, TTBR1
// disabled, 40-bit physical addresses.
const TCR_VALUE: u64 = 32          // T0SZ
    | 0b01 << 8                    // IRGN0: write-back write-allocate
    | 0b01 << 10                   // ORGN0: write-back write-allocate
    | 0b11 << 12                   // SH0: inner shareable
    | 1 << 23                      // EPD1: no TTBR1 walks
    | 0b010 << 32; // IPS: 40-bit physical address space

// SCTLR_EL1 bits to turn on: MMU, data cache, instruction cache.
const SCTLR_MMU: u64 = 1;
const SCTLR_DCACHE: u64 = 1 << 2;
const SCTLR_ICACHE: u64 = 1 << 12;

/// Build the identity map and enable the MMU and caches. Called once, early in `kmain`,
/// while still executing from the identity-mapped kernel image (so enabling translation
/// does not move the program counter).
pub fn enable_identity_map() {
    let table = &raw mut LEVEL1_TABLE;
    // SAFETY: the table is only touched here, once, on the single boot core, before the
    // MMU is on; the descriptor values below identity-map exactly the `virt` machine's
    // MMIO window and DRAM window with the attributes described in the module docs.
    unsafe {
        // [0]: 0x0000_0000..0x4000_0000 — MMIO (output address 0). Device memory, never
        // executed.
        (*table).0[0] = BLOCK | ATTR_INDEX_DEVICE | ACCESS_FLAG | PXN | UXN;
        // [1]: 0x4000_0000..0x8000_0000 — DRAM. Normal cacheable, executable (wasm code
        // is published into the heap).
        (*table).0[1] = 0x4000_0000 | BLOCK | ATTR_INDEX_NORMAL | ACCESS_FLAG | INNER_SHAREABLE;

        let table_address = table as u64;
        asm!(
            // Make the table writes visible to the walker, then point the MMU at it.
            "dsb ishst",
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr0}",
            "isb",
            // No stale TLB entries (the MMU was off, but be thorough).
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            // Turn on translation and both caches.
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
    crate::kprintln!("mmu: identity map enabled (device 0..1 GiB, normal RAM 1..2 GiB, caches on)");
}
