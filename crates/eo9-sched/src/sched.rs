//! The scheduler proper: task table, run queue, and conserved fuel accounts.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;

use crate::fuel::{Fuel, FuelError, FuelLedger};
use crate::policy::{DeterministicPolicy, FairPolicy, Policy};

/// An abstract task identifier.
///
/// The scheduler knows nothing about what a task *is* — the embedder maps ids to whatever it
/// actually executes. Ids are allocated by [`Scheduler::spawn`], are unique within one
/// scheduler, and are **never reused**, so a stale id can never alias a later task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(u64);

impl TaskId {
    /// The id as a plain integer, for the embedder's logs and diagnostics.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Builds an id from a raw value — only for this crate's own tests; embedders obtain ids
    /// from [`Scheduler::spawn`].
    #[cfg(test)]
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task #{}", self.0)
    }
}

/// The lifecycle state of a task, as the scheduler sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Ready to run: waiting in the run queue to be picked.
    Runnable,
    /// Picked by [`Scheduler::pick`] and currently being resumed by the embedder. At most one
    /// task is `Running` at a time (single core, single resumer — see the crate docs).
    Running,
    /// Waiting for an external completion; [`Scheduler::ready`] makes it runnable again.
    Blocked,
    /// Finished, or killed. Stays in the table for its parent to inspect until
    /// [`Scheduler::reap`] removes it.
    Done,
}

/// How a resume ended — the scheduler-relevant half of the Task API's `resume-outcome` (the
/// program's own outcome value stays with the embedder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// The donated fuel ran out before the task blocked or finished; it is still runnable.
    OutOfFuel,
    /// The task is waiting on I/O or another external event.
    Blocked,
    /// The task finished (or was torn down by the embedder during the resume).
    Done,
}

/// An error from a [`Scheduler`] operation. Failed operations leave the scheduler unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedError {
    /// The task id is not in the table: it never existed or was already reaped.
    NoSuchTask(TaskId),
    /// The operation needs a live task, but this one has already finished.
    TaskDone(TaskId),
    /// The operation cannot be applied to the task currently being resumed (for example
    /// [`Scheduler::kill`] or [`Scheduler::reclaim`] — report the resume first).
    Running(TaskId),
    /// [`Scheduler::report`] was called for a task that is not the one currently running.
    NotRunning(TaskId),
    /// [`Scheduler::reap`] was called on a task that has not finished.
    NotDone(TaskId),
    /// [`Scheduler::reap`] was called on a task that still has unreaped children.
    HasChildren(TaskId),
    /// Not enough fuel for the requested donation, reclaim, export, or spend.
    InsufficientFuel {
        /// How much fuel the operation asked for.
        requested: Fuel,
        /// How much fuel the account actually holds.
        available: Fuel,
    },
    /// The node's outstanding fuel (pool plus task balances) would exceed [`Fuel::MAX`].
    FuelOverflow,
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchTask(task) => write!(f, "{task} is not in the task table"),
            Self::TaskDone(task) => write!(f, "{task} has already finished"),
            Self::Running(task) => write!(f, "{task} is currently being resumed"),
            Self::NotRunning(task) => write!(f, "{task} is not the task currently running"),
            Self::NotDone(task) => write!(f, "{task} has not finished"),
            Self::HasChildren(task) => write!(f, "{task} still has unreaped children"),
            Self::InsufficientFuel {
                requested,
                available,
            } => write!(
                f,
                "insufficient fuel: {requested} requested, {available} held"
            ),
            Self::FuelOverflow => write!(f, "the node's outstanding fuel would overflow"),
        }
    }
}

impl core::error::Error for SchedError {}

