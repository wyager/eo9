//! 16550 UART console (COM1) on QEMU's x86_64 `q35` machine.
//!
//! COM1 sits at the legacy I/O ports 0x3F8..0x3FF and QEMU wires it to stdio under
//! `-nographic`. Transmit is "poll the transmit-holding-register-empty flag, write the data
//! register" and needs no initialization in QEMU's model. Receive mirrors the aarch64 PL011
//! and riscv64 NS16550A drivers: an interrupt (PIC IRQ 4) drains arriving bytes into a small
//! ring so the executor can halt in `hlt` and be woken by a keystroke instead of polling —
//! see src/arch/aarch64/uart.rs for the ring's design notes. The register protocol is the
//! same 16550 the riscv64 port drives; only the access method differs (port I/O here, MMIO
//! there), which is why the driver is duplicated rather than shared.

use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::io::{inb, outb};

/// COM1 base I/O port.
const COM1: u16 = 0x3F8;
/// Receive buffer (read) / transmit holding register (write).
const RBR_THR: u16 = 0;
/// Interrupt enable register.
const IER: u16 = 1;
/// FIFO control register (write).
const FCR: u16 = 2;
/// Line control register.
const LCR: u16 = 3;
/// Modem control register.
const MCR: u16 = 4;
/// Line status register.
const LSR: u16 = 5;
/// Line status: data ready (a received byte is waiting).
const LSR_DR: u8 = 1 << 0;
/// Line status: transmit holding register empty.
const LSR_THRE: u8 = 1 << 5;
/// Interrupt enable: received data available.
const IER_ERBFI: u8 = 1 << 0;
/// Line control: 8 data bits, no parity, one stop bit (and DLAB clear).
const LCR_8N1: u8 = 0x03;
/// FIFO control: enable and clear both FIFOs.
const FCR_ENABLE_CLEAR: u8 = 0x07;
/// Modem control: DTR | RTS | OUT2 — OUT2 gates the UART's interrupt onto the PIC line.
const MCR_DTR_RTS_OUT2: u8 = 0x0B;

fn reg_read(offset: u16) -> u8 {
    inb(COM1 + offset)
}

fn reg_write(offset: u16, value: u8) {
    outb(COM1 + offset, value);
}

/// Write one byte, spinning while the transmit holding register is full.
pub fn put_byte(byte: u8) {
    while reg_read(LSR) & LSR_THRE == 0 {
        core::hint::spin_loop();
    }
    reg_write(RBR_THR, byte);
}

/// Read one received byte if one is waiting (non-blocking). Only used as a fallback before
/// the receive interrupt is enabled; afterwards [`drain_rx`] moves bytes into the ring and
/// the read-line provider consumes them via [`ring_get_byte`].
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn try_get_byte() -> Option<u8> {
    if reg_read(LSR) & LSR_DR == 0 {
        None
    } else {
        Some(reg_read(RBR_THR))
    }
}

// --- Interrupt-driven receive -------------------------------------------------------------
//
// Same single-producer/single-consumer ring as the aarch64 PL011 driver: the trap handler
// (the only producer) drains the UART when PIC IRQ 4 fires, and the read-line provider on
// the boot CPU (the only consumer) takes bytes out, so head/tail atomics are sufficient and
// the receive FIFO is fully drained before the interrupt is acknowledged.

/// RX ring capacity (power of two; one slot is left empty to distinguish full from empty).
const RX_RING_CAP: usize = 256;

/// Single-producer (trap handler) / single-consumer (boot CPU) byte ring for received input.
struct RxRing {
    buf: UnsafeCell<[u8; RX_RING_CAP]>,
    /// Next index the producer (trap handler) will write.
    head: AtomicUsize,
    /// Next index the consumer (read-line) will read.
    tail: AtomicUsize,
}

// SAFETY: the only producer is the trap handler and the only consumer is the boot CPU's
// read-line poll; access is coordinated through `head`/`tail` with acquire/release ordering.
unsafe impl Sync for RxRing {}

static RX_RING: RxRing = RxRing {
    buf: UnsafeCell::new([0; RX_RING_CAP]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
};

/// Configure the line (8n1, FIFOs on) and enable the receive interrupt so an arriving byte
/// asserts PIC IRQ 4. MCR.OUT2 must be set or the 16550 never drives its interrupt line.
/// Call once during boot after the PIC has been remapped (src/arch/x86_64/mod.rs).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn enable_rx_interrupt() {
    reg_write(LCR, LCR_8N1);
    reg_write(FCR, FCR_ENABLE_CLEAR);
    reg_write(MCR, MCR_DTR_RTS_OUT2);
    reg_write(IER, IER_ERBFI);
}

/// Interrupt handler body: drain every waiting RX byte into [`RX_RING`]. Called from the
/// trap dispatcher (src/arch/x86_64/traps.rs) when IRQ 4 fires; emptying the receive FIFO
/// deasserts the UART's interrupt line.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn drain_rx() {
    while reg_read(LSR) & LSR_DR != 0 {
        let byte = reg_read(RBR_THR);
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
    // SAFETY: the boot CPU is the sole consumer; this slot was published by the producer
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
/// returns `false`. Single-consumer-safe: only the boot CPU calls this and `ring_get_byte`,
/// never concurrently, so reading `tail..head` and advancing `tail` here is sound.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn take_ctrl_c() -> bool {
    let head = RX_RING.head.load(Ordering::Acquire);
    let mut i = RX_RING.tail.load(Ordering::Relaxed);
    while i != head {
        // SAFETY: the boot CPU is the sole consumer; this slot is published (it is before
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
