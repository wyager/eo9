//! Assembly boot path for x86_64 (QEMU `q35`, PVH direct boot).
//!
//! QEMU's `-kernel` loader recognises the `XEN_ELFNOTE_PHYS32_ENTRY` ELF note below and
//! boots the image through the PVH ("direct HVM") protocol: after firmware POST it jumps to
//! [`pvh_start`] in **32-bit protected mode** with flat segments, paging off, interrupts
//! masked, and `%ebx` holding the physical address of the `hvm_start_info` structure (which
//! carries the kernel command line and the memory map). No external bootloader is involved —
//! this was chosen over Multiboot because QEMU's Multiboot path only loads 32-bit ELFs,
//! while the PVH note lets the ordinary ELF64 image boot directly.
//!
//! The stub:
//!
//! 1. loads its own GDT (a 64-bit code descriptor and a flat data descriptor),
//! 2. points CR3 at a statically assembled identity map (0..4 GiB as 2 MiB pages — RAM and
//!    every MMIO window the kernel touches), enables PAE, long mode (EFER.LME), and paging,
//! 3. far-returns into 64-bit code, reloads the data segments, sets the boot stack, zeroes
//!    `.bss`, and
//! 4. calls [`boot_entry`], which validates the start_info magic and hands the PVH command
//!    line pointer to the shared Rust entry point [`kmain`](crate::kmain).
//!
//! Interrupt/trap entry stubs live in src/arch/x86_64/traps.rs; nothing is delivered until
//! `interrupts_init` builds the IDT and unmasks the lines it owns.

use core::arch::global_asm;

