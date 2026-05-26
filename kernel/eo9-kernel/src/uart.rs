//! PL011 UART console on QEMU's aarch64 `virt` machine.
//!
//! The UART sits at its fixed `virt` address (0x0900_0000) and QEMU wires it to stdio
//! under `-nographic`, so transmit is just "poll the FIFO-full flag, write the data
//! register". QEMU's model needs no initialization for transmit-only use, which is all the
//! spike needs. The console is stateless (every write goes straight to the MMIO
//! registers), so no global state or locking is required on the single boot core.

use core::fmt;

/// PL011 base address on the QEMU `virt` machine.
const UART_BASE: usize = 0x0900_0000;
/// Data register.
const UARTDR: usize = 0x000;
/// Flag register.
const UARTFR: usize = 0x018;
/// Flag register: transmit FIFO full.
const UARTFR_TXFF: u32 = 1 << 5;

fn mmio_read(offset: usize) -> u32 {
    // SAFETY: `UART_BASE + offset` is a valid PL011 register on the `virt` machine, and
    // volatile MMIO reads have no other side conditions.
    unsafe { core::ptr::read_volatile((UART_BASE + offset) as *const u32) }
}

fn mmio_write(offset: usize, value: u32) {
    // SAFETY: as above, for writes.
    unsafe { core::ptr::write_volatile((UART_BASE + offset) as *mut u32, value) }
}

/// Write one byte, spinning while the transmit FIFO is full.
pub fn put_byte(byte: u8) {
    while mmio_read(UARTFR) & UARTFR_TXFF != 0 {
        core::hint::spin_loop();
    }
    mmio_write(UARTDR, u32::from(byte));
}

/// Zero-sized serial console handle; `core::fmt::Write` goes straight to the hardware.
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            put_byte(byte);
        }
        Ok(())
    }
}

/// Print to the serial console (no trailing newline).
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use ::core::fmt::Write as _;
        let _ = ::core::write!($crate::uart::Console, $($arg)*);
    }};
}

/// Print a line to the serial console.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{
        $crate::kprint!($($arg)*);
        $crate::kprint!("\n");
    }};
}
