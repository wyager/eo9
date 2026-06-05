/* The serial-loader stub runs in place at 0x0400_0000 — far above the kernel payload
 * window (load_addr 0x0020_0000 + up to ~62 MiB stays below us only for images under
 * 0x03E0_0000; the stub guards the overlap at runtime) and far below U-Boot's relocated
 * home near the top of the first DRAM bank. The Image header's text_offset would make a
 * booti relocation a no-op (0x0020_0000 + 0x03E0_0000 == 0x0400_0000), but vendor booti
 * data-aborts on this image (BOOT.md incident) — launch with `go 0x04000000` only. */
ENTRY(_head)
SECTIONS
{
    . = 0x04000000;

    .text : {
        KEEP(*(.text.head))
        *(.text*)
    }
    .rodata : { *(.rodata*) }
    .data : { *(.data*) }

    . = ALIGN(16);
    __bss_start = .;
    .bss (NOLOAD) : {
        *(.bss*)
        *(COMMON)
        . = ALIGN(16);
        /* 8 KiB stub stack, zeroed along with the rest of the bss. */
        . += 0x2000;
        . = ALIGN(16);
        _stack_top = .;
    }
    __bss_end = _stack_top;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
