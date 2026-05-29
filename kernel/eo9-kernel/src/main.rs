//! The Eo9 bare-metal kernel (plan/12-kernel.md): aarch64 and riscv64 on QEMU `virt`,
//! x86_64 on QEMU `q35`.
//!
//! Boot path: QEMU's `-kernel` loader reads the ELF produced by `cargo xtask build-kernel
//! <arch>` and jumps to the per-architecture entry (src/arch/<arch>/boot.rs) — `_start` at
//! EL1 on aarch64, `_start` in S-mode (entered from OpenSBI) on riscv64, and the PVH
//! 32-bit entry `pvh_start` on x86_64. The per-architecture stub parks
//! secondary cores, enables FP/SIMD for later wasm code, installs the trap vectors, sets up
//! the boot stack, zeroes `.bss`, and calls [`kmain`]. From there everything is shared
//! Rust — serial output, a global heap (no_std + alloc), the platform timer and RTC, and,
//! behind the `wasm-*` features, a wasmtime embedding that runs host-precompiled components
//! against the kernel's own eo9 root providers — reaching the hardware only through the
//! per-architecture modules under src/arch/.
//!
//! On the host triple (where the kernel workspace's unit tests run) this crate compiles to
//! a stub `main` so the workspace stays buildable and testable without a cross target.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

mod ticks;

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod arch;
#[cfg(target_os = "none")]
mod fdt;
#[cfg(target_os = "none")]
mod heap;
#[cfg(target_os = "none")]
mod panic;
// Raw ECAM/PCIe support; only the wasm `eo9:pci` root provider drives it, so it is gated
// with the store/runner feature to keep the featureless CI build lean.
#[cfg(all(target_os = "none", feature = "wasm-store"))]
mod pci;
// The kernel's own polled virtio-blk driver, used only for the persistent store disk
// (the `storedisk` boot token); guests keep getting their disks through the wasm
// `disk.virtio` driver over `eo9:pci`.
#[cfg(all(target_os = "none", feature = "wasm-storedisk"))]
mod virtio_blk;
#[cfg(all(
    target_os = "none",
    any(
        feature = "wasm-seed",
        feature = "wasm-hello",
        feature = "wasm-async",
        feature = "wasm-store"
    )
))]
mod wasm;

// The shared core (this file, src/heap.rs, src/wasm/) reaches the per-architecture drivers
// through these crate-root names; every architecture provides the same modules with the same
// public functions (src/arch/mod.rs). `rtc` is only consumed by the feature-gated providers,
// so the re-export is otherwise-unused in the feature-less build.
#[cfg(target_os = "none")]
#[allow(unused_imports)]
pub(crate) use arch::{mmu, power, rtc, timer, uart};

/// Rust entry point, called from the per-architecture boot stub with the stack set up and
/// `.bss` zeroed. Banner, heap self-test, timer readings, then the embedded wasm artifacts
/// (the seed canary and the eo9-example-hello program, when built in) — and finally the
/// machine powers off so QEMU exits cleanly.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn kmain(dtb: *const u8) -> ! {
    // Machine identification, privilege level, timer frequency, wall clock.
    arch::banner();

    // Bring up whatever memory translation the architecture needs before wasm code runs
    // (aarch64: identity map + caches, because compiled wasm programs perform unaligned
    // accesses that are only legal on Normal memory; riscv64: bare mode for now).
    mmu::init();

    // Heap: everything from the end of the kernel image to the architecture's usable top of
    // RAM.
    heap::init();
    heap::self_test();

    // Platform timer: readable counter plus a polled 10 ms timer condition.
    timer::self_test();

    // Interrupt delivery: forward the timer and UART-receive interrupts to this core so the
    // executor can idle in a low-power wait — woken by a sleep deadline or by a keystroke —
    // instead of busy-polling (see each architecture's `interrupts_init`).
    arch::interrupts_init();

    // The kernel command line (QEMU -append) selects what to run: `program=<name>` runs a
    // store entry headless, `demo` runs the original demo sequence below, and nothing at
    // all boots to the interactive eosh shell.
    let bootargs = fdt::bootargs(dtb);
    if let Some(bootargs) = bootargs {
        kprintln!("cmdline: {bootargs}");
    }
    #[cfg(feature = "wasm-store")]
    let handled = wasm::runner::boot(bootargs);
    #[cfg(not(feature = "wasm-store"))]
    let handled = false;

    if !handled {
        #[cfg(feature = "wasm-seed")]
        wasm::seed::run();
        #[cfg(feature = "wasm-hello")]
        wasm::hello::run();
        #[cfg(feature = "wasm-async")]
        wasm::async_demo::run();
        #[cfg(feature = "wasm-codegen")]
        wasm::codegen::run();
        #[cfg(not(any(
            feature = "wasm-seed",
            feature = "wasm-hello",
            feature = "wasm-async",
            feature = "wasm-codegen"
        )))]
        kprintln!(
            "wasm: no components embedded (build with `cargo xtask build-kernel {}`)",
            arch::NAME
        );
    }

    kprintln!(
        "[{:>8} us] kernel run complete; requesting {}",
        timer::uptime_us(),
        power::OFF_REQUEST
    );
    power::system_off()
}

/// Host-triple stub so `cargo test`/`cargo check` on the host keep working; the real
/// kernel only exists for bare-metal targets.
#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!(
        "eo9-kernel is a bare-metal image; build and run it via `cargo xtask build-kernel <arch>` and `cargo xtask qemu <arch>` (aarch64, riscv64 or x86_64)"
    );
}
