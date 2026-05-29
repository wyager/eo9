/*
 * Linker script for the Eo9 kernel on QEMU's riscv64 `virt` machine.
 *
 * RAM on `virt` starts at 0x8000_0000 and the first stretch of it belongs to OpenSBI (the
 * machine's default `-bios`, which runs in M-mode underneath the kernel and PMP-protects
 * itself), so the image is linked 2 MiB in (0x8020_0000) — the conventional next-stage
 * address, which QEMU's `-kernel` ELF loader honours along with the entry point recorded
 * here. QEMU places its device tree near the *top* of RAM on this machine; src/arch/riscv64
 * reserves the last 2 MiB so the heap stays clear of it (see mmu::HEAP_END).
 *
 * Everything from the end of the image up to that reservation is handed to the heap
 * allocator; see src/heap.rs.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x80200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : ALIGN(16) {
        /* Global pointer for linker relaxation: conventionally the small-data base + 0x800. */
        __global_pointer$ = . + 0x800;
        *(.sdata .sdata.*)
        *(.data .data.*)
    }

    .bss : ALIGN(16) {
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* Boot stack for the single hart: 512 KiB, growing down from __stack_top. */
    .stack (NOLOAD) : ALIGN(4096) {
        __stack_bottom = .;
        . += 512K;
        __stack_top = .;
    }

    . = ALIGN(4096);
    __heap_start = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.note .note.*)
    }
}
