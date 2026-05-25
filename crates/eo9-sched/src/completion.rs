//! Completion queues and doorbells: the readiness primitives of the async host side.
//!
//! SPEC.md ("How readiness is implemented"): the OS core implements the host side of the
//! Component Model async ABI with per-task completion queues and edge-triggered doorbells. A
//! backend pushes a completion record and rings the doorbell only on the empty→non-empty
//! transition; on its next resume the task drains the queue and dispatches to its parked
//! waitable-sets. O(1) per completion and at most one wake per batch — the io_uring shape.
//!
//! This module provides those two primitives as reusable types, generic over the completion
//! record; the usermode runtime and the kernel both build their async host side out of them.
//!
//! # Who synchronizes what
//!
//! [`Doorbell`] is atomic and can be shared freely (`&self` methods) — another thread or an
//! interrupt handler may ring it while the consumer polls it. [`CompletionQueue`] is
//! deliberately *not* a concurrent structure: pushes take `&mut self`, and the embedder
//! brackets them with whatever exclusion it already has (a mutex around the task's queue in
//! usermode, a critical section or interrupt-disable window in the kernel). Single-core first;
//! a lock-free multi-producer variant can slot in behind the same push/drain shape when SMP
//! lands.
//!
//! The intended pairing:
//!
//! ```
//! use eo9_sched::{CompletionQueue, Doorbell};
//!
//! let mut queue: CompletionQueue<&str> = CompletionQueue::new();
//! let doorbell = Doorbell::new();
//!
//! // Producer side (a backend completing an operation), under the embedder's exclusion:
//! if queue.push("read #7 done") {
//!     // empty→non-empty edge: ring, and wake the consumer only if the bell was not already rung
//!     if doorbell.ring() {
//!         // wake the task's resumer (mark the task ready / unpark the scheduler thread)
//!     }
//! }
//!
//! // Consumer side (the task, on its next resume):
//! if doorbell.take() {
//!     for record in queue.drain() {
//!         // dispatch `record` to the parked waitable-set it belongs to
//!         assert_eq!(record, "read #7 done");
//!     }
//! }
//! assert!(queue.is_empty());
//! ```

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, Ordering};

/// An edge-triggered wake flag.
///
/// [`ring`](Self::ring) reports the unset→set edge, so a producer wakes the consumer **at most
/// once per batch** of completions; [`take`](Self::take) consumes the signal. All methods are
/// `&self` and atomic, so the doorbell can be rung from an interrupt handler or another thread
/// while the consumer polls it.
#[derive(Debug, Default)]
pub struct Doorbell {
    rung: AtomicBool,
}

impl Doorbell {
    /// A doorbell that has not been rung.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rung: AtomicBool::new(false),
        }
    }

    /// Rings the doorbell. Returns `true` iff this call set it — the edge on which the caller
    /// should wake the consumer. Further rings return `false` until the consumer
    /// [`take`](Self::take)s the signal.
    pub fn ring(&self) -> bool {
        !self.rung.swap(true, Ordering::AcqRel)
    }

    /// Consumes the signal: returns whether the doorbell was rung since the last `take`, and
    /// resets it.
    pub fn take(&self) -> bool {
        self.rung.swap(false, Ordering::AcqRel)
    }

    /// Whether the doorbell is currently rung, without consuming the signal.
    #[must_use]
    pub fn is_rung(&self) -> bool {
        self.rung.load(Ordering::Acquire)
    }
}

/// A FIFO queue of completion records for one consumer (one task).
///
/// [`push`](Self::push) reports the empty→non-empty transition so the producer knows when to
/// ring the consumer's [`Doorbell`]; pushes onto a non-empty queue are silent, which is what
/// makes a completion O(1) with at most one wake per batch. Synchronization is the embedder's
/// job (see the module docs).
#[derive(Debug, Clone)]
pub struct CompletionQueue<T> {
    records: VecDeque<T>,
}

impl<T> CompletionQueue<T> {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            records: VecDeque::new(),
        }
    }

    /// Appends a completion record. Returns `true` iff the queue was empty — the
    /// empty→non-empty edge on which the producer should ring the doorbell.
    pub fn push(&mut self, record: T) -> bool {
        let was_empty = self.records.is_empty();
        self.records.push_back(record);
        was_empty
    }

    /// Removes and returns the oldest record, or `None` if the queue is empty.
    pub fn pop(&mut self) -> Option<T> {
        self.records.pop_front()
    }

    /// Removes and returns all records, oldest first.
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.records.drain(..)
    }

    /// The number of queued records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the queue holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl<T> Default for CompletionQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletionQueue, Doorbell};

    #[test]
    fn doorbell_reports_only_the_edge() {
        let doorbell = Doorbell::new();
        assert!(!doorbell.is_rung());

        assert!(doorbell.ring());
        assert!(!doorbell.ring());
        assert!(doorbell.is_rung());

        assert!(doorbell.take());
        assert!(!doorbell.take());
        assert!(!doorbell.is_rung());

        assert!(doorbell.ring());
    }

    #[test]
    fn queue_reports_only_the_empty_to_non_empty_edge() {
        let mut queue = CompletionQueue::new();
        assert!(queue.push(1));
        assert!(!queue.push(2));
        assert!(!queue.push(3));

        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.len(), 2);
        assert!(!queue.push(4)); // still non-empty: no new edge

        let drained: Vec<i32> = queue.drain().collect();
        assert_eq!(drained, vec![2, 3, 4]);
        assert!(queue.is_empty());

        assert!(queue.push(5)); // empty again: a fresh edge
    }
}
