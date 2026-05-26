//! Assembly boot stub and exception vectors for aarch64 (QEMU `virt`).
//!
//! QEMU's `-kernel` ELF loader starts the boot CPU at `_start` at EL1 with the MMU and
//! caches off and all interrupts masked. The stub:
//!
//! 1. parks every core except core 0 (xtask boots QEMU single-core, but be safe),
//! 2. enables FP/SIMD access at EL1 (kernel code is built without FP, but wasm code
//!    compiled by Cranelift may use vector registers),
//! 3. installs the exception vector table so any unexpected synchronous exception or
//!    interrupt prints a register dump over serial instead of hanging silently,
//! 4. points the stack at `__stack_top` (linker script), zeroes `.bss`, and
//! 5. calls the Rust entry point [`kmain`](crate::kmain).
//!
//! The stub itself leaves the MMU off (the `aarch64-unknown-none` target builds all Rust
//! code with `+strict-align`, so that is safe); `kmain` then builds the identity map and
//! turns on the MMU and caches via [`crate::mmu::enable_identity_map`] before any wasm
//! code runs, because Cranelift-generated programs perform unaligned accesses that are
//! only legal on Normal memory.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    // Preserve the DTB pointer QEMU passes in x0 (callee-saved x19 survives the stub;
    // see crate::fdt for the consumer). Parked secondary cores never use it.
    mov     x19, x0
    // Park everything except core 0.
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbz     x0, 1f
0:  wfe
    b       0b
1:
    // Enable FP/SIMD at EL1 (CPACR_EL1.FPEN = 0b11) so Cranelift-generated code may use
    // vector registers without trapping.
    mov     x0, #(0x3 << 20)
    msr     cpacr_el1, x0
    isb

    // Exception vectors.
    adrp    x0, __exception_vectors
    add     x0, x0, :lo12:__exception_vectors
    msr     vbar_el1, x0
    isb

    // Boot stack.
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     sp, x0

    // Zero .bss.
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
2:  cmp     x1, x2
    b.hs    3f
    str     xzr, [x1], #8
    b       2b
3:  // Hand the preserved DTB pointer to the Rust entry point.
    mov     x0, x19
    bl      kmain
    // kmain never returns; if it somehow does, park the core.
4:  wfe
    b       4b

// Exception vector table: 16 entries of up to 32 instructions each, 2 KiB aligned.
// Every entry funnels into `kexception` (src/exceptions.rs) with the vector index and the
// relevant syndrome registers; the kernel treats any exception as fatal for now (wasm
// traps are explicit checks in generated code, not CPU exceptions, when signals-based
// traps are disabled).
.macro eo9_vector index
    .p2align 7
    mov     x0, #\index
    mrs     x1, esr_el1
    mrs     x2, elr_el1
    mrs     x3, far_el1
    b       kexception
.endm

.section .text.vectors, "ax"
.p2align 11
.globl __exception_vectors
__exception_vectors:
    eo9_vector 0   // current EL, SP_EL0: synchronous
    eo9_vector 1   //                     IRQ
    eo9_vector 2   //                     FIQ
    eo9_vector 3   //                     SError
    eo9_vector 4   // current EL, SP_ELx: synchronous
    eo9_vector 5   //                     IRQ
    eo9_vector 6   //                     FIQ
    eo9_vector 7   //                     SError
    eo9_vector 8   // lower EL, aarch64:  synchronous
    eo9_vector 9   //                     IRQ
    eo9_vector 10  //                     FIQ
    eo9_vector 11  //                     SError
    eo9_vector 12  // lower EL, aarch32:  synchronous
    eo9_vector 13  //                     IRQ
    eo9_vector 14  //                     FIQ
    eo9_vector 15  //                     SError
"#
);
