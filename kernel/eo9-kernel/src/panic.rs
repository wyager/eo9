//! Panic handler: report over serial, then power off.
//!
//! Powering off (rather than spinning) keeps `cargo xtask qemu aarch64` scriptable — a
//! kernel panic ends the QEMU run instead of hanging it.

use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    crate::kprintln!();
    crate::kprintln!("KERNEL PANIC: {info}");
    crate::kprintln!("powering off");
    crate::psci::system_off()
}
