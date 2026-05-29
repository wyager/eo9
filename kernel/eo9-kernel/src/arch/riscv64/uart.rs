//! NS16550A UART console on QEMU's riscv64 `virt` machine.
//!
//! The UART sits at its fixed `virt` address (0x1000_0000, byte-wide registers) and QEMU
//! wires it to stdio under `-nographic`. Transmit is "poll the transmit-holding-register
//! empty flag, write the data register" and needs no initialization in QEMU's model.
//! Receive mirrors the aarch64 PL011 driver: an interrupt (PLIC source 10) drains arriving
//! bytes into a small ring so the executor can halt in `wfi` and be woken by a keystroke
//! instead of polling — see src/arch/aarch64/uart.rs for the ring's design notes.

use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

/// NS16550A base address on the QEMU riscv64 `virt` machine.
const UART_BASE: usize = 0x1000_0000;
/// Receive buffer (read) / transmit holding register (write).
const RBR_THR: usize = 0;
/// Interrupt enable register.
const IER: usize = 1;
/// Line status register.
const LSR: usize = 5;
/// Line status: data ready (a received byte is waiting).
const LSR_DR: u8 = 1 << 0;
/// Line status: transmit holding register empty.
const LSR_THRE: u8 = 1 << 5;
/// Interrupt enable: received data available.
const IER_ERBFI: u8 = 1 << 0;

fn mmio_read(offset: usize) -> u8 {
    // SAFETY: `UART_BASE + offset` is a valid NS16550A register on the `virt` machine, and
    // volatile MMIO reads have no other side conditions.
    unsafe { core::ptr::read_volatile((UART_BASE + offset) as *const u8) }
}

fn mmio_write(offset: usize, value: u8) {
    // SAFETY: as above, for writes.
    unsafe { core::ptr::write_volatile((UART_BASE + offset) as *mut u8, value) }
}

/// Write one byte, spinning while the transmit holding register is full.
pub fn put_byte(byte: u8) {
    while mmio_read(LSR) & LSR_THRE == 0 {
        core::hint::spin_loop();
    }
    mmio_write(RBR_THR, byte);
}

/// Read one received byte if one is waiting (non-blocking). Only used as a fallback before
/// the receive interrupt is enabled; afterwards [`drain_rx`] moves bytes into the ring and
/// the read-line provider consumes them via [`ring_get_byte`].
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn try_get_byte() -> Option<u8> {
    if mmio_read(LSR) & LSR_DR == 0 {
        None
    } else {
        Some(mmio_read(RBR_THR))
    }
}

// --- Interrupt-driven receive -------------------------------------------------------------
//
// Same single-producer/single-consumer ring as the aarch64 PL011 driver: the trap handler
// (the only producer) drains the UART when the PLIC delivers source 10, and the read-line
// provider on the boot hart (the only consumer) takes bytes out, so head/tail atomics are
// sufficient and a level-style receive condition is fully drained before the claim is
// completed.

/// RX ring capacity (power of two; one slot is left empty to distinguish full from empty).
const RX_RING_CAP: usize = 256;

/// Single-producer (trap handler) / single-consumer (boot hart) byte ring for received input.
struct RxRing {
    buf: UnsafeCell<[u8; RX_RING_CAP]>,
    /// Next index the producer (trap handler) will write.
    head: AtomicUsize,
    /// Next index the consumer (read-line) will read.
    tail: AtomicUsize,
}

// SAFETY: the only producer is the trap handler and the only consumer is the boot hart's
// read-line poll; access is coordinated through `head`/`tail` with acquire/release ordering.
unsafe impl Sync for RxRing {}

static RX_RING: RxRing = RxRing {
    buf: UnsafeCell::new([0; RX_RING_CAP]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
};

/// Enable the receive interrupt so an arriving byte asserts the UART's PLIC line. Call once
/// during boot after the PLIC forwards source 10 (src/arch/riscv64/mod.rs).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn enable_rx_interrupt() {
    mmio_write(IER, IER_ERBFI);
}

/// Interrupt handler body: drain every waiting RX byte into [`RX_RING`]. Called from the
/// external-interrupt trap path (src/arch/riscv64/traps.rs) when PLIC source 10 fires;
/// emptying the receive buffer deasserts the UART's interrupt line.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn drain_rx() {
    while mmio_read(LSR) & LSR_DR != 0 {
        let byte = mmio_read(RBR_THR);
        let head = RX_RING.head.load(Ordering::Relaxed);
        let next = (head + 1) % RX_RING_CAP;
        // Drop the byte if the ring is full rather than overwrite unread input.
        if next != RX_RING.tail.load(Ordering::Acquire) {
            // SAFETY: the trap handler is the sole producer; this slot is not being read
            // (it is at/after `head`, ahead of the consumer's `tail`).
            unsafe { (*RX_RING.buf.get())[head] = byte };
            RX_RING.head.store(next, Ordering::Release);
        }
    }
}

/// Consume one received byte from the interrupt-filled ring, or `None` if none is waiting.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn ring_get_byte() -> Option<u8> {
    let tail = RX_RING.tail.load(Ordering::Relaxed);
    if tail == RX_RING.head.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: the boot hart is the sole consumer; this slot was published by the producer
    // (head moved past it with release ordering, observed by the acquire load above).
    let byte = unsafe { (*RX_RING.buf.get())[tail] };
    RX_RING
        .tail
        .store((tail + 1) % RX_RING_CAP, Ordering::Release);
    Some(byte)
}

/// ETX (Ctrl-C) — the interrupt key.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub const CTRL_C: u8 = 0x03;

/// Non-destructively scan the waiting input for a Ctrl-C and, if present, consume the ring up
/// to and including it (flushing pending input through the interrupt, the usual terminal
/// behaviour) and return `true`. If no Ctrl-C is waiting the ring is left untouched and this
/// returns `false`. Single-consumer-safe: only the boot hart calls this and `ring_get_byte`,
/// never concurrently, so reading `tail..head` and advancing `tail` here is sound.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn take_ctrl_c() -> bool {
    let head = RX_RING.head.load(Ordering::Acquire);
    let mut i = RX_RING.tail.load(Ordering::Relaxed);
    while i != head {
        // SAFETY: the boot hart is the sole consumer; this slot is published (it is before
        // `head`, which was loaded with acquire ordering).
        let byte = unsafe { (*RX_RING.buf.get())[i] };
        let next = (i + 1) % RX_RING_CAP;
        if byte == CTRL_C {
            // Discard everything up to and including the Ctrl-C.
            RX_RING.tail.store(next, Ordering::Release);
            return true;
        }
        i = next;
    }
    false
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
