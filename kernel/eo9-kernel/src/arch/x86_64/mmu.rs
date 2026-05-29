//! Runtime page tables for the x86_64 port, with W^X for published JIT code.
//!
//! Long mode requires paging, so the boot stub (src/arch/x86_64/boot.rs) enters the kernel
//! under a statically assembled identity map (0..4 GiB as 2 MiB RWX pages). That map has
//! neither 4 KiB granularity nor the NX bit, so [`init`] replaces it: it builds an identity
//! map with per-page permissions, enables `EFER.NXE` (so the NX bit is honoured) and
//! `CR0.WP` (so ring 0 honours read-only pages), and switches `CR3` over. The boot tables
//! are only used for the few microseconds it takes to reach `kmain`.
//!
//! The runtime map (mirroring the aarch64/riscv64 layouts in src/arch/{aarch64,riscv64}/mmu.rs):
//!
//! * `0x0000_0000..0x2000_0000` — the 512 MiB of RAM, mapped at **4 KiB page granularity**
//!   (one page directory → 256 leaf tables) so per-page permissions exist:
//!     - `[0, __kernel_start)` (the real-mode/firmware area, the PVH start_info and command
//!       line, SeaBIOS structures) → read/write, non-executable.
//!     - `[__kernel_start, __heap_start)` (the kernel image, boot stack and these tables) →
//!       RWX (the kernel runs from here; trusted, like the other ports).
//!     - `[__heap_start, top of RAM)` (the heap) → read/write, **non-executable** by default.
//! * `0x2000_0000..0x1_0000_0000` — the rest of the first 4 GiB as 2 MiB pages, read/write,
//!   non-executable: nothing up there is RAM, but the LAPIC/IOAPIC windows and the q35 PCIe
//!   ECAM live in this range, so keep it reachable as device memory.
//! * everything above 4 GiB → unmapped (accesses fault, which is what we want).
//!
//! **W^X for on-target code.** Cranelift-emitted (or deserialized) guest code lands in a
//! heap allocation, which is writable-but-non-executable by default — so it cannot be
//! executed while wasmtime is writing it. When wasmtime publishes it
//! (`wasm::BareMetalCodeMemory::publish_executable`), the kernel flips those pages to
//! executable-and-read-only via [`set_range_permissions`]; unpublishing flips them back. So
//! a code page is never simultaneously writable and executable. `CR0.WP` makes the
//! read-only half real even in ring 0. (The kernel image itself stays RWX — internal W^X
//! for `.text`/`.rodata` is a further hardening; the JIT'd *guest* code is the threat this
//! addresses, exactly as on the other two ports.)

use core::arch::asm;

/// RAM size the kernel assumes; must match the `-m` value in xtask's QEMU invocation.
const RAM_SIZE: usize = 512 * 1024 * 1024;
/// First byte past the heap (src/heap.rs): the top of RAM. The PVH start_info, command line
/// and firmware tables all live below 1 MiB — far below `__heap_start` — so no top-of-RAM
/// reservation is needed on this machine.
pub(crate) const HEAP_END: usize = RAM_SIZE;

const PAGE_SIZE: usize = 4096;
/// Number of 2 MiB page-directory entries needed to cover RAM (= number of leaf tables).
const RAM_PD_ENTRIES: usize = RAM_SIZE / (2 * 1024 * 1024); // 256
/// 2 MiB page-directory entries per directory.
const PD_ENTRIES: usize = 512;

unsafe extern "C" {
    static __kernel_start: u8;
    static __heap_start: u8;
}

/// One 4 KiB translation table: 512 eight-byte entries.
#[repr(C, align(4096))]
struct TranslationTable([u64; 512]);

