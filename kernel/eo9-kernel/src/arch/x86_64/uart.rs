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

use core::fmt;
use core::sync::atomic::Ordering;

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

static RX_RING: crate::rxring::RxRing = crate::rxring::RxRing::new();

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

/// Move every byte waiting in the receive FIFO into [`RX_RING`]; returns how many moved.
/// Caller must be the sole producer: the trap handler, or thread context with interrupts
/// masked ([`scavenge_rx`]).
fn fifo_to_ring() -> usize {
    RX_RING.drain(|| {
        if reg_read(LSR) & LSR_DR == 0 {
            None
        } else {
            Some(reg_read(RBR_THR))
        }
    })
}

/// Interrupt handler body: drain every waiting RX byte into [`RX_RING`]. Called from the
/// trap dispatcher (src/arch/x86_64/traps.rs) when IRQ 4 fires; emptying the receive FIFO
/// deasserts the UART's interrupt line.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn drain_rx() {
    fifo_to_ring();
}

/// Idle-path receive scavenger: rescue FIFO leftovers and nudge a wedged host character
/// feed. Same design and trade-offs as the aarch64 driver's `scavenge_rx` (see
/// src/arch/aarch64/uart.rs): QEMU's 16550 model calls `accept_input` unconditionally on
/// every receive-buffer read, so after a second of total input silence one harmless read
/// of an empty receive buffer resumes a wedged feed.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn scavenge_rx(now_ns: u64) -> usize {
    use core::sync::atomic::AtomicU64;
    static LAST_ACTIVITY_NS: AtomicU64 = AtomicU64::new(0);
    static LAST_KICK_NS: AtomicU64 = AtomicU64::new(0);
    const QUIET_BEFORE_KICK_NS: u64 = 1_000_000_000;

    // SAFETY: mask interrupt delivery so this thread is the ring's only producer for the
    // duration; cli/sti have no stack or register effects.
    unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)) };
    let moved = fifo_to_ring();
    let ring_busy = RX_RING.is_busy();
    if moved > 0 || ring_busy {
        LAST_ACTIVITY_NS.store(now_ns, Ordering::Relaxed);
    } else {
        let quiet_since = LAST_ACTIVITY_NS.load(Ordering::Relaxed);
        let last_kick = LAST_KICK_NS.load(Ordering::Relaxed);
        if now_ns.saturating_sub(quiet_since) >= QUIET_BEFORE_KICK_NS
            && now_ns.saturating_sub(last_kick) >= QUIET_BEFORE_KICK_NS
        {
            LAST_KICK_NS.store(now_ns, Ordering::Relaxed);
            // Receive buffer is empty (checked under the mask): this read exists purely
            // for QEMU's unconditional `accept_input` side effect.
            let _ = reg_read(RBR_THR);
        }
    }
    // SAFETY: restore interrupt delivery.
    unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)) };
    moved
}

/// Consume one received byte from the interrupt-filled ring, or `None` if none is waiting.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn ring_get_byte() -> Option<u8> {
    RX_RING.get_byte()
}

/// ETX (Ctrl-C) — the interrupt key.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub const CTRL_C: u8 = crate::rxring::CTRL_C;

/// Non-destructively scan the waiting input for a Ctrl-C and, if present, consume the ring up
/// to and including it (flushing pending input through the interrupt, the usual terminal
/// behaviour) and return `true`. If no Ctrl-C is waiting the ring is left untouched and this
/// returns `false`. Single-consumer-safe: only the boot CPU calls this and `ring_get_byte`,
/// never concurrently, so reading `tail..head` and advancing `tail` here is sound.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn take_ctrl_c() -> bool {
    RX_RING.take_ctrl_c()
}

/// Non-destructively check whether a Ctrl-C is waiting in the input ring, without consuming
/// it (or anything before it). The wasm `eo9:pci` provider's interrupt `wait` peeks this so a
/// console interrupt aborts a blocked device wait promptly; the *consuming* check (and the
/// resulting kill) stays the shell's job (`take_ctrl_c` above).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn ctrl_c_pending() -> bool {
    RX_RING.ctrl_c_pending()
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
