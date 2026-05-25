//! Run-queue policies.
//!
//! A policy is only an ordering decision: the [`Scheduler`](crate::Scheduler) tells it which
//! tasks are runnable and asks it which one to run next. Two policies ship with the crate:
//!
//! * [`DeterministicPolicy`] — always the lowest-numbered runnable task. The choice is a pure
//!   function of the *set* of runnable task ids, independent of arrival order, so a scripted
//!   workload replays to an identical execution trace on every run. This is the policy of the
//!   deterministic-environment story; it will happily starve high-numbered tasks if lower ones
//!   never block, which is exactly the reproducibility it promises.
//! * [`FairPolicy`] — first-in-first-out round-robin: tasks run in arrival order and go to the
//!   back of the queue when they run out of fuel.

use alloc::collections::{BTreeSet, VecDeque};

use crate::sched::TaskId;

/// A run-queue policy: the scheduler's ordering decision and nothing else.
///
/// # Contract
///
/// The [`Scheduler`](crate::Scheduler) maintains these properties; a custom policy may rely on
/// them and must preserve them:
///
/// * A task is enqueued only when it becomes runnable, and never enqueued again without an
///   intervening `dequeue` or `remove` — the queue never holds duplicates.
/// * `dequeue` removes the returned task from the queue.
/// * `remove` takes a task out of the queue wherever it sits (it is about to be killed).
/// * The policy holds no state about tasks that are not currently queued.
pub trait Policy {
    /// Adds a runnable task to the queue.
    fn enqueue(&mut self, task: TaskId);

    /// Removes and returns the next task to run, or `None` if no task is queued.
    fn dequeue(&mut self) -> Option<TaskId>;

    /// Removes `task` from the queue if present; returns whether it was present.
    fn remove(&mut self, task: TaskId) -> bool;

    /// The number of queued tasks.
    fn len(&self) -> usize;

    /// Whether no task is queued.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Lowest task id first: a stable order, independent of arrival order.
#[derive(Debug, Clone, Default)]
pub struct DeterministicPolicy {
    queue: BTreeSet<TaskId>,
}

impl DeterministicPolicy {
    /// An empty deterministic run queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: BTreeSet::new(),
        }
    }
}

impl Policy for DeterministicPolicy {
    fn enqueue(&mut self, task: TaskId) {
        let inserted = self.queue.insert(task);
        debug_assert!(inserted, "{task} was already queued");
    }

    fn dequeue(&mut self) -> Option<TaskId> {
        self.queue.pop_first()
    }

    fn remove(&mut self, task: TaskId) -> bool {
        self.queue.remove(&task)
    }

    fn len(&self) -> usize {
        self.queue.len()
    }
}

/// First-in-first-out round-robin.
#[derive(Debug, Clone, Default)]
pub struct FairPolicy {
    queue: VecDeque<TaskId>,
}

impl FairPolicy {
    /// An empty round-robin run queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }
}

impl Policy for FairPolicy {
    fn enqueue(&mut self, task: TaskId) {
        debug_assert!(!self.queue.contains(&task), "{task} was already queued");
        self.queue.push_back(task);
    }

    fn dequeue(&mut self) -> Option<TaskId> {
        self.queue.pop_front()
    }

    fn remove(&mut self, task: TaskId) -> bool {
        match self.queue.iter().position(|&queued| queued == task) {
            Some(index) => {
                self.queue.remove(index);
                true
            }
            None => false,
        }
    }

    fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeterministicPolicy, FairPolicy, Policy};
    use crate::sched::TaskId;

    fn id(n: u64) -> TaskId {
        TaskId::from_raw(n)
    }

    #[test]
    fn deterministic_order_ignores_arrival_order() {
        let mut policy = DeterministicPolicy::new();
        for n in [3, 0, 2, 1] {
            policy.enqueue(id(n));
        }
        let order: Vec<TaskId> = core::iter::from_fn(|| policy.dequeue()).collect();
        assert_eq!(order, vec![id(0), id(1), id(2), id(3)]);
        assert!(policy.is_empty());
    }

    #[test]
    fn fair_order_is_arrival_order() {
        let mut policy = FairPolicy::new();
        for n in [3, 0, 2, 1] {
            policy.enqueue(id(n));
        }
        let order: Vec<TaskId> = core::iter::from_fn(|| policy.dequeue()).collect();
        assert_eq!(order, vec![id(3), id(0), id(2), id(1)]);
        assert!(policy.is_empty());
    }

    #[test]
    fn remove_takes_a_task_out_of_either_queue() {
        let mut det = DeterministicPolicy::new();
        let mut fair = FairPolicy::new();
        for n in 0..4 {
            det.enqueue(id(n));
            fair.enqueue(id(n));
        }

        assert!(det.remove(id(2)));
        assert!(!det.remove(id(2)));
        assert!(fair.remove(id(2)));
        assert!(!fair.remove(id(2)));

        assert_eq!(det.len(), 3);
        assert_eq!(fair.len(), 3);
        let det_order: Vec<TaskId> = core::iter::from_fn(|| det.dequeue()).collect();
        let fair_order: Vec<TaskId> = core::iter::from_fn(|| fair.dequeue()).collect();
        assert_eq!(det_order, vec![id(0), id(1), id(3)]);
        assert_eq!(fair_order, vec![id(0), id(1), id(3)]);
    }
}