/// PML4 (one entry used: [0] → the first 512 GiB).
static mut PML4: TranslationTable = TranslationTable([0; 512]);
/// PDPT for the first 512 GiB (entries [0..4) → the first 4 GiB).
static mut PDPT: TranslationTable = TranslationTable([0; 512]);
/// Page directories for the first 4 GiB (PD[0] covers RAM, PD[1..4) are 2 MiB device maps).
static mut PDS: [TranslationTable; 4] = [const { TranslationTable([0; 512]) }; 4];
/// 256 leaf tables, one per 2 MiB, covering the 512 MiB of RAM at 4 KiB granularity.
static mut LEAF_TABLES: [TranslationTable; RAM_PD_ENTRIES] =
    [const { TranslationTable([0; 512]) }; RAM_PD_ENTRIES];

// x86_64 paging bits.
const PTE_P: u64 = 1 << 0;
const PTE_RW: u64 = 1 << 1;
const PTE_A: u64 = 1 << 5;
const PTE_D: u64 = 1 << 6;
const PTE_PS: u64 = 1 << 7;
const PTE_NX: u64 = 1 << 63;

/// Common bits for every 4 KiB leaf entry: present, with A/D pre-set so the CPU never has
/// to write the tables itself.
const LEAF: u64 = PTE_P | PTE_A | PTE_D;
/// Read/write, non-executable (low RAM, the heap, device windows).
const T_RW_NX: u64 = LEAF | PTE_RW | PTE_NX;
/// Read/write/execute (the kernel image, stack and these tables).
const T_RWX: u64 = LEAF | PTE_RW;
/// Read-only, executable (published JIT code).
const T_RX_RO: u64 = LEAF;
/// A non-leaf entry pointing at the next-level table: present, writable, accessed.
const TABLE: u64 = PTE_P | PTE_RW | PTE_A;

/// `EFER` MSR and its NXE bit (the LME bit was set by the boot stub).
const MSR_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;
/// `CR0.WP`: ring 0 honours read-only pages (what makes published code truly read-only).
const CR0_WP: u64 = 1 << 16;

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
    pa as u64 | template
}

/// A non-leaf entry pointing at the next-level table at `table_pa`.
const fn table_entry(table_pa: usize) -> u64 {
    table_pa as u64 | TABLE
}

