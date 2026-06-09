//! The console receive ring: a single-producer/single-consumer byte ring shared by every
//! architecture's UART driver (PL011 / DW-APB on aarch64, NS16550A on riscv64, the 16550
//! at COM1 on x86_64).
//!
//! The producer is the interrupt path (the IRQ/trap handler draining the device FIFO, or
//! thread context with interrupts masked — the idle-path scavenger), the consumer is the
//! boot core's read-line provider, so head/tail atomics with acquire/release ordering are
//! sufficient: no lock, and the interrupt context never blocks.
//!
//! Kept free of any hardware access so it compiles — and its unit tests run — on the host
//! triple as well as on bare metal (the `ticks` pattern). The tests model the one failure
//! mode that took a bench day to root-cause (the board's 64-byte console truncation,
//! GAPS 2026-06-08): a hardware FIFO of finite depth overflows silently unless the
//! interrupt path drains it at line rate — an idle-paced backstop drain alone loses
//! everything past the FIFO depth.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Ring capacity (power of two; one slot is left empty to distinguish full from empty).
/// Sized to hold the longest line the shell accepts (`MAX_READ_LINE_BYTES`, 4096) so a
/// pasted command line of any accepted length survives even if the consumer is slow to
/// drain — paste robustness, plan/12.
pub(crate) const RX_RING_CAP: usize = 4096;

/// ETX (Ctrl-C) — the interrupt key.
pub(crate) const CTRL_C: u8 = 0x03;

/// Single-producer (interrupt path) / single-consumer (boot core) byte ring for received
/// console input.
pub(crate) struct RxRing {
    buf: UnsafeCell<[u8; RX_RING_CAP]>,
    /// Next index the producer (interrupt path) will write.
    head: AtomicUsize,
    /// Next index the consumer (read-line) will read.
    tail: AtomicUsize,
}

// SAFETY: the only producer is the interrupt path and the only consumer is the boot
// core's read-line poll; access is coordinated through `head`/`tail` with
// acquire/release ordering.
unsafe impl Sync for RxRing {}

