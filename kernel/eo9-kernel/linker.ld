/*
 * Linker script for the Eo9 kernel on QEMU's aarch64 `virt` machine.
 *
 * RAM on `virt` starts at 0x4000_0000. QEMU places the device tree blob at the very start
 * of RAM when booting with `-kernel`, so the image is linked 2 MiB in (0x4020_0000) to stay
 * clear of it. The kernel is loaded as an ELF by QEMU's `-kernel` loader, which honours the
 * load addresses and entry point recorded here, so no separate objcopy step is needed.
 *
 * Everything from the end of the image up to the top of RAM (fixed at 512 MiB by xtask's
 * QEMU invocation) is handed to the heap allocator; see src/heap.rs.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x40200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16) {
        *(.data .data.*)
    }

    .bss : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* Boot stack for the single core: 512 KiB, growing down from __stack_top. */
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
