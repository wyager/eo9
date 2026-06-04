/*
 * Linker script for the Eo9 kernel on the Orange Pi 5 Plus (RK3588), board profile
 * `board-opi5plus` (docs/board/orange-pi-5-plus.md).
 *
 * The board's first DRAM bank starts at 0x0020_0000 (bdinfo, verified on the board).
 * U-Boot's `booti` relocates a Linux-format Image to `bi_dram[0].start + text_offset`
 * (2017.09-era rule) — with text_offset 0 that is exactly 0x0020_0000, so the kernel is
 * linked there and `booti` either runs it in place (when `fatload` already put it there)
 * or memmoves it to the link address. Either way the bytes execute where they were linked.
 * TF-A's low reservations (BL31 pieces at 0x40000 and 0xf0000) sit below the link address;
 * U-Boot itself has relocated to the top of RAM by `booti` time, so 0x0020_0000 is free.
 *
 * The image opens with the 64-byte arm64 Linux boot header (`.text.header`, boot.rs) that
 * `booti` requires on a flat binary; QEMU's ELF loader path never sees this script.
 *
 * `__image_size` (header field) covers everything that must be reserved in RAM, including
 * .bss and the boot stack; the heap is found at runtime (src/heap.rs) above `__heap_start`.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x00200000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.header))
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

    __image_end = .;
    __image_size = __image_end - __kernel_start;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.note .note.*)
    }
}
