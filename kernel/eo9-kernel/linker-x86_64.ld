/*
 * Linker script for the Eo9 kernel on QEMU's x86_64 `q35` machine.
 *
 * The image boots through QEMU's PVH direct-boot path: `-kernel` notices the
 * XEN_ELFNOTE_PHYS32_ENTRY note (src/arch/x86_64/boot.rs) and, after firmware POST, jumps
 * to that 32-bit entry point with paging off and `%ebx` pointing at the PVH start_info
 * structure. The image is linked at 2 MiB physical — comfortably above the legacy/firmware
 * region below 1 MiB, inside the 512 MiB of RAM xtask's QEMU invocation provides — and the
 * boot stub's identity map keeps virtual == physical, so a single load address serves both.
 *
 * The PVH note must end up in a PT_NOTE program header for QEMU's loader to find it, which
 * is why this script declares PHDRS explicitly (one RWX load segment for the whole image,
 * plus the note segment). Everything from the end of the image to mmu::HEAP_END is handed
 * to the heap allocator; see src/heap.rs.
 */

ENTRY(pvh_start)

PHDRS
{
    text PT_LOAD FLAGS(7);   /* read + write + execute: one segment for the whole image */
    note PT_NOTE FLAGS(4);   /* the PVH entry-point note */
}

SECTIONS
{
    . = 0x200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    } :text

    .note.Xen : {
        KEEP(*(.note.Xen))
    } :text :note

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
        /* PIC-compiled prebuilt core/alloc keep relocated constants here; the values are
           resolved at link time, so for this fixed-address image it is ordinary rodata. */
        *(.data.rel.ro .data.rel.ro.*)
    } :text

    .data : ALIGN(16) {
        *(.data .data.*)
    } :text

    /* The prebuilt core/alloc rlibs are position-independent and reach a few symbols
       (memcpy and friends) through a global offset table. The entries are filled in at
       link time, but the section must be placed explicitly — an orphan section would land
       after __heap_start, where the heap allocator would clobber it. */
    .got : ALIGN(16) {
        *(.got .got.plt)
    } :text

    .bss : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    } :text

    /* Boot stack for the single CPU: 512 KiB, growing down from __stack_top. */
    .stack (NOLOAD) : ALIGN(4096) {
        __stack_bottom = .;
        . += 512K;
        __stack_top = .;
    } :text

    . = ALIGN(4096);
    __heap_start = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.comment)
    }
}