/// A snapshot of the scheduler's fuel books, for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuelAudit {
    /// Lifetime fuel this node received ([`Scheduler::refuel`]).
    pub imported: u128,
    /// Lifetime fuel consumed by resumed tasks ([`Scheduler::report`]).
    pub burned: u128,
    /// Lifetime fuel handed back out of this node ([`Scheduler::export`]).
    pub exported: u128,
    /// Fuel currently sitting in the node's own pool.
    pub pool: Fuel,
    /// Fuel currently held by tasks.
    pub held_by_tasks: u128,
}

impl FuelAudit {
    /// The conservation law: everything imported was burned, exported, or is still held.
    #[must_use]
    pub fn is_conserved(&self) -> bool {
        self.imported == self.burned + self.exported + u128::from(self.pool) + self.held_by_tasks
    }
}

/// The scheduler node's internal fuel accounts: its own undistributed pool plus one account
/// per tracked task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Account {
    /// Fuel donated to this scheduler node and not yet donated onward.
    Pool,
    /// Fuel donated to a task and not yet spent by it.
    Task(TaskId),
}

/// One row of the task table.
#[derive(Debug)]
struct Task {
    state: TaskState,
    parent: Option<TaskId>,
    /// Unreaped children, in spawn order.
    children: Vec<TaskId>,
}

/// The scheduler: a task table, a run queue behind a [`Policy`], and conserved fuel accounts.
///
/// The scheduler never runs anything itself — it decides, the embedder executes. See the crate
/// docs for the resume cycle and the invariants (single resumer per task, fuel conservation,
/// ids never reused). Single-core for now: one run queue, at most one task in flight.
#[derive(Debug)]
pub struct Scheduler<P: Policy> {
    tasks: BTreeMap<TaskId, Task>,
    next_id: u64,
    policy: P,
    ledger: FuelLedger<Account>,
    running: Option<TaskId>,
}

impl Scheduler<DeterministicPolicy> {
    /// A scheduler using the deterministic (lowest-id-first) policy.
    #[must_use]
    pub fn deterministic() -> Self {
        Self::new(DeterministicPolicy::new())
    }
}

impl Scheduler<FairPolicy> {
    /// A scheduler using the fair (round-robin) policy.
    #[must_use]
    pub fn fair() -> Self {
        Self::new(FairPolicy::new())
    }
}

impl<P: Policy> Scheduler<P> {
    /// A scheduler with no tasks and no fuel, using `policy` for its run queue.
    pub fn new(policy: P) -> Self {
        let mut ledger = FuelLedger::new();
        ledger
            .open(Account::Pool)
            .expect("a fresh ledger cannot already hold the pool account");
        Self {
            tasks: BTreeMap::new(),
            next_id: 0,
            policy,
            ledger,
            running: None,
        }
    }

    // --- task lifecycle ------------------------------------------------------------------

    /// Creates a task. It starts [`Runnable`](TaskState::Runnable) with no fuel, queued behind
    /// whatever is already runnable, and is recorded as a child of `parent` if one is given.
    pub fn spawn(&mut self, parent: Option<TaskId>) -> Result<TaskId, SchedError> {
        if let Some(parent_id) = parent {
            match self.state(parent_id)? {
                TaskState::Done => return Err(SchedError::TaskDone(parent_id)),
                TaskState::Runnable | TaskState::Running | TaskState::Blocked => {}
            }
        }
        let id = TaskId(self.next_id);
        self.next_id += 1;
        self.ledger
            .open(Account::Task(id))
            .expect("task ids are never reused, so the account cannot already exist");
        self.tasks.insert(
            id,
            Task {
                state: TaskState::Runnable,
                parent,
                children: Vec::new(),
            },
        );
        if let Some(parent_id) = parent {
            self.tasks
                .get_mut(&parent_id)
                .expect("the parent's existence was checked above")
                .children
                .push(id);
        }
        self.policy.enqueue(id);
        Ok(id)
    }