/// Build the runtime identity map and switch to it. Called once, early in `kmain`, while
/// still executing from the identity-mapped kernel image (so reloading `CR3` does not move
/// the program counter). Also enables `EFER.NXE` (the boot map sets no NX bits, so turning
/// it on first is harmless) and `CR0.WP`.
pub fn init() {
    let kernel_start = (&raw const __kernel_start).addr();
    let heap_start = (&raw const __heap_start).addr();

    let pml4 = &raw mut PML4;
    let pdpt = &raw mut PDPT;
    let pds = &raw mut PDS;
    let leaves = &raw mut LEAF_TABLES;

    // SAFETY: the tables are only built here, once, on the single boot CPU, while the boot
    // identity map is still active; the entries identity-map exactly the first 4 GiB with
    // the per-region permissions described in the module docs, and the switch-over below
    // happens while executing from the (identically mapped) kernel image.
    unsafe {
        // 4 KiB leaf entries for all of RAM, choosing each page's permission by region.
        let mut pd_i = 0;
        while pd_i < RAM_PD_ENTRIES {
            let mut leaf_i = 0;
            while leaf_i < 512 {
                let va = pd_i * (2 * 1024 * 1024) + leaf_i * PAGE_SIZE;
                let template = if va < kernel_start {
                    T_RW_NX // firmware / PVH structures below the image: data only
                } else if va < heap_start {
                    T_RWX // kernel image + boot stack + these tables
                } else {
                    T_RW_NX // heap: writable, never executable
                };
                (*leaves)[pd_i].0[leaf_i] = leaf_entry(va, template);
                leaf_i += 1;
            }
            // Point this page-directory entry at its leaf table.
            let leaf_addr = (&raw const (*leaves)[pd_i]).addr();
            (*pds)[0].0[pd_i] = table_entry(leaf_addr);
            pd_i += 1;
        }
        // The rest of PD[0] (512 MiB .. 1 GiB) and PD[1..4) (1 .. 4 GiB): 2 MiB device pages,
        // read/write, never executable.
        let mut entry = RAM_PD_ENTRIES;
        while entry < PD_ENTRIES {
            let pa = entry * (2 * 1024 * 1024);
            (*pds)[0].0[entry] = leaf_entry(pa, T_RW_NX | PTE_PS);
            entry += 1;
        }
        let mut pd = 1;
        while pd < 4 {
            let mut entry = 0;
            while entry < PD_ENTRIES {
                let pa = pd * (1024 * 1024 * 1024) + entry * (2 * 1024 * 1024);
                (*pds)[pd].0[entry] = leaf_entry(pa, T_RW_NX | PTE_PS);
                entry += 1;
            }
            pd += 1;
        }

        // PDPT[0..4) → the four page directories; PML4[0] → the PDPT.
        let mut pd = 0;
        while pd < 4 {
            (*pdpt).0[pd] = table_entry((&raw const (*pds)[pd]).addr());
            pd += 1;
        }
        (*pml4).0[0] = table_entry((&raw const *pdpt).addr());

        // Enable NXE (so the NX bits above are honoured rather than reserved-bit faults) and
        // WP (so ring 0 honours read-only pages), then switch CR3. A MOV to CR3 is a
        // serializing instruction, so the table writes above are globally visible before any
        // translation can use them, and the full (non-global) TLB is flushed.
        let (efer_lo, efer_hi): (u32, u32);
        asm!("rdmsr", in("ecx") MSR_EFER, out("eax") efer_lo, out("edx") efer_hi,
             options(nostack, preserves_flags));
        let efer = ((efer_hi as u64) << 32 | efer_lo as u64) | EFER_NXE;
        asm!("wrmsr", in("ecx") MSR_EFER, in("eax") efer as u32,
             in("edx") (efer >> 32) as u32, options(nostack, preserves_flags));
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 |= CR0_WP;
        asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
        asm!("mov cr3, {}", in(reg) (&raw const *pml4).addr() as u64,
             options(nostack, preserves_flags));
    }
    crate::kprintln!(
        "mmu: runtime identity map enabled (RAM 0..512 MiB at 4 KiB pages, heap W^X, NXE+WP; \
         devices to 4 GiB as 2 MiB pages)"
    );
}

/// Set the page permissions of `[start, start+len)` (rounded out to page boundaries) within
/// the RAM window. Used by the code publisher to flip freshly written code pages to
/// executable-read-only and back to writable-non-executable. Pages outside the 4 KiB-mapped
/// RAM window are ignored.
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

    let mut va = begin;
    while va < end {
        if va < RAM_SIZE {
            let pd_i = va >> 21;
            let leaf_i = (va >> 12) & 0x1ff;
            // SAFETY: pd_i < RAM_PD_ENTRIES and leaf_i < 512 by construction (va < RAM_SIZE);
            // the tables live in the kernel image's writable mapping. The PTE store is
            // ordered before the `invlpg` below by x86's store ordering, and `invlpg` drops
            // the stale translation for exactly this page.
            unsafe {
                (*leaves)[pd_i].0[leaf_i] = leaf_entry(va, template);
                asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
            }
        }
        va += PAGE_SIZE;
    }
}

/// Make `[ptr, ptr+len)` coherent with the instruction-fetch path. x86 keeps instruction
/// fetch coherent with data stores on the same logical processor, and the publisher's later
/// jump into the freshly written (and just remapped) code is a serializing control transfer
/// preceded by the serializing/invalidating page-permission flip — so nothing is required
/// here, unlike the explicit cache maintenance on aarch64 (`dc cvau`/`ic ivau`) and riscv64
/// (`fence.i`).
///
/// # Safety
/// `ptr`/`len` must describe a readable range that the caller owns; the call itself does
/// nothing.
#[allow(dead_code)] // only used by the feature-gated code publisher (see PagePerm)
pub unsafe fn flush_code_range(_ptr: *const u8, _len: usize) {}
