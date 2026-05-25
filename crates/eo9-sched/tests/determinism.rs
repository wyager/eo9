//! Property test: a scripted set of tasks and completions, driven under the deterministic
//! policy, yields an identical execution trace on every run.
//!
//! The "runtime" here is simulated: each scripted task is a sequence of work segments (fuel to
//! consume) separated by blocking points, and the completion that unblocks a task arrives a
//! scripted number of driver steps after it blocks. The driver loop below has the same shape
//! the real runtime uses — refuel from the platform's quantum, deliver completions, pick,
//! resume, report — so the trace it records is exactly the scheduler-visible execution order.

use eo9_sched::{DeterministicPolicy, Policy, ResumeOutcome, Scheduler, TaskId};

/// A small deterministic generator (splitmix64) so the test needs no external crates.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

/// One scripted task: fuel needed per work segment, and for each blocking point between
/// segments, how many driver steps later its completion arrives.
#[derive(Clone, Debug)]
struct ScriptedTask {
    segments: Vec<u64>,
    completion_delays: Vec<u64>,
}

/// A whole scripted workload.
#[derive(Clone, Debug)]
struct Script {
    tasks: Vec<ScriptedTask>,
    /// Fuel the platform grants this scheduler node per driver step (the timer quantum).
    quantum_per_step: u64,
    /// Fuel the node donates to the picked task per resume.
    donation_per_resume: u64,
}

fn random_script(rng: &mut Rng) -> Script {
    let task_count = 1 + rng.below(6) as usize;
    let tasks = (0..task_count)
        .map(|_| {
            let segment_count = 1 + rng.below(4) as usize;
            ScriptedTask {
                segments: (0..segment_count).map(|_| 1 + rng.below(40)).collect(),
                completion_delays: (1..segment_count).map(|_| rng.below(5)).collect(),
            }
        })
        .collect();
    Script {
        tasks,
        quantum_per_step: 8 + rng.below(32),
        donation_per_resume: 4 + rng.below(16),
    }
}

/// One scheduler-visible event. The trace is the sequence of these.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TraceEvent {
    Ready(TaskId),
    Resumed {
        task: TaskId,
        spent: u64,
        outcome: ResumeOutcome,
    },
    Idle,
}

/// The simulated execution state of one scripted task.
struct SimTask {
    segment: usize,
    remaining: u64,
}

/// Drives `script` to completion under `policy` and returns the execution trace.
fn run<P: Policy>(script: &Script, policy: P) -> Vec<TraceEvent> {
    let mut sched = Scheduler::new(policy);
    let mut trace = Vec::new();

    let ids: Vec<TaskId> = script
        .tasks
        .iter()
        .map(|_| sched.spawn(None).unwrap())
        .collect();
    let mut sims: std::collections::BTreeMap<TaskId, SimTask> = ids
        .iter()
        .zip(&script.tasks)
        .map(|(&id, spec)| {
            (
                id,
                SimTask {
                    segment: 0,
                    remaining: spec.segments[0],
                },
            )
        })
        .collect();
    let specs: std::collections::BTreeMap<TaskId, &ScriptedTask> =
        ids.iter().copied().zip(&script.tasks).collect();

    // Completions waiting to be delivered, as (due step, task).
    let mut pending: Vec<(u64, TaskId)> = Vec::new();

    let mut step: u64 = 0;
    while sched.live_tasks() > 0 {
        assert!(step < 100_000, "scripted workload did not terminate");

        // The platform's timer tick: this node is donated one quantum per step.
        sched.refuel(script.quantum_per_step).unwrap();

        // Deliver due completions in (step, task id) order.
        pending.sort_unstable();
        let due: Vec<TaskId> = pending
            .iter()
            .filter(|&&(when, _)| when <= step)
            .map(|&(_, task)| task)
            .collect();
        pending.retain(|&(when, _)| when > step);
        for task in due {
            if sched.ready(task).unwrap() {
                trace.push(TraceEvent::Ready(task));
            }
        }

        // One pick per step: resume the chosen task against its scripted behaviour.
        if let Some(task) = sched.pick() {
            let top_up = script.donation_per_resume.min(sched.pool());
            sched.donate(task, top_up).unwrap();
            let fuel = sched.fuel_of(task).unwrap();

            let sim = sims.get_mut(&task).unwrap();
            let spec = specs[&task];
            let (spent, outcome) = if fuel >= sim.remaining {
                let spent = sim.remaining;
                if sim.segment + 1 == spec.segments.len() {
                    (spent, ResumeOutcome::Done)
                } else {
                    let delay = spec.completion_delays[sim.segment];
                    pending.push((step + delay, task));
                    sim.segment += 1;
                    sim.remaining = spec.segments[sim.segment];
                    (spent, ResumeOutcome::Blocked)
                }
            } else {
                sim.remaining -= fuel;
                (fuel, ResumeOutcome::OutOfFuel)
            };

            sched.report(task, spent, outcome).unwrap();
            trace.push(TraceEvent::Resumed {
                task,
                spent,
                outcome,
            });
        } else {
            // Everything live is blocked: the embedder would wait on Platform::idle here.
            assert!(sched.is_idle());
            trace.push(TraceEvent::Idle);
        }

        // The books balance after every step.
        assert!(sched.fuel_audit().is_conserved());

        step += 1;
    }

    // Every task finished; reap them all and check the final books.
    for &id in &ids {
        sched.reap(id).unwrap();
    }
    assert_eq!(sched.tasks().count(), 0);
    let audit = sched.fuel_audit();
    assert!(audit.is_conserved());
    assert_eq!(audit.held_by_tasks, 0);

    trace
}