    /// Marks a task done without it having run to completion.
    ///
    /// The task must not be the one currently being resumed — finish that resume and
    /// [`report`](Self::report) it first. Its unspent fuel returns to the pool. Its children
    /// are untouched: whether to kill them too is the embedder's policy, not the scheduler's.
    /// Killing a task that is already done is a no-op — kills legitimately race with exits.
    pub fn kill(&mut self, task: TaskId) -> Result<(), SchedError> {
        match self.state(task)? {
            TaskState::Running => Err(SchedError::Running(task)),
            TaskState::Done => Ok(()),
            state @ (TaskState::Runnable | TaskState::Blocked) => {
                if state == TaskState::Runnable {
                    let removed = self.policy.remove(task);
                    debug_assert!(removed, "a runnable task is always queued");
                }
                self.reclaim_all(task);
                self.tasks
                    .get_mut(&task)
                    .expect("existence was checked by state()")
                    .state = TaskState::Done;
                Ok(())
            }
        }
    }

    /// Removes a finished task from the table.
    ///
    /// The task must be [`Done`](TaskState::Done) and all of its children must already have
    /// been reaped, so the parent/child structure never dangles. Its fuel account is closed —
    /// it was emptied back into the pool when the task finished.
    pub fn reap(&mut self, task: TaskId) -> Result<(), SchedError> {
        let entry = self.tasks.get(&task).ok_or(SchedError::NoSuchTask(task))?;
        if entry.state != TaskState::Done {
            return Err(SchedError::NotDone(task));
        }
        if !entry.children.is_empty() {
            return Err(SchedError::HasChildren(task));
        }
        let parent = entry.parent;
        self.ledger
            .close(Account::Task(task))
            .expect("a done task's fuel was returned to the pool when it finished");
        self.tasks.remove(&task);
        if let Some(parent_id) = parent {
            self.tasks
                .get_mut(&parent_id)
                .expect("a parent cannot be reaped before its children")
                .children
                .retain(|&child| child != task);
        }
        Ok(())
    }

    // --- readiness -----------------------------------------------------------------------