impl RxRing {
    /// An empty ring (const, so each architecture can keep its `static RX_RING`).
    pub(crate) const fn new() -> Self {
        RxRing {
            buf: UnsafeCell::new([0; RX_RING_CAP]),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer side: pull bytes out of the device until `next_byte` reports the FIFO
    /// empty, publishing each into the ring; returns how many bytes the device yielded.
    /// Bytes beyond the ring's capacity are dropped (paste overflow loses characters;
    /// the console never goes deaf), but still count toward the return value — the
    /// device was drained either way.
    ///
    /// The caller must be the sole producer at the time of the call: either the
    /// IRQ/trap handler, or thread context with interrupts masked (the scavenger).
    pub(crate) fn drain(&self, mut next_byte: impl FnMut() -> Option<u8>) -> usize {
        let mut moved = 0;
        while let Some(byte) = next_byte() {
            moved += 1;
            let head = self.head.load(Ordering::Relaxed);
            let next = (head + 1) % RX_RING_CAP;
            // Drop the byte if the ring is full rather than overwrite unread input.
            if next != self.tail.load(Ordering::Acquire) {
                // SAFETY: the caller is the sole producer; this slot is not being read
                // (it is at/after `head`, ahead of the consumer's `tail`).
                unsafe { (*self.buf.get())[head] = byte };
                self.head.store(next, Ordering::Release);
            }
        }
        moved
    }

    /// Consumer side: take one byte out of the ring, or `None` if none is waiting.
    pub(crate) fn get_byte(&self) -> Option<u8> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the boot core is the sole consumer; this slot was published by the
        // producer (head moved past it with release ordering, observed by the acquire
        // load above).
        let byte = unsafe { (*self.buf.get())[tail] };
        self.tail.store((tail + 1) % RX_RING_CAP, Ordering::Release);
        Some(byte)
    }

    /// Whether unconsumed bytes are waiting (the scavenger's activity probe; producer or
    /// consumer context).
    pub(crate) fn is_busy(&self) -> bool {
        self.head.load(Ordering::Relaxed) != self.tail.load(Ordering::Relaxed)
    }

    /// Non-destructively scan the waiting input for a Ctrl-C and, if present, consume
    /// the ring up to and including it (flushing pending input through the interrupt,
    /// the usual terminal behaviour) and return `true`. If no Ctrl-C is waiting the
    /// ring is left untouched and this returns `false`. Single-consumer-safe: only the
    /// boot core calls this and `get_byte`, never concurrently, so reading
    /// `tail..head` and advancing `tail` here is sound.
    pub(crate) fn take_ctrl_c(&self) -> bool {
        let head = self.head.load(Ordering::Acquire);
        let mut i = self.tail.load(Ordering::Relaxed);
        while i != head {
            // SAFETY: the boot core is the sole consumer; this slot is published (it is
            // before `head`, which was loaded with acquire ordering).
            let byte = unsafe { (*self.buf.get())[i] };
            let next = (i + 1) % RX_RING_CAP;
            if byte == CTRL_C {
                // Discard everything up to and including the Ctrl-C.
                self.tail.store(next, Ordering::Release);
                return true;
            }
            i = next;
        }
        false
    }

    /// Non-destructively check whether a Ctrl-C is waiting in the ring, without
    /// consuming it (or anything before it). The wasm `eo9:pci` provider's interrupt
    /// `wait` peeks this so a console interrupt aborts a blocked device wait promptly;
    /// the *consuming* check (and the resulting kill) stays the shell's job
    /// (`take_ctrl_c` above).
    pub(crate) fn ctrl_c_pending(&self) -> bool {
        let head = self.head.load(Ordering::Acquire);
        let mut i = self.tail.load(Ordering::Relaxed);
        while i != head {
            // SAFETY: as in `take_ctrl_c`.
            let byte = unsafe { (*self.buf.get())[i] };
            if byte == CTRL_C {
                return true;
            }
            i = (i + 1) % RX_RING_CAP;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{CTRL_C, RX_RING_CAP, RxRing};
    use std::collections::VecDeque;
    use std::vec::Vec;

    /// A model of a hardware receive FIFO of finite depth: the wire side pushes (and
    /// silently loses what does not fit, exactly like the silicon), the CPU side pops.
    struct ModelFifo {
        depth: usize,
        queue: VecDeque<u8>,
        /// Bytes the wire delivered that the full FIFO discarded.
        overflowed: usize,
    }

    impl ModelFifo {
        fn new(depth: usize) -> Self {
            ModelFifo {
                depth,
                queue: VecDeque::new(),
                overflowed: 0,
            }
        }

        /// A byte arrives from the wire; a full FIFO drops it on the floor.
        fn wire_byte(&mut self, byte: u8) {
            if self.queue.len() < self.depth {
                self.queue.push_back(byte);
            } else {
                self.overflowed += 1;
            }
        }

        /// The CPU reads the data register (`None` = FIFO empty).
        fn read(&mut self) -> Option<u8> {
            self.queue.pop_front()
        }
    }

    /// Drain the whole ring into a Vec (consumer side).
    fn consume_all(ring: &RxRing) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(byte) = ring.get_byte() {
            out.push(byte);
        }
        out
    }

    /// The DW-APB UART2 on the board has a 64-byte RX FIFO. A 100-byte line arriving at
    /// line rate with the RX interrupt wired — the handler drains the FIFO every time
    /// the trigger asserts (modelled here as a drain per byte; real trigger levels only
    /// make the handler run *less* often while the FIFO is *less* full) — survives
    /// completely and in order.
    #[test]
    fn interrupt_time_drains_carry_a_line_longer_than_the_fifo() {
        let ring = RxRing::new();
        let mut fifo = ModelFifo::new(64);
        let line: Vec<u8> = (0..100u8).collect();
        for &byte in &line {
            fifo.wire_byte(byte);
            // The RX interrupt fires and the handler empties the FIFO into the ring.
            ring.drain(|| fifo.read());
        }
        assert_eq!(fifo.overflowed, 0);
        assert_eq!(consume_all(&ring), line);
    }

    /// The board bug's mechanism, pinned as a model (GAPS 2026-06-08, "Board console
    /// input truncates at exactly 64 bytes"): with NO interrupt-time drains — input
    /// rescued only by an idle-paced backstop scavenge long after the line finished —
    /// a 64-deep FIFO delivers exactly its depth and silently loses the rest.
    #[test]
    fn a_backstop_only_drain_truncates_at_exactly_the_fifo_depth() {
        let ring = RxRing::new();
        let mut fifo = ModelFifo::new(64);
        for byte in 0..100u8 {
            fifo.wire_byte(byte);
        }
        // The idle backstop finally scavenges, far too late.
        let moved = ring.drain(|| fifo.read());
        assert_eq!(moved, 64);
        assert_eq!(fifo.overflowed, 36);
        assert_eq!(consume_all(&ring), (0..64u8).collect::<Vec<u8>>());
    }

    /// A full ring drops new bytes rather than overwriting unread input, and the
    /// consumer still sees every byte that fit, in order (the console never goes deaf).
    #[test]
    fn a_full_ring_drops_excess_but_keeps_what_fit() {
        let ring = RxRing::new();
        let mut source = (0..RX_RING_CAP + 100).map(|i| (i % 251) as u8);
        let moved = ring.drain(|| source.next());
        // The device was fully drained even though not everything fit.
        assert_eq!(moved, RX_RING_CAP + 100);
        let consumed = consume_all(&ring);
        // One slot stays empty to distinguish full from empty.
        assert_eq!(consumed.len(), RX_RING_CAP - 1);
        for (i, byte) in consumed.iter().enumerate() {
            assert_eq!(*byte, (i % 251) as u8);
        }
    }

    /// Production and consumption interleaved across the wrap boundary lose nothing.
    #[test]
    fn wraparound_preserves_order() {
        let ring = RxRing::new();
        let mut sent = 0usize;
        let mut received = Vec::new();
        // Push/pull in unequal chunks so head and tail wrap several times.
        for round in 0..(4 * RX_RING_CAP / 7) {
            let chunk: Vec<u8> = (0..7).map(|i| ((sent + i) % 256) as u8).collect();
            sent += chunk.len();
            let mut iter = chunk.into_iter();
            ring.drain(|| iter.next());
            for _ in 0..(if round % 2 == 0 { 7 } else { 6 }) {
                if let Some(byte) = ring.get_byte() {
                    received.push(byte);
                }
            }
        }
        received.extend(consume_all(&ring));
        assert_eq!(received.len(), sent);
        for (i, byte) in received.iter().enumerate() {
            assert_eq!(*byte, (i % 256) as u8, "byte {i} out of order");
        }
    }

    /// `take_ctrl_c` consumes through the Ctrl-C (flushing what preceded it) and leaves
    /// what followed; `ctrl_c_pending` observes without consuming.
    #[test]
    fn ctrl_c_scan_and_flush() {
        let ring = RxRing::new();
        let mut input = vec![b'a', b'b', CTRL_C, b'c'].into_iter();
        ring.drain(|| input.next());
        assert!(ring.ctrl_c_pending());
        // The peek consumed nothing.
        assert!(ring.is_busy());
        assert!(ring.take_ctrl_c());
        // Everything up to and including the Ctrl-C is gone; the tail survives.
        assert_eq!(consume_all(&ring), vec![b'c']);
        assert!(!ring.ctrl_c_pending());
        assert!(!ring.take_ctrl_c());
    }
}
