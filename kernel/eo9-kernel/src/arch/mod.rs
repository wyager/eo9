//! The per-architecture layer: boot stub and trap vectors, serial console, interrupt
//! controller, timer, RTC, memory management, and power-off.
//!
//! Every architecture exposes the same surface — the `mmu`, `power`, `rtc`, `timer`, and
//! `uart` modules (re-exported at the crate root by src/main.rs so the shared core never
//! needs `target_arch` cfgs) plus [`banner`], [`interrupts_init`], and [`NAME`] — with the
//! same public function signatures. aarch64 (QEMU `virt`, GICv2) is the reference
//! implementation; riscv64 (QEMU `virt`, S-mode under OpenSBI, PLIC) follows it
//! (plan/12-kernel.md).

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::*;

#[cfg(not(target_arch = "aarch64"))]
compile_error!(
    "the Eo9 bare-metal kernel covers aarch64 so far (riscv64 is next; plan/12-kernel.md); \
     build for that target or on the host triple (where this crate is a stub)"
);
