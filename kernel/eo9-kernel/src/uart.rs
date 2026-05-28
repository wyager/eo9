//! PL011 UART console on QEMU's aarch64 `virt` machine.
//!
//! The UART sits at its fixed `virt` address (0x0900_0000) and QEMU wires it to stdio
//! under `-nographic`, so transmit is just "poll the FIFO-full flag, write the data
//! register". QEMU's model needs no initialization for transmit-only use, which is all the
//! spike needs. The console is stateless (every write goes straight to the MMIO
//! registers), so no global state or locking is required on the single boot core.

use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

/// PL011 base address on the QEMU `virt` machine.
const UART_BASE: usize = 0x0900_0000;
/// Data register.
const UARTDR: usize = 0x000;
/// Flag register.
const UARTFR: usize = 0x018;
/// Interrupt mask set/clear register (write 1 to a bit to unmask that interrupt source).
const UARTIMSC: usize = 0x038;
/// Interrupt clear register (write 1 to a bit to clear that pending interrupt source).
const UARTICR: usize = 0x044;
/// Flag register: transmit FIFO full.
const UARTFR_TXFF: u32 = 1 << 5;
/// Flag register: receive FIFO empty.
// Receive is only consumed by the wasm `read-line` provider, which the feature-less CI
// build does not compile; keep the path unconditional rather than feature-gating MMIO.
#[allow(dead_code)]
const UARTFR_RXFE: u32 = 1 << 4;
/// Receive interrupt (UARTIMSC/UARTICR bit 4).
#[allow(dead_code)] // used only on the wasm/interactive path, not the feature-less CI build
const UART_INT_RX: u32 = 1 << 4;
/// Receive-timeout interrupt (UARTIMSC/UARTICR bit 6): fires when RX data has waited without
/// reaching the FIFO threshold, so a single keystroke still raises an interrupt.
#[allow(dead_code)]
const UART_INT_RT: u32 = 1 << 6;

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

/// Read one received byte if one is waiting (non-blocking; QEMU feeds the RX FIFO from
/// stdin under `-nographic`). Returns `None` when the receive FIFO is empty.
///
/// Used as a fallback before the RX interrupt is enabled; once [`enable_rx_interrupt`] has
/// run the interrupt handler ([`drain_rx`]) moves bytes into [`RX_RING`] and the read-line
/// provider consumes them via [`ring_get_byte`] instead — so the core can `wfi`-idle and be
/// woken by a keystroke rather than polling the data register.
#[allow(dead_code)] // see UARTFR_RXFE above
pub fn try_get_byte() -> Option<u8> {
    if mmio_read(UARTFR) & UARTFR_RXFE != 0 {
        None
    } else {
        Some((mmio_read(UARTDR) & 0xff) as u8)
    }
}

// --- Interrupt-driven receive -------------------------------------------------------------
//
// The PL011 raises its interrupt (routed through the GIC as SPI 33 on `virt`) when receive
// data arrives. The handler drains the RX FIFO into a small single-producer/single-consumer
// ring: the interrupt context is the only producer and the read-line provider on the boot
// core is the only consumer, so head/tail atomics are sufficient (no lock). This decouples
// "a byte arrived" (wakes `wfi`) from "the shell consumed it" and keeps a level-sensitive
// RX interrupt from re-firing — the handler empties the FIFO before returning.

/// RX ring capacity (power of two; one slot is left empty to distinguish full from empty).
const RX_RING_CAP: usize = 256;

/// Single-producer (IRQ) / single-consumer (boot core) byte ring for received input.
struct RxRing {
    buf: UnsafeCell<[u8; RX_RING_CAP]>,
    /// Next index the producer (IRQ) will write.
    head: AtomicUsize,
    /// Next index the consumer (read-line) will read.
    tail: AtomicUsize,
}

// SAFETY: the only producer is the IRQ handler and the only consumer is the boot core's
// read-line poll; access is coordinated through `head`/`tail` with acquire/release ordering.
unsafe impl Sync for RxRing {}

static RX_RING: RxRing = RxRing {
    buf: UnsafeCell::new([0; RX_RING_CAP]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
};

/// Enable the PL011 receive (and receive-timeout) interrupt so an arriving byte asserts the
/// UART's GIC line. Call once during boot after the GIC forwards UART SPI 33 (src/main.rs).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn enable_rx_interrupt() {
    mmio_write(UARTIMSC, UART_INT_RX | UART_INT_RT);
}

/// Interrupt handler body: drain every waiting RX byte into [`RX_RING`], then clear the
/// UART's RX/RT interrupt sources. Called from the GIC IRQ dispatch (src/exceptions.rs)
/// when UART SPI 33 fires. Draining fully deasserts the level-sensitive line.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn drain_rx() {
    while mmio_read(UARTFR) & UARTFR_RXFE == 0 {
        let byte = (mmio_read(UARTDR) & 0xff) as u8;
        let head = RX_RING.head.load(Ordering::Relaxed);
        let next = (head + 1) % RX_RING_CAP;
        // Drop the byte if the ring is full rather than overwrite unread input.
        if next != RX_RING.tail.load(Ordering::Acquire) {
            // SAFETY: the IRQ context is the sole producer; this slot is not being read
            // (it is at/after `head`, ahead of the consumer's `tail`).
            unsafe { (*RX_RING.buf.get())[head] = byte };
            RX_RING.head.store(next, Ordering::Release);
        }
    }
    // Clear the RX and RX-timeout interrupt sources at the UART.
    mmio_write(UARTICR, UART_INT_RX | UART_INT_RT);
}

/// Consume one received byte from the interrupt-filled ring, or `None` if none is waiting.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn ring_get_byte() -> Option<u8> {
    let tail = RX_RING.tail.load(Ordering::Relaxed);
    if tail == RX_RING.head.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: the boot core is the sole consumer; this slot was published by the producer
    // (head moved past it with release ordering, observed by the acquire load above).
    let byte = unsafe { (*RX_RING.buf.get())[tail] };
    RX_RING
        .tail
        .store((tail + 1) % RX_RING_CAP, Ordering::Release);
    Some(byte)
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
