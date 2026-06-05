//! Panic handler: report over serial, then end the run.
//!
//! Powering off (rather than spinning) keeps `cargo xtask qemu <arch>` scriptable — a
//! kernel panic ends the QEMU run instead of hanging it. On the Orange Pi board profile
//! (`board-opi5plus`) the end-of-run transition is a PSCI SYSTEM_RESET back to U-Boot
//! instead (the unattended dev loop — a panicked board must come back on its own), and the
//! report carries a grep-stable `EO9-PANIC` marker so the loop driver can classify the
//! boot; `power::system_off()` drains the UART transmit FIFO before the reset so the whole
//! report reaches the wire.

use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    crate::kprintln!();
    crate::kprintln!("KERNEL PANIC: {info}");
    #[cfg(feature = "board-opi5plus")]
    crate::kprintln!("EO9-PANIC");
    crate::kprintln!("powering off");
    crate::power::system_off()
}