/// The deterministic policy yields an identical trace on every run of the same script.
#[test]
fn deterministic_policy_replays_identically() {
    for seed in 0..200 {
        let script = random_script(&mut Rng::new(seed));
        let first = run(&script, DeterministicPolicy::new());
        let second = run(&script, DeterministicPolicy::new());
        let third = run(&script, DeterministicPolicy::new());
        assert_eq!(first, second, "trace diverged for seed {seed}");
        assert_eq!(first, third, "trace diverged for seed {seed}");
    }
}

/// Under the deterministic policy the runnable task with the lowest id always runs next,
/// regardless of the order in which tasks became runnable.
#[test]
fn deterministic_policy_runs_lowest_id_first() {
    let mut sched = Scheduler::deterministic();
    sched.refuel(1_000).unwrap();
    let a = sched.spawn(None).unwrap();
    let b = sched.spawn(None).unwrap();
    let c = sched.spawn(None).unwrap();

    // Run each task once, in id order, until each blocks on I/O.
    for task in [a, b, c] {
        assert_eq!(sched.pick(), Some(task));
        sched.donate(task, 10).unwrap();
        sched.report(task, 10, ResumeOutcome::Blocked).unwrap();
    }

    // Completions arrive in the "wrong" order: c first, then a, then b.
    assert!(sched.ready(c).unwrap());
    assert!(sched.ready(a).unwrap());
    assert!(sched.ready(b).unwrap());

    // The pick order is still id order, not readiness order.
    let mut order = Vec::new();
    while let Some(task) = sched.pick() {
        sched.report(task, 0, ResumeOutcome::Done).unwrap();
        order.push(task);
    }
    assert_eq!(order, vec![a, b, c]);
}

/// The fair policy round-robins: arrival order, with out-of-fuel tasks going to the back.
#[test]
fn fair_policy_round_robins() {
    let mut sched = Scheduler::fair();
    sched.refuel(1_000).unwrap();
    let a = sched.spawn(None).unwrap();
    let b = sched.spawn(None).unwrap();
    let c = sched.spawn(None).unwrap();

    let mut order = Vec::new();
    for _ in 0..6 {
        let task = sched.pick().unwrap();
        sched.donate(task, 1).unwrap();
        // Everyone needs two resumes: first runs out of fuel, second finishes.
        let outcome = if order.contains(&task) {
            ResumeOutcome::Done
        } else {
            ResumeOutcome::OutOfFuel
        };
        sched.report(task, 1, outcome).unwrap();
        order.push(task);
    }

    assert_eq!(order, vec![a, b, c, a, b, c]);
    assert_eq!(sched.live_tasks(), 0);
    assert!(sched.fuel_audit().is_conserved());
}