    /// Records that an external completion may have made `task` runnable.
    ///
    /// A blocked task becomes runnable and joins the run queue (returns `true`). Spurious
    /// wakes are fine: a task that is already runnable, currently running, or already done is
    /// left untouched (returns `false`) — completions legitimately race with exits and kills.
    pub fn ready(&mut self, task: TaskId) -> Result<bool, SchedError> {
        let entry = self
            .tasks
            .get_mut(&task)
            .ok_or(SchedError::NoSuchTask(task))?;
        if entry.state == TaskState::Blocked {
            entry.state = TaskState::Runnable;
            self.policy.enqueue(task);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // --- the resume cycle ----------------------------------------------------------------

    /// Picks the next task to resume, according to the policy, and marks it
    /// [`Running`](TaskState::Running). Returns `None` if no task is runnable.
    ///
    /// The caller is now that task's **single resumer**: it resumes the task (with at most the
    /// task's fuel balance) and then calls [`report`](Self::report) exactly once before
    /// picking again.
    ///
    /// # Panics
    ///
    /// Panics if a task is already running — a violation of the single-resumer invariant (see
    /// the crate docs), which is an embedder bug and not a recoverable condition.
    pub fn pick(&mut self) -> Option<TaskId> {
        assert!(
            self.running.is_none(),
            "single-resumer invariant violated: pick() while a task is still being resumed"
        );
        let task = self.policy.dequeue()?;
        let entry = self
            .tasks
            .get_mut(&task)
            .expect("queued tasks are always in the table");
        debug_assert_eq!(
            entry.state,
            TaskState::Runnable,
            "queued tasks are always runnable"
        );
        entry.state = TaskState::Running;
        self.running = Some(task);
        Some(task)
    }

    /// Reports the outcome of resuming the task returned by the last [`pick`](Self::pick).
    ///
    /// `spent` is the fuel the resume actually consumed: it is burned from the task's account
    /// and must not exceed what the task held (the embedder hands the execution engine at most
    /// the task's balance, so an excess spend is an embedder bug and is rejected). The outcome
    /// decides the next state: out-of-fuel re-queues the task, blocked parks it until
    /// [`ready`](Self::ready), done retires it and returns its unspent fuel to the pool.
    pub fn report(
        &mut self,
        task: TaskId,
        spent: Fuel,
        outcome: ResumeOutcome,
    ) -> Result<(), SchedError> {
        if self.running != Some(task) {
            return Err(SchedError::NotRunning(task));
        }
        self.ledger
            .burn(Account::Task(task), spent)
            .map_err(Self::fuel_err)?;
        let entry = self
            .tasks
            .get_mut(&task)
            .expect("the running task is always in the table");
        match outcome {
            ResumeOutcome::OutOfFuel => {
                entry.state = TaskState::Runnable;
                self.policy.enqueue(task);
            }
            ResumeOutcome::Blocked => entry.state = TaskState::Blocked,
            ResumeOutcome::Done => {
                entry.state = TaskState::Done;
                self.reclaim_all(task);
            }
        }
        self.running = None;
        Ok(())
    }

    // --- fuel ----------------------------------------------------------------------------

    /// Credits `amount` fuel to the node's pool: this scheduler's own incoming donation.
    ///
    /// The caller asserts that it really received this fuel — from its parent's `resume`, or,
    /// at the root, from the platform's timer quantum. Conservation is only as honest as this
    /// call. Fails if the node's outstanding fuel (pool plus task balances) would exceed
    /// [`Fuel::MAX`]; keeping the outstanding total within `Fuel::MAX` is what lets internal
    /// movements (donate, reclaim, retirement of a finished task's balance) never overflow.
    pub fn refuel(&mut self, amount: Fuel) -> Result<(), SchedError> {
        if self.ledger.circulating() + u128::from(amount) > u128::from(Fuel::MAX) {
            return Err(SchedError::FuelOverflow);
        }
        self.ledger
            .import(Account::Pool, amount)
            .expect("outstanding fuel is bounded by Fuel::MAX, so the pool cannot overflow");
        Ok(())
    }

    /// Hands `amount` fuel from the node's pool back out of this scheduler, to whoever donated
    /// it. The counterpart of [`refuel`](Self::refuel).
    pub fn export(&mut self, amount: Fuel) -> Result<(), SchedError> {
        self.ledger
            .export(Account::Pool, amount)
            .map_err(Self::fuel_err)
    }

    /// Donates `amount` fuel from the node's pool to `task`, for its next (or current) resume.
    ///
    /// The task must not have finished. Donating to the task currently being resumed is
    /// allowed — the usual cycle is pick, then top the picked task up from the pool, then
    /// resume it with its balance.
    pub fn donate(&mut self, task: TaskId, amount: Fuel) -> Result<(), SchedError> {
        match self.state(task)? {
            TaskState::Done => Err(SchedError::TaskDone(task)),
            TaskState::Runnable | TaskState::Running | TaskState::Blocked => self
                .ledger
                .transfer(Account::Pool, Account::Task(task), amount)
                .map_err(Self::fuel_err),
        }
    }

    /// Takes `amount` unspent fuel back from `task` into the node's pool.
    ///
    /// Not allowed while the task is being resumed (the fuel may be in the execution engine's
    /// hands right now) or after it has finished (its fuel already returned to the pool).
    pub fn reclaim(&mut self, task: TaskId, amount: Fuel) -> Result<(), SchedError> {
        match self.state(task)? {
            TaskState::Running => Err(SchedError::Running(task)),
            TaskState::Done => Err(SchedError::TaskDone(task)),
            TaskState::Runnable | TaskState::Blocked => self
                .ledger
                .transfer(Account::Task(task), Account::Pool, amount)
                .map_err(Self::fuel_err),
        }
    }

    /// The fuel currently sitting in the node's own pool.
    #[must_use]
    pub fn pool(&self) -> Fuel {
        self.ledger
            .balance(Account::Pool)
            .expect("the pool account always exists")
    }

    /// The unspent fuel currently held by `task`.
    pub fn fuel_of(&self, task: TaskId) -> Result<Fuel, SchedError> {
        self.state(task)?;
        Ok(self
            .ledger
            .balance(Account::Task(task))
            .expect("every tracked task has a fuel account"))
    }

    /// A snapshot of the fuel books.
    #[must_use]
    pub fn fuel_audit(&self) -> FuelAudit {
        let pool = self.pool();
        FuelAudit {
            imported: self.ledger.imported(),
            burned: self.ledger.burned(),
            exported: self.ledger.exported(),
            pool,
            held_by_tasks: self.ledger.circulating() - u128::from(pool),
        }
    }

    // --- queries -------------------------------------------------------------------------

    /// The state of `task`.
    pub fn state(&self, task: TaskId) -> Result<TaskState, SchedError> {
        self.tasks
            .get(&task)
            .map(|entry| entry.state)
            .ok_or(SchedError::NoSuchTask(task))
    }

    /// The parent of `task`, if it has one.
    pub fn parent(&self, task: TaskId) -> Result<Option<TaskId>, SchedError> {
        self.tasks
            .get(&task)
            .map(|entry| entry.parent)
            .ok_or(SchedError::NoSuchTask(task))
    }

    /// The unreaped children of `task`, in spawn order.
    pub fn children(&self, task: TaskId) -> Result<&[TaskId], SchedError> {
        self.tasks
            .get(&task)
            .map(|entry| entry.children.as_slice())
            .ok_or(SchedError::NoSuchTask(task))
    }

    /// All tracked (unreaped) task ids, in id order.
    pub fn tasks(&self) -> impl Iterator<Item = TaskId> + '_ {
        self.tasks.keys().copied()
    }

    /// The task currently being resumed, if any.
    #[must_use]
    pub fn running(&self) -> Option<TaskId> {
        self.running
    }

    /// Whether any task is waiting in the run queue.
    #[must_use]
    pub fn has_runnable(&self) -> bool {
        !self.policy.is_empty()
    }

    /// The number of tasks that have not finished (runnable, running, or blocked).
    #[must_use]
    pub fn live_tasks(&self) -> usize {
        self.tasks
            .values()
            .filter(|entry| entry.state != TaskState::Done)
            .count()
    }

    /// Whether there is nothing to do right now: no task is runnable and none is being
    /// resumed. If live tasks remain they are all blocked, and the embedder should drain its
    /// completion queues or wait on [`Platform::idle`](crate::Platform::idle).
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.running.is_none() && self.policy.is_empty()
    }

