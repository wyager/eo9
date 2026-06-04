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
/// Line control register: word length, FIFO enable.
const UARTLCR_H: usize = 0x02C;
/// Control register: UART/TX/RX enables.
const UARTCR: usize = 0x030;
/// Interrupt FIFO level select register.
const UARTIFLS: usize = 0x034;
/// Line control: enable the 16-byte FIFOs.
const UARTLCR_H_FEN: u32 = 1 << 4;
/// Line control: 8-bit words.
const UARTLCR_H_WLEN8: u32 = 0b11 << 5;
/// Control: UART enable / transmit enable / receive enable.
const UARTCR_UARTEN: u32 = 1 << 0;
const UARTCR_TXE: u32 = 1 << 8;
const UARTCR_RXE: u32 = 1 << 9;
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
    // SAFETY: `UART_BASE + offset` is a valid PL011 register on the `virt` machine;
    // `crate::mmio` pins the access to a syndrome-valid GPR form (device memory must
    // never be touched through plain volatile on aarch64 — see that module's docs).
    unsafe { crate::mmio::read_u32(UART_BASE + offset) }
}

fn mmio_write(offset: usize, value: u32) {
    // SAFETY: as above, for writes.
    unsafe { crate::mmio::write_u32(UART_BASE + offset, value) }
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
/// Sized to hold the longest line the shell accepts (`MAX_READ_LINE_BYTES`, 4096) so a
/// pasted command line of any accepted length survives even if the consumer is slow to
/// drain — paste robustness, plan/12.
const RX_RING_CAP: usize = 4096;

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
///
/// Also programs the line for interactive use: 8-bit words with the 16-byte FIFOs enabled.
/// The FIFO matters for paste robustness — QEMU paces its character feed by the device's
/// free FIFO space (`pl011_can_receive` returns `depth - read_count`), and with FIFOs off
/// the depth is 1, so *every byte* stalls the host-side feed until the guest completes a
/// full interrupt + drain + data-register-read round-trip. Under host load those per-byte
/// pause/resume cycles are where the chardev flow has been observed to wedge permanently
/// (the paste-freeze bug, plan/12); with a 16-deep FIFO the feed pauses at most once per
/// 16 bytes. On real hardware the FIFO is equally desirable (fewer interrupts per burst).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn enable_rx_interrupt() {
    // 8n1, FIFOs on. Written before the enables per the PL011 programming sequence.
    mmio_write(UARTLCR_H, UARTLCR_H_WLEN8 | UARTLCR_H_FEN);
    // Earliest RX trigger (1/8 full) so real hardware interrupts promptly; QEMU's model
    // raises the receive interrupt on the first byte regardless.
    mmio_write(UARTIFLS, 0);
    // QEMU works without touching UARTCR (its reset state already moves data), but the
    // TRM-correct enables cost nothing and matter on real silicon.
    mmio_write(UARTCR, UARTCR_UARTEN | UARTCR_TXE | UARTCR_RXE);
    // Start from a clean slate, then unmask receive + receive-timeout.
    mmio_write(UARTICR, 0x7FF);
    mmio_write(UARTIMSC, UART_INT_RX | UART_INT_RT);
}

/// Move every byte waiting in the RX FIFO into [`RX_RING`]; returns how many moved.
/// Bytes beyond the ring's capacity are dropped (paste overflow loses characters; the
/// console never goes deaf). Caller must be the sole producer at the time of the call:
/// either the IRQ handler, or thread context with IRQs masked ([`scavenge_rx`]).
fn fifo_to_ring() -> usize {
    let mut moved = 0;
    while mmio_read(UARTFR) & UARTFR_RXFE == 0 {
        let byte = (mmio_read(UARTDR) & 0xff) as u8;
        moved += 1;
        let head = RX_RING.head.load(Ordering::Relaxed);
        let next = (head + 1) % RX_RING_CAP;
        // Drop the byte if the ring is full rather than overwrite unread input.
        if next != RX_RING.tail.load(Ordering::Acquire) {
            // SAFETY: the caller is the sole producer; this slot is not being read
            // (it is at/after `head`, ahead of the consumer's `tail`).
            unsafe { (*RX_RING.buf.get())[head] = byte };
            RX_RING.head.store(next, Ordering::Release);
        }
    }
    moved
}

/// Interrupt handler body: clear the UART's RX/RT interrupt sources, then drain every
/// waiting byte into [`RX_RING`]. Called from the GIC IRQ dispatch (src/exceptions.rs)
/// when UART SPI 33 fires.
///
/// The order is acknowledge-then-drain, and it is load-bearing on QEMU: its PL011 model
/// latches the receive interrupt on the empty-to-occupied FIFO transition and `UARTICR`
/// clears the latch. Draining first and clearing after races a byte that lands between the
/// final empty check and the `UARTICR` write — the clear then wipes the just-latched
/// interrupt while the byte sits in the FIFO, the FIFO never re-crosses the trigger
/// transition, and (with no receive-timeout timer in the model) no UART interrupt ever
/// fires again: the console goes permanently deaf. Clearing first means a byte that
/// arrives after the final drain check re-latches the interrupt and is delivered normally.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn drain_rx() {
    mmio_write(UARTICR, UART_INT_RX | UART_INT_RT);
    fifo_to_ring();
}