global_asm!(
    r#"
// ---------------------------------------------------------------------------------------
// PVH entry-point note: QEMU's -kernel loader reads this to find the 32-bit entry.
// namesz = 4 ("Xen\0"), descsz = 4 (one 32-bit physical address), type = 18
// (XEN_ELFNOTE_PHYS32_ENTRY).
// ---------------------------------------------------------------------------------------
.section .note.Xen, "a", @note
.balign 4
.long 4
.long 4
.long 18
.ascii "Xen\0"
.long pvh_start

// ---------------------------------------------------------------------------------------
// Boot GDT: null, 64-bit code (selector 0x08), flat data (selector 0x10). The accessed
// bits are pre-set so the CPU never needs to write the descriptors.
// ---------------------------------------------------------------------------------------
.section .rodata.boot_gdt, "a"
.balign 8
boot_gdt:
    .quad 0
    .quad 0x00209B0000000000
    .quad 0x0000930000000000
boot_gdt_end:
boot_gdt_descriptor:
    .word boot_gdt_end - boot_gdt - 1
    .long boot_gdt

// ---------------------------------------------------------------------------------------
// Statically assembled identity map: 0..4 GiB as 2 MiB pages, present + writable. Covers
// the 512 MiB of RAM, the LAPIC/IOAPIC windows and the PCIe ECAM. Long mode cannot be
// entered without paging, so this map exists only to reach `kmain`; mmu::init() then
// builds the runtime 4 KiB-granular tables (with NX + WP for W^X) and switches CR3 away
// from these.
// Flags: 0x83 = present | writable | page-size (2 MiB leaf); 0x03 = present | writable.
// ---------------------------------------------------------------------------------------
.section .data.boot_pagetables, "aw"
.balign 4096
boot_pml4:
    .quad boot_pdpt + 0x3
    .fill 511, 8, 0
.balign 4096
boot_pdpt:
    .quad boot_pd0 + 0x3
    .quad boot_pd1 + 0x3
    .quad boot_pd2 + 0x3
    .quad boot_pd3 + 0x3
    .fill 508, 8, 0
.balign 4096
boot_pd0:
    .set entry_paddr, 0x00000000
    .rept 512
    .quad entry_paddr + 0x83
    .set entry_paddr, entry_paddr + 0x200000
    .endr
.balign 4096
boot_pd1:
    .rept 512
    .quad entry_paddr + 0x83
    .set entry_paddr, entry_paddr + 0x200000
    .endr
.balign 4096
boot_pd2:
    .rept 512
    .quad entry_paddr + 0x83
    .set entry_paddr, entry_paddr + 0x200000
    .endr
.balign 4096
boot_pd3:
    .rept 512
    .quad entry_paddr + 0x83
    .set entry_paddr, entry_paddr + 0x200000
    .endr

// ---------------------------------------------------------------------------------------
// 32-bit PVH entry: GDT, identity map, PAE + long mode + paging, far return into 64-bit.
// %ebx (the hvm_start_info pointer) is preserved untouched through the whole sequence.
// ---------------------------------------------------------------------------------------
.section .text.boot, "ax"
.code32
.globl pvh_start
pvh_start:
    cli
    mov esp, offset __stack_top
    lgdt [boot_gdt_descriptor]
    // PAE.
    mov eax, cr4
    or eax, 0x20
    mov cr4, eax
    // Identity map.
    mov eax, offset boot_pml4
    mov cr3, eax
    // Long mode (EFER.LME).
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x100
    wrmsr
    // Paging on (CR0.PG | CR0.PE).
    mov eax, cr0
    or eax, 0x80000001
    mov cr0, eax
    // Far return into the 64-bit code segment (the entry address goes through a register so
    // the assembler emits a full 32-bit immediate rather than a 16-bit push).
    mov eax, offset long_mode_entry
    push 0x08
    push eax
    retf

.code64
long_mode_entry:
    // Flat data segments.
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    // Boot stack (the 32-bit esp value is fine, but reload it as a 64-bit pointer).
    mov rsp, offset __stack_top
    // Enable SSE: Cranelift-generated wasm code uses SSE2 (XMM registers) for any f32/f64
    // value, while the kernel's own Rust code is compiled soft-float (`x86_64-unknown-none`)
    // and never touches them — which is also why the trap entry does not need to save XMM
    // state. Clear CR0.EM (no x87 emulation) and CR0.TS (no lazy-switch trap), set CR0.MP,
    // and set CR4.OSFXSR | CR4.OSXMMEXCPT so SSE instructions execute natively in ring 0.
    mov rax, cr0
    and rax, -13
    or  rax, 0x2
    mov cr0, rax
    mov rax, cr4
    or  rax, 0x600
    mov cr4, rax
    // Zero .bss (both ends are 16-byte aligned by the linker script).
    lea rdi, [rip + __bss_start]
    lea rcx, [rip + __bss_end]
    sub rcx, rdi
    shr rcx, 3
    xor eax, eax
    rep stosq
    // hvm_start_info pointer (still in ebx) -> first argument; into Rust.
    mov edi, ebx
    call boot_entry
    // boot_entry never returns; if it somehow does, park the CPU.
2:  hlt
    jmp 2b
"#
);

/// `hvm_start_info.magic` ("xEn3" little-endian) — the PVH boot protocol's signature.
const HVM_START_INFO_MAGIC: u32 = 0x336e_c578;

/// Rust side of the boot stub: install the IDT (so any later exception produces a register
/// dump instead of a silent triple fault — the other ports install their trap vectors in
/// the boot stub too), validate the PVH start_info structure, and hand the kernel command
/// line (a plain NUL-terminated string, not a device tree) to the shared entry point. A
/// missing or unexpected structure simply means no command line.
#[unsafe(no_mangle)]
extern "C" fn boot_entry(start_info: *const u8) -> ! {
    super::traps::init();
    crate::kmain(pvh_cmdline(start_info))
}

/// The `cmdline_paddr` field of a valid `hvm_start_info`, or null.
fn pvh_cmdline(start_info: *const u8) -> *const u8 {
    if start_info.is_null() || !(start_info as usize).is_multiple_of(4) {
        return core::ptr::null();
    }
    // SAFETY: the PVH boot protocol passes a pointer to a readable hvm_start_info structure
    // in identity-mapped low RAM; the magic check below guards against anything else.
    let magic = unsafe { core::ptr::read_volatile(start_info as *const u32) };
    if magic != HVM_START_INFO_MAGIC {
        return core::ptr::null();
    }
    // SAFETY: offset 24 is `cmdline_paddr` in every published version of the structure.
    let cmdline_paddr = unsafe { core::ptr::read_volatile(start_info.add(24) as *const u64) };
    cmdline_paddr as *const u8
}
