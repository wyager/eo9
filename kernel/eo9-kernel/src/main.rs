//! The Eo9 bare-metal kernel (plan/12-kernel.md): aarch64 on QEMU `virt`.
//!
//! Boot path: QEMU's `-kernel` loader reads the ELF produced by `cargo xtask build-kernel
//! aarch64` and jumps to `_start` (src/boot.rs) at EL1 with the MMU off. The assembly stub
//! parks secondary cores, enables FP/SIMD for later wasm code, installs the exception
//! vectors, sets up the boot stack, zeroes `.bss`, and calls [`kmain`]. From there
//! everything is Rust: PL011 serial output, a global heap (no_std + alloc), the generic
//! timer and PL031 RTC, and — behind the `wasm-seed` / `wasm-hello` features — a wasmtime
//! embedding that runs host-precompiled components: the hand-written seed canary and the
//! real `eo9-example-hello` program linked against the kernel's own eo9 root providers.
//!
//! On the host triple (where the kernel workspace's unit tests run) this crate compiles to
//! a stub `main` so the workspace stays buildable and testable without a cross target.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

mod ticks;

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(target_os = "none")]
mod boot;
#[cfg(target_os = "none")]
mod exceptions;
#[cfg(target_os = "none")]
mod heap;
#[cfg(target_os = "none")]
mod mmu;
#[cfg(target_os = "none")]
mod panic;
#[cfg(target_os = "none")]
mod psci;
#[cfg(target_os = "none")]
mod rtc;
#[cfg(target_os = "none")]
mod timer;
#[cfg(target_os = "none")]
mod uart;
#[cfg(all(
    target_os = "none",
    any(feature = "wasm-seed", feature = "wasm-hello", feature = "wasm-async")
))]
mod wasm;

/// Rust entry point, called from the assembly boot stub with the stack set up and `.bss`
/// zeroed. Banner, heap self-test, generic-timer readings, then the embedded wasm
/// artifacts (the seed canary and the eo9-example-hello program, when built in) — and
/// finally the machine powers off so QEMU exits cleanly.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    kprintln!();
    kprintln!("Eo9 kernel — aarch64 (QEMU virt)");
    kprintln!("  exception level: EL{}", current_el());
    kprintln!("  counter-timer frequency: {} Hz", timer::frequency());
    kprintln!(
        "  wall clock: {}.{:09} s since the Unix epoch (PL031 + generic timer)",
        rtc::seconds(),
        timer::subsecond_ns()
    );

    // Identity-map the machine and turn on the MMU and caches: compiled wasm programs
    // perform unaligned accesses, which are only legal on Normal memory.
    mmu::enable_identity_map();

    // Heap: everything from the end of the kernel image to the top of RAM.
    heap::init();
    heap::self_test();

    // Generic timer: readable counter plus a polled 10 ms timer condition.
    timer::self_test();

    #[cfg(feature = "wasm-seed")]
    wasm::seed::run();
    #[cfg(feature = "wasm-hello")]
    wasm::hello::run();
    #[cfg(feature = "wasm-async")]
    wasm::async_demo::run();
    #[cfg(not(any(feature = "wasm-seed", feature = "wasm-hello", feature = "wasm-async")))]
    kprintln!("wasm: no components embedded (build with `cargo xtask build-kernel aarch64`)");

    kprintln!(
        "[{:>8} us] kernel run complete; requesting PSCI SYSTEM_OFF",
        timer::uptime_us()
    );
    psci::system_off()
}

/// The current exception level (expected: 1 on QEMU `virt` without EL2/EL3 enabled).
#[cfg(target_os = "none")]
fn current_el() -> u64 {
    let current_el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) current_el, options(nomem, nostack)) };
    (current_el >> 2) & 0b11
}

/// Host-triple stub so `cargo test`/`cargo check` on the host keep working; the real
/// kernel only exists for bare-metal targets.
#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!(
        "eo9-kernel is a bare-metal image; build and run it via `cargo xtask build-kernel aarch64` and `cargo xtask qemu aarch64`"
    );
}