/// Idle-path receive scavenger: rescue anything the interrupt path missed, and nudge a
/// wedged host character feed. Called from the executor's idle wakes (src/wasm/mod.rs
/// `idle_wait`, at least about once a second via the backstop), in thread context.
///
/// Two distinct recoveries, both belt-and-braces behind [`drain_rx`]'s ordering fix:
///
/// * **Stranded FIFO bytes** — any data sitting in the FIFO with the interrupt latch dead
///   is moved into the ring (and the interrupt path resumes with the next clean byte).
/// * **Wedged host feed** — under host load, QEMU's per-chunk pause/resume of the
///   character feed (`can_receive == 0` → wait for `accept_input`) has been observed to
///   stop delivering input permanently while the FIFO sits *empty* (the paste-freeze bug):
///   the guest cannot see the undelivered bytes at all. QEMU calls `accept_input`
///   unconditionally on every `UARTDR` read — even with an empty FIFO — so after one
///   second of total input silence, one harmless data-register read kicks the feed back
///   to life. Trade-off, documented: if a byte lands in the few-instruction window between
///   the final empty check and that dummy read, it is consumed here and lost (one
///   keystroke, only ever during an already-silent second); permanent deafness is traded
///   for that vanishingly rare single-character loss.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn scavenge_rx(now_ns: u64) {
    use core::sync::atomic::AtomicU64;
    /// Uptime of the last observed receive activity (ring movement or FIFO data).
    static LAST_ACTIVITY_NS: AtomicU64 = AtomicU64::new(0);
    /// Uptime of the last feed kick, so a fully idle console kicks at most once a second.
    static LAST_KICK_NS: AtomicU64 = AtomicU64::new(0);
    const QUIET_BEFORE_KICK_NS: u64 = 1_000_000_000;

    // Mask IRQs so this thread is the ring's only producer for the duration (the IRQ
    // handler is the producer otherwise; single core, so masking excludes it entirely).
    // SAFETY: mask/unmask of DAIF.I around a few MMIO reads, no stack or register effects.
    unsafe { core::arch::asm!("msr daifset, #2", options(nomem, nostack, preserves_flags)) };
    let moved = fifo_to_ring();
    let ring_busy = RX_RING.head.load(Ordering::Relaxed) != RX_RING.tail.load(Ordering::Relaxed);
    if moved > 0 || ring_busy {
        LAST_ACTIVITY_NS.store(now_ns, Ordering::Relaxed);
    } else {
        let quiet_since = LAST_ACTIVITY_NS.load(Ordering::Relaxed);
        let last_kick = LAST_KICK_NS.load(Ordering::Relaxed);
        if now_ns.saturating_sub(quiet_since) >= QUIET_BEFORE_KICK_NS
            && now_ns.saturating_sub(last_kick) >= QUIET_BEFORE_KICK_NS
        {
            LAST_KICK_NS.store(now_ns, Ordering::Relaxed);
            // The FIFO is empty (checked under the mask just above): this read returns
            // stale data and exists purely for QEMU's unconditional `accept_input` side
            // effect, which resumes a wedged character feed.
            let _ = mmio_read(UARTDR);
        }
    }
    // SAFETY: restore IRQ delivery.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
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

/// ETX (Ctrl-C) — the interrupt key.
pub const CTRL_C: u8 = 0x03;

/// Non-destructively scan the waiting input for a Ctrl-C and, if present, consume the ring up
/// to and including it (flushing pending input through the interrupt, the usual terminal
/// behaviour) and return `true`. If no Ctrl-C is waiting the ring is left untouched and this
/// returns `false`. Single-consumer-safe: only the boot core calls this and `ring_get_byte`,
/// never concurrently, so reading `tail..head` and advancing `tail` here is sound.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn take_ctrl_c() -> bool {
    let head = RX_RING.head.load(Ordering::Acquire);
    let mut i = RX_RING.tail.load(Ordering::Relaxed);
    while i != head {
        // SAFETY: the boot core is the sole consumer; this slot is published (it is before
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

/// Non-destructively check whether a Ctrl-C is waiting in the input ring, without consuming
/// it (or anything before it). The wasm `eo9:pci` provider's interrupt `wait` peeks this so a
/// console interrupt aborts a blocked device wait promptly; the *consuming* check (and the
/// resulting kill) stays the shell's job (`take_ctrl_c` above).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn ctrl_c_pending() -> bool {
    let head = RX_RING.head.load(Ordering::Acquire);
    let mut i = RX_RING.tail.load(Ordering::Relaxed);
    while i != head {
        // SAFETY: the boot core is the sole consumer; this slot is published (it is before
        // `head`, which was loaded with acquire ordering).
        let byte = unsafe { (*RX_RING.buf.get())[i] };
        if byte == CTRL_C {
            return true;
        }
        i = (i + 1) % RX_RING_CAP;
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
