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
/// FIFO control register (write).
const FCR: usize = 2;
/// FIFO control: enable both FIFOs and clear them.
const FCR_ENABLE_CLEAR: u8 = 0x07;

fn mmio_read(offset: usize) -> u8 {
    // SAFETY: `UART_BASE + offset` is a valid NS16550A register on the `virt` machine, and
    // volatile MMIO reads have no other side conditions.
    // Plain volatile is fine here: ISV syndrome decoding is an aarch64-hypervisor
    // concern (see crate::mmio) — riscv64 runs under TCG only.
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
/// Sized to hold the longest line the shell accepts (`MAX_READ_LINE_BYTES`, 4096) so a
/// pasted command line of any accepted length survives even if the consumer is slow to
/// drain — paste robustness, plan/12.
const RX_RING_CAP: usize = 4096;

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
    // Enable the 16-byte FIFOs first: QEMU paces its character feed by the receive buffer's
    // free space, and with the FIFO off that space is a single byte, so every received byte
    // stalls the host-side feed until the guest finishes a full interrupt round-trip — the
    // per-byte pause/resume churn behind the paste-freeze bug (plan/12; the aarch64 driver
    // documents the mechanism). With the FIFO on, the feed pauses at most once per chunk.
    mmio_write(FCR, FCR_ENABLE_CLEAR);
    mmio_write(IER, IER_ERBFI);
}

/// Interrupt handler body: drain every waiting RX byte into [`RX_RING`]. Called from the
/// external-interrupt trap path (src/arch/riscv64/traps.rs) when PLIC source 10 fires;
/// emptying the receive buffer deasserts the UART's interrupt line.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
/// Move every byte waiting in the receive FIFO into [`RX_RING`]; returns how many moved.
/// Caller must be the sole producer: the trap handler, or thread context with interrupts
/// masked ([`scavenge_rx`]).
fn fifo_to_ring() -> usize {
    let mut moved = 0;
    while mmio_read(LSR) & LSR_DR != 0 {
        let byte = mmio_read(RBR_THR);
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

#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn drain_rx() {
    fifo_to_ring();
}

/// Idle-path receive scavenger: rescue FIFO leftovers and nudge a wedged host character
/// feed. Same design and trade-offs as the aarch64 driver's `scavenge_rx` (see
/// src/arch/aarch64/uart.rs): QEMU's 16550 model also calls `accept_input` unconditionally
/// on every receive-buffer read, so after a second of total input silence one harmless
/// read of an empty receive buffer resumes a wedged feed.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn scavenge_rx(now_ns: u64) -> usize {
    use core::sync::atomic::AtomicU64;
    static LAST_ACTIVITY_NS: AtomicU64 = AtomicU64::new(0);
    static LAST_KICK_NS: AtomicU64 = AtomicU64::new(0);
    const QUIET_BEFORE_KICK_NS: u64 = 1_000_000_000;

    // SAFETY: mask supervisor interrupts so this thread is the ring's only producer for
    // the duration; mask/unmask of sstatus.SIE has no stack or register effects.
    unsafe { core::arch::asm!("csrci sstatus, 2", options(nomem, nostack, preserves_flags)) };
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
            // Receive buffer is empty (checked under the mask): this read exists purely
            // for QEMU's unconditional `accept_input` side effect.
            let _ = mmio_read(RBR_THR);
        }
    }
    // SAFETY: restore supervisor interrupt delivery.
    unsafe { core::arch::asm!("csrsi sstatus, 2", options(nomem, nostack, preserves_flags)) };
    moved
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

/// Inject bytes into the console input ring as a SECOND producer — the M4 console-sink
/// path (docs/board/usb-ohci-plan.md): a USB HID keyboard service feeds the same ring
/// serial input lands in, so injected bytes interleave with serial bytes, the existing
/// Ctrl-C scan catches an injected 0x03 exactly like a serial one, and >64-byte lines
/// survive (no UART FIFO in this path). Returns how many bytes were accepted; ring-full
/// bytes are dropped and counted (`INJECT_DROPPED`), never blocking the injector.
///
/// Producer discipline: the IRQ handler is the ring's usual producer; this runs in
/// thread context with interrupts masked for the few stores (the same exclusion
/// `scavenge_rx` uses), so the single-producer invariant holds throughout.
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn inject_input(bytes: &[u8]) -> usize {
    static INJECT_DROPPED: AtomicUsize = AtomicUsize::new(0);
    // SAFETY: mask/unmask around a few ring stores, no stack or register effects.
    unsafe { core::arch::asm!("csrci sstatus, 2", options(nomem, nostack, preserves_flags)) };
    let mut accepted = 0;
    for &byte in bytes {
        let head = RX_RING.head.load(Ordering::Relaxed);
        let next = (head + 1) % RX_RING_CAP;
        // Drop the byte if the ring is full rather than overwrite unread input
        // (the serial producer's policy, applied to this producer too).
        if next != RX_RING.tail.load(Ordering::Acquire) {
            // SAFETY: interrupts are masked, so this thread is the sole producer;
            // the slot is at `head`, ahead of the consumer's `tail`.
            unsafe { (*RX_RING.buf.get())[head] = byte };
            RX_RING.head.store(next, Ordering::Release);
            accepted += 1;
        } else {
            INJECT_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
    // SAFETY: restore interrupt delivery.
    unsafe { core::arch::asm!("csrsi sstatus, 2", options(nomem, nostack, preserves_flags)) };
    accepted
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
        // SAFETY: the boot hart is the sole consumer; this slot is published (it is before
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
