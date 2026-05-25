//! Property tests: fuel conservation holds under arbitrary donate/spend sequences.
//!
//! Two layers are exercised with a deterministic pseudo-random operation stream (no external
//! crates): the standalone [`FuelLedger`], and the [`Scheduler`]'s fuel bookkeeping through
//! its public API. After every operation — successful or rejected — the conservation law must
//! hold, and a rejected operation must leave the books untouched.

use eo9_sched::{FuelLedger, ResumeOutcome, Scheduler, TaskId};

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

/// The ledger conserves fuel under arbitrary open/close/import/export/transfer/burn sequences,
/// and failed operations change nothing.
#[test]
fn ledger_conserves_fuel_under_arbitrary_operations() {
    for seed in 0..100 {
        let mut rng = Rng::new(seed);
        let mut ledger: FuelLedger<u8> = FuelLedger::new();

        for _ in 0..500 {
            let account = rng.below(6) as u8;
            let other = rng.below(6) as u8;
            // Amounts occasionally huge, to exercise the insufficient-fuel and overflow paths.
            let amount = if rng.below(10) == 0 {
                u64::MAX - rng.below(1_000)
            } else {
                rng.below(1_000)
            };

            let before = ledger.clone();
            let result = match rng.below(6) {
                0 => ledger.open(account),
                1 => ledger.close(account),
                2 => ledger.import(account, amount),
                3 => ledger.export(account, amount),
                4 => ledger.transfer(account, other, amount),
                _ => ledger.burn(account, amount),
            };

            assert!(
                ledger.is_conserved(),
                "conservation broken for seed {seed}: {ledger:?}"
            );
            if result.is_err() {
                assert_eq!(
                    ledger, before,
                    "a failed operation changed the ledger (seed {seed})"
                );
            }
        }
    }
}

/// The scheduler's books stay conserved under arbitrary spawn/refuel/donate/reclaim/resume/
/// kill/ready/reap/export sequences driven through its public API.
#[test]
fn scheduler_conserves_fuel_under_arbitrary_operations() {
    for seed in 0..100 {
        let mut rng = Rng::new(seed);
        let mut sched = Scheduler::deterministic();
        let mut ids: Vec<TaskId> = Vec::new();

        for _ in 0..500 {
            // Pick a task id to aim operations at; may be live, finished, or already reaped.
            let target = if ids.is_empty() {
                None
            } else {
                Some(ids[rng.below(ids.len() as u64) as usize])
            };
            let amount = rng.below(1_000);

            match rng.below(10) {
                0 => {
                    let parent = if rng.below(2) == 0 { target } else { None };
                    if let Ok(id) = sched.spawn(parent) {
                        ids.push(id);
                    }
                }
                1 => {
                    // Refuel with an occasionally extreme quantum to hit the overflow guard.
                    let quantum = if rng.below(20) == 0 { u64::MAX } else { amount };
                    let _ = sched.refuel(quantum);
                }
                2 => {
                    if let Some(task) = target {
                        let _ = sched.donate(task, amount);
                    }
                }
                3 => {
                    if let Some(task) = target {
                        let _ = sched.reclaim(task, amount);
                    }
                }
                4 => {
                    let _ = sched.export(amount);
                }
                5 => {
                    if let Some(task) = target {
                        let _ = sched.kill(task);
                    }
                }
                6 => {
                    if let Some(task) = target {
                        let _ = sched.ready(task);
                    }
                }
                7 => {
                    if let Some(task) = target {
                        let _ = sched.reap(task);
                    }
                }
                _ => {
                    // A full resume cycle: pick, donate a top-up, spend some of the balance,
                    // and report a random outcome.
                    if let Some(task) = sched.pick() {
                        let _ = sched.donate(task, amount.min(sched.pool()));
                        let balance = sched.fuel_of(task).unwrap();
                        let spent = if balance == 0 {
                            0
                        } else {
                            rng.below(balance + 1)
                        };
                        let outcome = match rng.below(3) {
                            0 => ResumeOutcome::OutOfFuel,
                            1 => ResumeOutcome::Blocked,
                            _ => ResumeOutcome::Done,
                        };
                        sched
                            .report(task, spent, outcome)
                            .expect("spent never exceeds the balance");
                    }
                }
            }

            let audit = sched.fuel_audit();
            assert!(
                audit.is_conserved(),
                "conservation broken for seed {seed}: {audit:?}"
            );
        }
    }
}
