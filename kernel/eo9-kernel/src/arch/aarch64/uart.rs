//! Serial console: PL011 on QEMU's aarch64 `virt` machine, or the RK3588's DW-APB UART2
//! on the `board-opi5plus` profile (1.5 Mbaud debug header; the line stays exactly as
//! U-Boot programmed it — divisor untouched, per docs/board/orange-pi-5-plus.md).
//!
//! The register layer lives in the cfg-selected `hw` module below; everything above it —
//! the RX ring (src/rxring.rs), Ctrl-C scanning, the console writer — is shared. Transmit
//! is "poll the ready flag, write the data register" on both parts. The console is
//! stateless (every write goes straight to the MMIO registers), so no global state or
//! locking is required on the single boot core.
//!
//! Board receive path: UART2's GIC SPI (333, [`RX_INTID`] 365) is forwarded like the
//! PL011's on `virt`, and `enable_rx_interrupt` unmasks the DW-APB receive interrupt, so
//! [`drain_rx`] empties the 64-byte RX FIFO at line rate. Day one shipped without the SPI
//! wired — input then reached the ring only through the idle-path [`scavenge_rx`] poll,
//! and anything past 64 bytes between scavenges overflowed the FIFO silently (the
//! exactly-64-byte console truncation, GAPS 2026-06-08). The scavenger stays as the
//! belt-and-braces backstop behind the interrupt path on both profiles.

use core::fmt;
use core::sync::atomic::Ordering;

/// PL011 register layer (QEMU `virt`).
#[cfg(not(feature = "board-opi5plus"))]
mod hw {
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
    /// Receive-timeout interrupt (UARTIMSC/UARTICR bit 6): fires when RX data has waited
    /// without reaching the FIFO threshold, so a single keystroke still raises an interrupt.
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

    /// Transmit not ready (FIFO full).
    pub(super) fn tx_busy() -> bool {
        mmio_read(UARTFR) & UARTFR_TXFF != 0
    }

    /// Write one byte to the data register.
    pub(super) fn tx_write(byte: u8) {
        mmio_write(UARTDR, u32::from(byte));
    }

    /// No received byte waiting.
    #[allow(dead_code)] // wasm/interactive path only; see UARTFR_RXFE
    pub(super) fn rx_empty() -> bool {
        mmio_read(UARTFR) & UARTFR_RXFE != 0
    }

    /// Read the data register (low byte = the received character).
    #[allow(dead_code)]
    pub(super) fn rx_read() -> u32 {
        mmio_read(UARTDR)
    }

    /// Acknowledge the receive interrupt sources. PL011: W1C via UARTICR — and the
    /// acknowledge-then-drain order is load-bearing (see `drain_rx`).
    #[allow(dead_code)]
    pub(super) fn irq_ack() {
        mmio_write(UARTICR, UART_INT_RX | UART_INT_RT);
    }

    /// Program the line for interactive use and unmask receive interrupts: 8-bit words,
    /// 16-byte FIFOs (paste robustness — QEMU paces its feed by free FIFO space; depth 1
    /// wedges under load, the paste-freeze bug, plan/12), earliest RX trigger, the
    /// TRM-correct enables, then a clean slate and RX+RT unmasked.
    #[allow(dead_code)]
    pub(super) fn line_init() {
        mmio_write(UARTLCR_H, UARTLCR_H_WLEN8 | UARTLCR_H_FEN);
        mmio_write(UARTIFLS, 0);
        mmio_write(UARTCR, UARTCR_UARTEN | UARTCR_TXE | UARTCR_RXE);
        mmio_write(UARTICR, 0x7FF);
        mmio_write(UARTIMSC, UART_INT_RX | UART_INT_RT);
    }
}

/// DW-APB (Synopsys 8250-family) register layer — the RK3588's UART2 debug console
/// (`board-opi5plus`): 32-bit registers at stride 4 (reg-shift 2 / reg-io-width 4),
/// base verified live at the board's U-Boot prompt. The line (1.5 Mbaud 8n1, FIFOs) is
/// exactly as U-Boot left it: nothing here touches LCR/the divisor/FCR — at 24 MHz the
/// 1.5 Mbaud divisor is exactly 1, and reprogramming a live line is day-two polish.
#[cfg(feature = "board-opi5plus")]
mod hw {
    /// UART2 base on the RK3588 (the Orange Pi 5 Plus debug header).
    const UART_BASE: usize = 0xfeb5_0000;
    /// Receive buffer (read) / transmit holding (write) register, reg 0 at stride 4.
    const DW_RBR_THR: usize = 0x00;
    /// Interrupt enable register, reg 1 at stride 4 (LCR.DLAB is clear — U-Boot leaves
    /// the divisor latched away once the line is up, or THR/RBR could not work).
    const DW_IER: usize = 0x04;
    /// Line status register, reg 5 at stride 4.
    const DW_LSR: usize = 0x14;
    /// IER: received-data-available interrupt (ERBFI). With the FIFOs on (as U-Boot
    /// leaves them — the 64-byte truncation proved it) this also covers the 16550
    /// character-timeout condition, so a lone keystroke below the RX trigger level
    /// still raises the interrupt after four character times.
    const DW_IER_ERBFI: u32 = 1 << 0;
    /// LSR: data ready.
    const DW_LSR_DR: u32 = 1 << 0;
    /// LSR: transmit holding register empty.
    const DW_LSR_THRE: u32 = 1 << 5;
    /// LSR: transmitter empty (FIFO *and* shift register — everything is on the wire).
    const DW_LSR_TEMT: u32 = 1 << 6;

