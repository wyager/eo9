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
//! turns on the MMU and caches via [`crate::mmu::init`] before any wasm
//! code runs, because Cranelift-generated programs perform unaligned accesses that are
//! only legal on Normal memory.

use core::arch::global_asm;

// Board profile (`board-opi5plus`): the 64-byte arm64 Linux `Image` header U-Boot's
// `booti` requires on a flat binary, placed first in the image by the board linker
// script (`.text.header`), plus the EL2->EL1 entry drop. The RK3588's TF-A hands
// control to U-Boot at EL2 (boot log: `SPSR = 0x3c9` — EL2h), and `booti` enters the
// kernel at that level; this kernel runs at EL1, so the trampoline zeroes CNTVOFF_EL2
// (the virtual timer must equal the physical one — src/timer.rs uses CNTV), sets
// HCR_EL2.RW (EL1 is aarch64), disables the EL2 FP/CP15 traps, gives SCTLR_EL1 its
// canonical MMU-off reset value, and `eret`s to EL1h with interrupts masked. QEMU's
// `-kernel` loader enters at EL1 directly, so the trampoline also tolerates that.
#[cfg(feature = "board-opi5plus")]
global_asm!(
    r#"
// Boot-bisection beacon: bang one raw char at the DW UART2 THR (0xfeb5_0000, stride-4),
// polling LSR (+0x14) THRE (bit 5) first. Pre-MMU, pre-stack, EL2 or EL1 — the line is
// whatever U-Boot left (1.5 Mbaud 8n1). Clobbers x1/x2 only. The beacons stay in the
// final image: they cost nothing and the "ABC…" prefix reads as a boot signature.
.macro eo9_beacon ch
    movz    x1, #0xfeb5, lsl #16
10: ldr     w2, [x1, #0x14]
    tbz     w2, #5, 10b
    mov     w2, #\ch
    str     w2, [x1]
.endm

.section .text.header, "ax"
.globl _image_header
_image_header:
    b       _board_entry            // code0: jump over the header
    .word   0                       // code1
    .quad   0                       // text_offset: run at the DRAM bank base (0x0020_0000)
    .quad   __image_size            // image_size: kernel + .bss + boot stack
    .quad   0xA                     // flags: little-endian, 4 KiB pages, placement anywhere
    .quad   0                       // res2
    .quad   0                       // res3
    .quad   0                       // res4
    .ascii  "ARMd"               // magic
    .word   0                       // res5

.section .text.boot, "ax"
.globl _board_entry
_board_entry:
    // Preserve the DTB pointer (x0) across the drop; x20 is dead this early.
    mov     x20, x0

    // 'A': the literal first instructions after the loader's jump. If even A is missing,
    // the jump itself (address, image bytes, UART) is the problem, not any later stage.
    eo9_beacon 0x41

    // Clean+invalidate the whole kernel footprint ([__kernel_start, __image_end) — image
    // plus .bss and the boot stack) to the Point of Coherency, then drop the I-cache.
    //
    // Why: loaders make no PoC promise this kernel can rely on. The serial-loader stub
    // writes the payload through U-Boot's live EL2 D-cache; its pre-jump sweep is
    // `dc civac` (PoC) since 2026-06-07, but it was `dc cvau` (Point of Unification
    // only) before that, an already-running stub keeps its old sweep until re-poked,
    // and other transports (`booti`, mm-poke) promise nothing. This kernel then
    // `eret`s to EL1 with SCTLR_EL1 M/C/I = 0, where every fetch and data access goes
    // straight to DRAM (the PoC): any line still dirty above the PoC means stale DRAM
    // bytes — a silent wild jump. Sweeping to PoC here, while still on the boot path the
    // loader's own sweep made fetchable, makes the image self-coherent no matter how
    // it was loaded (under `booti` with caches off every op is a cheap clean-line no-op).
    // Covering .bss/.stack also evicts stale *dirty* lines left over from U-Boot's earlier
    // use of low DRAM, which could otherwise write back over our cache-off stores at any
    // moment (the EL1 pre-MMU world writes uncached; translation tables live in .bss).
    adrp    x1, __kernel_start
    add     x1, x1, :lo12:__kernel_start
    adrp    x2, __image_end
    add     x2, x2, :lo12:__image_end
    mrs     x3, ctr_el0
    ubfx    x3, x3, #16, #4         // DminLine: log2(words)
    mov     x4, #4
    lsl     x3, x4, x3              // D-cache line size in bytes
    sub     x4, x3, #1
    bic     x1, x1, x4              // align down to a line
11: dc      civac, x1               // clean+invalidate to PoC
    add     x1, x1, x3
    cmp     x1, x2
    b.lo    11b
    dsb     sy
    ic      iallu
    dsb     sy
    isb

    // 'b' if entered at EL1 directly; 'B' after the EL2->EL1 drop (banged at 9: below).
    mov     w21, #0x62
    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    9f                      // already EL1: continue
    mov     w21, #0x42
    // Configure EL1 from EL2, then drop.
    msr     cntvoff_el2, xzr        // virtual counter == physical counter
    mov     x1, #(1 << 31)          // HCR_EL2.RW: EL1 executes aarch64
    msr     hcr_el2, x1
    mov     x1, #0x33ff             // CPTR_EL2: no FP/SIMD trap to EL2 (RES1 pattern)
    msr     cptr_el2, x1
    msr     hstr_el2, xzr           // no CP15 traps
    mov     x1, #0x30d0
    lsl     x1, x1, #16
    movk    x1, #0x0800             // SCTLR_EL1 = 0x30D00800: RES1 bits, MMU/caches off
    msr     sctlr_el1, x1
    mov     x1, #0x3c5              // SPSR_EL2: EL1h, DAIF masked
    msr     spsr_el2, x1
    adr     x1, 9f
    msr     elr_el2, x1
    eret
9:  // 'B' (post-drop) or 'b' (entered at EL1): the EL2->EL1 trampoline survived.
    movz    x1, #0xfeb5, lsl #16
12: ldr     w2, [x1, #0x14]
    tbz     w2, #5, 12b
    str     w21, [x1]
    mov     x0, x20
    b       _start
"#
);

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

// IRQ vector: branch to the handler stub. Used for the "current EL" IRQ entries so the
// executor's `wfi` can be woken by the generic timer; every other vector stays fatal.
.macro eo9_irq_vector
    .p2align 7
    b       __irq_entry
.endm

.section .text.vectors, "ax"
.p2align 11
.globl __exception_vectors
__exception_vectors:
    eo9_vector 0       // current EL, SP_EL0: synchronous
    eo9_irq_vector     //                     IRQ
    eo9_vector 2       //                     FIQ
    eo9_vector 3       //                     SError
    eo9_vector 4       // current EL, SP_ELx: synchronous
    eo9_irq_vector     //                     IRQ
    eo9_vector 6       //                     FIQ
    eo9_vector 7       //                     SError
    eo9_vector 8       // lower EL, aarch64:  synchronous
    eo9_vector 9       //                     IRQ
    eo9_vector 10      //                     FIQ
    eo9_vector 11      //                     SError
    eo9_vector 12      // lower EL, aarch32:  synchronous
    eo9_vector 13      //                     IRQ
    eo9_vector 14      //                     FIQ
    eo9_vector 15      //                     SError

// IRQ handler stub. The interrupt can land in Cranelift-generated wasm code mid-computation,
// so we must not clobber anything that code owns. We save only the caller-saved integer
// registers x0-x18 plus x30 (the link register `bl` overwrites); the Rust handler `kirq`
// preserves x19-x29 per the procedure-call standard, and — being built without FP — never
// touches the v registers, so the interrupted code's SIMD/FP state is left intact. ELR_EL1
// and SPSR_EL1 already hold the return state and `kirq` does not touch them, so `eret`
// resumes the interrupted instruction stream exactly. IRQs are masked on entry, so the
// handler runs to completion without nesting.
.section .text, "ax"
.globl __irq_entry
__irq_entry:
    sub     sp, sp, #(16 * 10)
    stp     x0,  x1,  [sp, #(16 * 0)]
    stp     x2,  x3,  [sp, #(16 * 1)]
    stp     x4,  x5,  [sp, #(16 * 2)]
    stp     x6,  x7,  [sp, #(16 * 3)]
    stp     x8,  x9,  [sp, #(16 * 4)]
    stp     x10, x11, [sp, #(16 * 5)]
    stp     x12, x13, [sp, #(16 * 6)]
    stp     x14, x15, [sp, #(16 * 7)]
    stp     x16, x17, [sp, #(16 * 8)]
    stp     x18, x30, [sp, #(16 * 9)]
    bl      kirq
    ldp     x0,  x1,  [sp, #(16 * 0)]
    ldp     x2,  x3,  [sp, #(16 * 1)]
    ldp     x4,  x5,  [sp, #(16 * 2)]
    ldp     x6,  x7,  [sp, #(16 * 3)]
    ldp     x8,  x9,  [sp, #(16 * 4)]
    ldp     x10, x11, [sp, #(16 * 5)]
    ldp     x12, x13, [sp, #(16 * 6)]
    ldp     x14, x15, [sp, #(16 * 7)]
    ldp     x16, x17, [sp, #(16 * 8)]
    ldp     x18, x30, [sp, #(16 * 9)]
    add     sp, sp, #(16 * 10)
    eret
"#
);