    // --- internals -----------------------------------------------------------------------

    /// Returns all of `task`'s unspent fuel to the pool. Infallible: the node's outstanding
    /// fuel never exceeds [`Fuel::MAX`] (enforced by [`refuel`](Self::refuel)), so the pool
    /// cannot overflow.
    fn reclaim_all(&mut self, task: TaskId) {
        let account = Account::Task(task);
        let balance = self
            .ledger
            .balance(account)
            .expect("every tracked task has a fuel account");
        if balance > 0 {
            self.ledger
                .transfer(account, Account::Pool, balance)
                .expect("outstanding fuel is bounded by Fuel::MAX, so the pool cannot overflow");
        }
    }

    /// Maps an internal ledger error onto the scheduler's error vocabulary.
    fn fuel_err(err: FuelError<Account>) -> SchedError {
        match err {
            FuelError::Insufficient {
                requested,
                available,
                ..
            } => SchedError::InsufficientFuel {
                requested,
                available,
            },
            FuelError::BalanceOverflow(_) => SchedError::FuelOverflow,
            // The scheduler opens exactly one account per tracked task (plus the pool), never
            // double-opens, and only closes emptied accounts, so these cannot reach here.
            FuelError::NoSuchAccount(_)
            | FuelError::AccountExists(_)
            | FuelError::NonEmptyClose { .. } => {
                unreachable!("the scheduler keeps exactly one open account per tracked task")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResumeOutcome, SchedError, Scheduler, TaskState};

    #[test]
    fn spawn_records_parent_and_children() {
        let mut sched = Scheduler::deterministic();
        let root = sched.spawn(None).unwrap();
        let child_a = sched.spawn(Some(root)).unwrap();
        let child_b = sched.spawn(Some(root)).unwrap();

        assert_eq!(sched.parent(root).unwrap(), None);
        assert_eq!(sched.parent(child_a).unwrap(), Some(root));
        assert_eq!(sched.children(root).unwrap(), &[child_a, child_b]);
        assert_eq!(sched.state(child_b).unwrap(), TaskState::Runnable);
        assert_eq!(sched.live_tasks(), 3);
        assert!(sched.has_runnable());
    }

    #[test]
    fn the_resume_cycle_walks_the_states() {
        let mut sched = Scheduler::deterministic();
        let task = sched.spawn(None).unwrap();
        sched.refuel(100).unwrap();

        // Out of fuel: back onto the queue.
        assert_eq!(sched.pick(), Some(task));
        assert_eq!(sched.state(task).unwrap(), TaskState::Running);
        assert_eq!(sched.running(), Some(task));
        sched.donate(task, 10).unwrap();
        sched.report(task, 10, ResumeOutcome::OutOfFuel).unwrap();
        assert_eq!(sched.state(task).unwrap(), TaskState::Runnable);
        assert_eq!(sched.running(), None);

        // Blocked: parked until ready().
        assert_eq!(sched.pick(), Some(task));
        sched.donate(task, 10).unwrap();
        sched.report(task, 4, ResumeOutcome::Blocked).unwrap();
        assert_eq!(sched.state(task).unwrap(), TaskState::Blocked);
        assert!(sched.is_idle());
        assert_eq!(sched.fuel_of(task).unwrap(), 6);

        assert!(sched.ready(task).unwrap());
        assert!(!sched.ready(task).unwrap()); // spurious wake: no-op
        assert_eq!(sched.state(task).unwrap(), TaskState::Runnable);

        // Done: retired, leftover fuel back to the pool.
        assert_eq!(sched.pick(), Some(task));
        sched.report(task, 2, ResumeOutcome::Done).unwrap();
        assert_eq!(sched.state(task).unwrap(), TaskState::Done);
        assert_eq!(sched.fuel_of(task).unwrap(), 0);
        assert_eq!(sched.pool(), 84);
        assert_eq!(sched.live_tasks(), 0);

        let audit = sched.fuel_audit();
        assert!(audit.is_conserved());
        assert_eq!(audit.burned, 16);

        sched.reap(task).unwrap();
        assert_eq!(sched.state(task), Err(SchedError::NoSuchTask(task)));
    }

    #[test]
    fn report_rejects_the_wrong_task_and_excess_spend() {
        let mut sched = Scheduler::deterministic();
        let a = sched.spawn(None).unwrap();
        let b = sched.spawn(None).unwrap();
        sched.refuel(20).unwrap();
        sched.donate(a, 10).unwrap();

        assert_eq!(
            sched.report(a, 0, ResumeOutcome::Done),
            Err(SchedError::NotRunning(a))
        );

        assert_eq!(sched.pick(), Some(a));
        assert_eq!(
            sched.report(b, 0, ResumeOutcome::Done),
            Err(SchedError::NotRunning(b))
        );
        assert_eq!(
            sched.report(a, 11, ResumeOutcome::Done),
            Err(SchedError::InsufficientFuel {
                requested: 11,
                available: 10
            })
        );
        // The failed reports changed nothing; a correct report still works.
        sched.report(a, 10, ResumeOutcome::Done).unwrap();
        assert_eq!(sched.state(a).unwrap(), TaskState::Done);
        assert!(sched.fuel_audit().is_conserved());
    }

    #[test]
    #[should_panic(expected = "single-resumer invariant violated")]
    fn picking_twice_without_reporting_panics() {
        let mut sched = Scheduler::deterministic();
        sched.spawn(None).unwrap();
        sched.spawn(None).unwrap();
        let _ = sched.pick();
        let _ = sched.pick();
    }

    #[test]
    fn kill_retires_a_task_and_returns_its_fuel() {
        let mut sched = Scheduler::deterministic();
        let a = sched.spawn(None).unwrap();
        let b = sched.spawn(None).unwrap();
        sched.refuel(50).unwrap();
        sched.donate(a, 30).unwrap();

        sched.kill(a).unwrap();
        assert_eq!(sched.state(a).unwrap(), TaskState::Done);
        assert_eq!(sched.pool(), 50);
        sched.kill(a).unwrap(); // idempotent

        // The killed task is out of the run queue; only b remains.
        assert_eq!(sched.pick(), Some(b));
        assert_eq!(sched.kill(b), Err(SchedError::Running(b)));
        sched.report(b, 0, ResumeOutcome::Blocked).unwrap();
        sched.kill(b).unwrap();
        assert_eq!(sched.live_tasks(), 0);
        assert!(sched.fuel_audit().is_conserved());
    }

    #[test]
    fn reap_requires_done_and_no_children() {
        let mut sched = Scheduler::deterministic();
        let root = sched.spawn(None).unwrap();
        let child = sched.spawn(Some(root)).unwrap();

        assert_eq!(sched.reap(root), Err(SchedError::NotDone(root)));
        sched.kill(root).unwrap();
        assert_eq!(sched.reap(root), Err(SchedError::HasChildren(root)));

        sched.kill(child).unwrap();
        sched.reap(child).unwrap();
        assert_eq!(sched.children(root).unwrap(), &[]);
        sched.reap(root).unwrap();
        assert_eq!(sched.tasks().count(), 0);
    }

    #[test]
    fn spawning_under_a_finished_parent_is_rejected() {
        let mut sched = Scheduler::deterministic();
        let root = sched.spawn(None).unwrap();
        sched.kill(root).unwrap();
        assert_eq!(sched.spawn(Some(root)), Err(SchedError::TaskDone(root)));
    }

    #[test]
    fn fuel_movements_are_checked() {
        let mut sched = Scheduler::fair();
        let task = sched.spawn(None).unwrap();

        assert_eq!(
            sched.donate(task, 1),
            Err(SchedError::InsufficientFuel {
                requested: 1,
                available: 0
            })
        );
        sched.refuel(10).unwrap();
        sched.donate(task, 7).unwrap();
        sched.reclaim(task, 3).unwrap();
        assert_eq!(sched.fuel_of(task).unwrap(), 4);
        assert_eq!(sched.pool(), 6);

        sched.export(6).unwrap();
        assert_eq!(sched.pool(), 0);
        assert_eq!(
            sched.export(1),
            Err(SchedError::InsufficientFuel {
                requested: 1,
                available: 0
            })
        );

        // Outstanding fuel may never exceed Fuel::MAX.
        sched.refuel(u64::MAX - 4).unwrap();
        assert_eq!(sched.refuel(1), Err(SchedError::FuelOverflow));

        assert!(sched.fuel_audit().is_conserved());
    }

    #[test]
    fn ids_are_never_reused() {
        let mut sched = Scheduler::deterministic();
        let a = sched.spawn(None).unwrap();
        sched.kill(a).unwrap();
        sched.reap(a).unwrap();
        let b = sched.spawn(None).unwrap();
        assert_ne!(a, b);
        assert_eq!(sched.state(a), Err(SchedError::NoSuchTask(a)));
    }
}