    fn mmio_read(offset: usize) -> u32 {
        // SAFETY: `UART_BASE + offset` is a valid DW-APB UART register on the RK3588;
        // `crate::mmio` pins the access to a syndrome-valid GPR form.
        unsafe { crate::mmio::read_u32(UART_BASE + offset) }
    }

    fn mmio_write(offset: usize, value: u32) {
        // SAFETY: as above, for writes.
        unsafe { crate::mmio::write_u32(UART_BASE + offset, value) }
    }

    /// Transmit not ready (holding register still full).
    pub(super) fn tx_busy() -> bool {
        mmio_read(DW_LSR) & DW_LSR_THRE == 0
    }

    /// Write one byte to the transmit holding register.
    pub(super) fn tx_write(byte: u8) {
        mmio_write(DW_RBR_THR, u32::from(byte));
    }

    /// No received byte waiting.
    #[allow(dead_code)] // wasm/interactive path only
    pub(super) fn rx_empty() -> bool {
        mmio_read(DW_LSR) & DW_LSR_DR == 0
    }

    /// Read the receive buffer (low byte = the received character).
    #[allow(dead_code)]
    pub(super) fn rx_read() -> u32 {
        mmio_read(DW_RBR_THR)
    }

    /// Acknowledge receive interrupt sources: nothing to do on the DW-APB — reading RBR
    /// (the drain that follows) clears the receive condition; there is no W1C latch to
    /// race, so the PL011's acknowledge-then-drain ordering concern does not exist here.
    #[allow(dead_code)]
    pub(super) fn irq_ack() {}

    /// Unmask the receive interrupt (ERBFI — received data available + character
    /// timeout) so an arriving byte asserts UART2's GIC SPI and [`super::drain_rx`]
    /// empties the 64-byte RX FIFO at line rate (the exactly-64-byte truncation fix,
    /// GAPS 2026-06-08). Everything else about the line — LCR, the divisor, FCR —
    /// stays exactly as U-Boot programmed it (1.5 Mbaud 8n1, FIFOs on): reprogramming
    /// a live line is banned bench doctrine, and IER is the one register the fix needs.
    /// The transmit (ETBEI) and line-status (ELSI) interrupts stay masked — transmit
    /// polls, and a line error simply reads as data the shell ignores.
    #[allow(dead_code)]
    pub(super) fn line_init() {
        mmio_write(DW_IER, DW_IER_ERBFI);
    }

    /// Everything transmitted is on the wire (FIFO and shift register both empty).
    pub(super) fn tx_idle() -> bool {
        mmio_read(DW_LSR) & DW_LSR_TEMT != 0
    }
}

/// Block (bounded) until the transmit path is fully drained — FIFO and shift register —
/// so output already printed survives an imminent PSCI SYSTEM_RESET. At 1.5 Mbaud a full
/// 16-byte FIFO takes ~107 µs; the bound (~50 M spins) is pure paranoia against a wedged
/// transmitter, in which case losing the tail beats hanging the reset.
#[cfg(feature = "board-opi5plus")]
pub fn tx_drain() {
    for _ in 0..50_000_000u32 {
        if hw::tx_idle() {
            return;
        }
        core::hint::spin_loop();
    }
}

/// Boot-bisection beacon (board profile): bang one raw char straight at the DW UART THR,
/// polling LSR THRE — usable from any boot stage, pre- and post-MMU (the UART window is
/// identity-mapped Device memory once translation is on, and plain physical before).
/// The asm twin lives in boot.rs (`eo9_beacon`) for the stages before Rust runs.
/// Call through the [`crate::beacon!`] macro so non-board builds stay byte-identical.
#[cfg(feature = "board-opi5plus")]
pub fn beacon_raw(c: u8) {
    put_byte(c);
}

/// Write one byte, spinning while the transmitter is busy.
///
/// This is the console TX chokepoint — every `kprintln!`/`kprint!`, shell echo, boot
/// beacon and panic report funnels through here — so the board profile's fbcon tee
/// hangs off it: after the byte is handed to the UART it is also fed to the HDMI
/// console when fbcon is active (one relaxed load when it is not; nothing at all on
/// non-board builds). Serial first: the transcript is the bench instrument, fbcon is
/// the tee.
pub fn put_byte(byte: u8) {
    while hw::tx_busy() {
        core::hint::spin_loop();
    }
    hw::tx_write(byte);
    #[cfg(feature = "board-opi5plus")]
    crate::fbcon::tee_byte(byte);
}

