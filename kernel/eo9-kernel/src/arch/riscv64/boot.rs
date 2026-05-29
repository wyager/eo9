//! Assembly boot stub and trap entry for riscv64 (QEMU `virt`, S-mode under OpenSBI).
//!
//! QEMU's `-kernel` loader places the ELF at its linked address (0x8020_0000,
//! linker-riscv64.ld) and the bundled OpenSBI firmware (`-bios` default) enters it in
//! S-mode on the boot hart with `a0` = hartid and `a1` = the device-tree address.
//! Secondary harts stay parked in the SBI HSM (and xtask boots QEMU single-hart anyway).
//! The stub:
//!
//! 1. clears `sie` so nothing is delivered until [`super::interrupts_init`] runs,
//! 2. enables FP access (`sstatus.FS`): the `riscv64gc` kernel is built hard-float, and
//!    Cranelift-generated wasm code will use FP registers too,
//! 3. loads the global pointer for linker relaxation of small-data accesses,
//! 4. installs the trap vector (`stvec`, direct mode) so any trap funnels into
//!    [`super::traps`]' dispatcher with a register dump for the fatal cases,
//! 5. points the stack at `__stack_top` (linker script), zeroes `.bss`, and
//! 6. calls the Rust entry point [`kmain`](crate::kmain) with the DTB pointer.
//!
//! Translation stays off (`satp` = Bare) for now; see `super::mmu`.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    // a0 = boot hartid, a1 = DTB address (OpenSBI / QEMU boot protocol).
    // No supervisor interrupt source is enabled until kmain's interrupts_init.
    csrw    sie, zero
    // Enable FP (sstatus.FS = Initial, bit 13): the riscv64gc target is hard-float and
    // Cranelift-generated code uses FP registers as well.
    li      t0, 0x2000
    csrs    sstatus, t0
    // Global pointer for linker relaxation (must be loaded without relaxation itself).
.option push
.option norelax
    la      gp, __global_pointer$
.option pop
    // Trap vector, direct mode (all traps funnel through __trap_entry below).
    la      t0, __trap_entry
    csrw    stvec, t0
    // Boot stack.
    la      sp, __stack_top
    // Zero .bss (both ends are 16-byte aligned by the linker script).
    la      t0, __bss_start
    la      t1, __bss_end
1:  bgeu    t0, t1, 2f
    sd      zero, 0(t0)
    addi    t0, t0, 8
    j       1b
2:  // Hand the DTB pointer to the Rust entry point.
    mv      a0, a1
    call    kmain
    // kmain never returns; if it somehow does, park the hart.
3:  wfi
    j       3b

// Trap entry (stvec, direct mode). The trap may land in Cranelift-generated wasm code with
// live caller-saved state, and the riscv64gc kernel is itself built hard-float, so both the
// caller-saved integer registers and the caller-saved FP registers (plus fcsr) are saved
// before calling the Rust dispatcher `ktrap` (src/arch/riscv64/traps.rs) and restored before
// `sret`. Interrupts stay disabled inside the handler (sstatus.SIE is cleared on trap entry),
// so it runs to completion without nesting; fatal exceptions never return.
// Depending on optimization level the module-level asm can be assembled with a baseline
// feature set that lacks the F/D extensions (the Rust code itself is unaffected), so
// re-state them for the FP save/restore below.
.option push
.option arch, +f, +d
.p2align 2
.globl __trap_entry
__trap_entry:
    addi    sp, sp, -304
    sd      ra,   0(sp)
    sd      t0,   8(sp)
    sd      t1,  16(sp)
    sd      t2,  24(sp)
    sd      t3,  32(sp)
    sd      t4,  40(sp)
    sd      t5,  48(sp)
    sd      t6,  56(sp)
    sd      a0,  64(sp)
    sd      a1,  72(sp)
    sd      a2,  80(sp)
    sd      a3,  88(sp)
    sd      a4,  96(sp)
    sd      a5, 104(sp)
    sd      a6, 112(sp)
    sd      a7, 120(sp)
    fsd     ft0,  128(sp)
    fsd     ft1,  136(sp)
    fsd     ft2,  144(sp)
    fsd     ft3,  152(sp)
    fsd     ft4,  160(sp)
    fsd     ft5,  168(sp)
    fsd     ft6,  176(sp)
    fsd     ft7,  184(sp)
    fsd     fa0,  192(sp)
    fsd     fa1,  200(sp)
    fsd     fa2,  208(sp)
    fsd     fa3,  216(sp)
    fsd     fa4,  224(sp)
    fsd     fa5,  232(sp)
    fsd     fa6,  240(sp)
    fsd     fa7,  248(sp)
    fsd     ft8,  256(sp)
    fsd     ft9,  264(sp)
    fsd     ft10, 272(sp)
    fsd     ft11, 280(sp)
    csrr    t0, fcsr
    sd      t0, 288(sp)
    csrr    a0, scause
    csrr    a1, sepc
    csrr    a2, stval
    call    ktrap
    ld      t0, 288(sp)
    csrw    fcsr, t0
    fld     ft0,  128(sp)
    fld     ft1,  136(sp)
    fld     ft2,  144(sp)
    fld     ft3,  152(sp)
    fld     ft4,  160(sp)
    fld     ft5,  168(sp)
    fld     ft6,  176(sp)
    fld     ft7,  184(sp)
    fld     fa0,  192(sp)
    fld     fa1,  200(sp)
    fld     fa2,  208(sp)
    fld     fa3,  216(sp)
    fld     fa4,  224(sp)
    fld     fa5,  232(sp)
    fld     fa6,  240(sp)
    fld     fa7,  248(sp)
    fld     ft8,  256(sp)
    fld     ft9,  264(sp)
    fld     ft10, 272(sp)
    fld     ft11, 280(sp)
    ld      ra,   0(sp)
    ld      t0,   8(sp)
    ld      t1,  16(sp)
    ld      t2,  24(sp)
    ld      t3,  32(sp)
    ld      t4,  40(sp)
    ld      t5,  48(sp)
    ld      t6,  56(sp)
    ld      a0,  64(sp)
    ld      a1,  72(sp)
    ld      a2,  80(sp)
    ld      a3,  88(sp)
    ld      a4,  96(sp)
    ld      a5, 104(sp)
    ld      a6, 112(sp)
    ld      a7, 120(sp)
    addi    sp, sp, 304
    sret
.option pop
"#
);