/// Read one received byte if one is waiting (non-blocking; QEMU feeds the RX FIFO from
/// stdin under `-nographic`). Returns `None` when the receive FIFO is empty.
///
/// Used as a fallback before the RX interrupt is enabled; once [`enable_rx_interrupt`] has
/// run the interrupt handler ([`drain_rx`]) moves bytes into [`RX_RING`] and the read-line
/// provider consumes them via [`ring_get_byte`] instead — so the core can `wfi`-idle and be
/// woken by a keystroke rather than polling the data register.
#[allow(dead_code)] // wasm/interactive path only
pub fn try_get_byte() -> Option<u8> {
    if hw::rx_empty() {
        None
    } else {
        Some((hw::rx_read() & 0xff) as u8)
    }
}

// --- Interrupt-driven receive -------------------------------------------------------------
//
// The UART raises its interrupt (routed through the GIC as an SPI — [`RX_INTID`]) when
// receive data arrives. The handler drains the RX FIFO into a small single-producer/
// single-consumer ring (src/rxring.rs): the interrupt context is the only producer and the
// read-line provider on the boot core is the only consumer, so head/tail atomics are
// sufficient (no lock). This decouples "a byte arrived" (wakes `wfi`) from "the shell
// consumed it" and keeps a level-sensitive RX interrupt from re-firing — the handler
// empties the FIFO before returning.

/// GIC INTID of the console UART's receive interrupt, dispatched in `exceptions::kirq`
/// and forwarded in `mod.rs::interrupts_init`:
///
/// * QEMU `virt`: the PL011 on SPI 33 (the machine's fixed irqmap).
/// * `board-opi5plus`: the RK3588's UART2 (DW-APB at `0xfeb5_0000`) on **GIC SPI 333**,
///   INTID 32 + 333 = **365**, level-high — verified from the live board's vendor
///   control FDT (`.claude/board-bringup/vendor-control-fdt.dtb`, node
///   `serial@feb50000`: `interrupts = <0x00 0x14d 0x04>`); mainline
///   `rk3588-base.dtsi`'s uart2 carries the same triple.
#[cfg(not(feature = "board-opi5plus"))]
pub const RX_INTID: u32 = 33;
#[cfg(feature = "board-opi5plus")]
pub const RX_INTID: u32 = 365;

static RX_RING: crate::rxring::RxRing = crate::rxring::RxRing::new();

/// Enable the UART's receive (and, on the PL011, receive-timeout) interrupt so an arriving
/// byte asserts the UART's GIC line ([`RX_INTID`]). Call once during boot after the GIC
/// forwards that SPI (`mod.rs::interrupts_init`). On the board profile this is the one-IER
/// write that fixed the 64-byte console truncation (see the board `hw::line_init`).
///
/// On QEMU it also programs the line for interactive use: 8-bit words, 16-byte FIFOs.
/// The FIFO matters for paste robustness — QEMU paces its character feed by the device's
/// free FIFO space (`pl011_can_receive` returns `depth - read_count`), and with FIFOs off
/// the depth is 1, so *every byte* stalls the host-side feed until the guest completes a
/// full interrupt + drain + data-register-read round-trip. Under host load those per-byte
/// pause/resume cycles are where the chardev flow has been observed to wedge permanently
/// (the paste-freeze bug, plan/12); with a 16-deep FIFO the feed pauses at most once per
/// 16 bytes. On real hardware the FIFO is equally desirable (fewer interrupts per burst).
#[allow(dead_code)] // wasm/interactive path only; not the feature-less CI build
pub fn enable_rx_interrupt() {
    hw::line_init();
}

/// Move every byte waiting in the RX FIFO into [`RX_RING`]; returns how many moved.
/// Bytes beyond the ring's capacity are dropped (paste overflow loses characters; the
/// console never goes deaf). Caller must be the sole producer at the time of the call:
/// either the IRQ handler, or thread context with IRQs masked ([`scavenge_rx`]).
fn fifo_to_ring() -> usize {
    RX_RING.drain(|| {
        if hw::rx_empty() {
            None
        } else {
            Some((hw::rx_read() & 0xff) as u8)
        }
    })
}

/// Interrupt handler body: clear the UART's RX/RT interrupt sources, then drain every
/// waiting byte into [`RX_RING`]. Called from the GIC IRQ dispatch (src/exceptions.rs)
/// when the UART's SPI ([`RX_INTID`]) fires.
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
    hw::irq_ack();
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
pub fn scavenge_rx(now_ns: u64) -> usize {
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
            // The FIFO is empty (checked under the mask just above): this read returns
            // stale data and exists purely for QEMU's unconditional `accept_input` side
            // effect, which resumes a wedged character feed (harmless stale read on real
            // hardware).
            let _ = hw::rx_read();
        }
    }
    // SAFETY: restore IRQ delivery.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
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
/// returns `false` (src/rxring.rs `take_ctrl_c`).
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
