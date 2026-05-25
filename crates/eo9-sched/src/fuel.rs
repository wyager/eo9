//! Conserved fuel accounting.
//!
//! Fuel is Eo9's CPU budget: `resume(task, fuel)` donates fuel to a task and runs it on the
//! caller's own CPU time until the fuel is spent, the task blocks, or it finishes (SPEC.md,
//! Execution APIs). Fuel is **conserved** — a scheduler node can only hand out fuel it was
//! itself donated — so CPU budgets compose down the task tree.
//!
//! [`FuelLedger`] is that rule as a data structure: a set of accounts whose balances change
//! only through [`import`](FuelLedger::import) (fuel arriving from outside this node),
//! [`transfer`](FuelLedger::transfer) (conserved movement between accounts),
//! [`burn`](FuelLedger::burn) (fuel consumed by execution), and [`export`](FuelLedger::export)
//! (fuel handed back out of this node). The conservation law
//!
//! ```text
//! imported == burned + exported + Σ balances
//! ```
//!
//! holds after every operation; failed operations change nothing.

use alloc::collections::BTreeMap;
use core::fmt;

/// A quantity of fuel, in the same units the Task API's `resume(task, fuel)` takes.
pub type Fuel = u64;

/// An error from a [`FuelLedger`] operation. Failed operations leave the ledger unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuelError<A> {
    /// The account is not open in this ledger.
    NoSuchAccount(A),
    /// [`FuelLedger::open`] was called for an account that is already open.
    AccountExists(A),
    /// The account does not hold enough fuel for the requested operation.
    Insufficient {
        /// The account that was asked for more fuel than it holds.
        account: A,
        /// How much fuel the operation asked for.
        requested: Fuel,
        /// How much fuel the account actually holds.
        available: Fuel,
    },
    /// Crediting the account would overflow its balance.
    BalanceOverflow(A),
    /// [`FuelLedger::close`] was called on an account that still holds fuel.
    NonEmptyClose {
        /// The account that was asked to close.
        account: A,
        /// The fuel it still holds.
        balance: Fuel,
    },
}

impl<A: fmt::Debug> fmt::Display for FuelError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchAccount(account) => write!(f, "fuel account {account:?} is not open"),
            Self::AccountExists(account) => write!(f, "fuel account {account:?} is already open"),
            Self::Insufficient {
                account,
                requested,
                available,
            } => write!(
                f,
                "fuel account {account:?} holds {available} fuel, {requested} requested"
            ),
            Self::BalanceOverflow(account) => {
                write!(f, "fuel account {account:?} balance would overflow")
            }
            Self::NonEmptyClose { account, balance } => {
                write!(f, "fuel account {account:?} still holds {balance} fuel")
            }
        }
    }
}

impl<A: fmt::Debug> core::error::Error for FuelError<A> {}

/// A conserved fuel ledger over a set of accounts.
///
/// The account key `A` is whatever the embedder uses to name fuel holders — the
/// [`Scheduler`](crate::Scheduler) uses its task ids plus its own pool; a nested user-level
/// scheduler can use the same type for its own books. Lifetime totals are `u128` so they
/// cannot overflow in practice; individual balances are [`Fuel`] (`u64`), matching the fuel
/// argument of `resume`.
///
/// The only way fuel enters a ledger is [`import`](Self::import) and the only ways it leaves
/// are [`burn`](Self::burn) and [`export`](Self::export); everything else is a conserved
/// transfer. [`is_conserved`](Self::is_conserved) states the law, and every mutating operation
/// re-checks it with a debug assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuelLedger<A: Ord + Copy> {
    balances: BTreeMap<A, Fuel>,
    imported: u128,
    burned: u128,
    exported: u128,
}

impl<A: Ord + Copy> FuelLedger<A> {
    /// An empty ledger: no accounts, nothing imported, burned, or exported.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            balances: BTreeMap::new(),
            imported: 0,
            burned: 0,
            exported: 0,
        }
    }

    /// Opens `account` with a zero balance.
    pub fn open(&mut self, account: A) -> Result<(), FuelError<A>> {
        if self.balances.contains_key(&account) {
            return Err(FuelError::AccountExists(account));
        }
        self.balances.insert(account, 0);
        debug_assert!(self.is_conserved());
        Ok(())
    }

    /// Closes `account`.
    ///
    /// The account must hold no fuel — transfer, burn, or export its balance first — so that
    /// closing an account can never silently destroy fuel.
    pub fn close(&mut self, account: A) -> Result<(), FuelError<A>> {
        match self.balances.get(&account) {
            None => Err(FuelError::NoSuchAccount(account)),
            Some(&balance) if balance != 0 => Err(FuelError::NonEmptyClose { account, balance }),
            Some(_) => {
                self.balances.remove(&account);
                debug_assert!(self.is_conserved());
                Ok(())
            }
        }
    }

    /// Credits `amount` fuel to `account` from **outside** this ledger.
    ///
    /// This is the only way fuel enters the ledger. The caller asserts that it really received
    /// this fuel (its parent donated it; at the root, the platform's timer quantum) — the
    /// conservation law is only as honest as this call.
    pub fn import(&mut self, account: A, amount: Fuel) -> Result<(), FuelError<A>> {
        let new = self
            .get(account)?
            .checked_add(amount)
            .ok_or(FuelError::BalanceOverflow(account))?;
        self.set(account, new);
        self.imported += u128::from(amount);
        debug_assert!(self.is_conserved());
        Ok(())
    }

    /// Debits `amount` fuel from `account` and hands it back out of this ledger, to whoever
    /// this node answers to. The counterpart of [`import`](Self::import).
    pub fn export(&mut self, account: A, amount: Fuel) -> Result<(), FuelError<A>> {
        let new = self.debited(account, amount)?;
        self.set(account, new);
        self.exported += u128::from(amount);
        debug_assert!(self.is_conserved());
        Ok(())
    }

    /// Moves `amount` fuel from `from` to `to`: a donation.
    ///
    /// Conserved by construction — the transfer fails (and changes nothing) unless `from`
    /// actually holds `amount` fuel.
    pub fn transfer(&mut self, from: A, to: A, amount: Fuel) -> Result<(), FuelError<A>> {
        if from == to {
            // A self-transfer moves nothing, but still has to be a transfer the account could
            // make: the account must exist and hold the amount.
            self.debited(from, amount)?;
            return Ok(());
        }
        let from_new = self.debited(from, amount)?;
        let to_new = self
            .get(to)?
            .checked_add(amount)
            .ok_or(FuelError::BalanceOverflow(to))?;
        self.set(from, from_new);
        self.set(to, to_new);
        debug_assert!(self.is_conserved());
        Ok(())
    }

    /// Consumes `amount` fuel from `account`: it was spent running code and is gone.
    pub fn burn(&mut self, account: A, amount: Fuel) -> Result<(), FuelError<A>> {
        let new = self.debited(account, amount)?;
        self.set(account, new);
        self.burned += u128::from(amount);
        debug_assert!(self.is_conserved());
        Ok(())
    }

    /// The balance of `account`, or `None` if it is not open.
    #[must_use]
    pub fn balance(&self, account: A) -> Option<Fuel> {
        self.balances.get(&account).copied()
    }

    /// Whether `account` is open.
    #[must_use]
    pub fn is_open(&self, account: A) -> bool {
        self.balances.contains_key(&account)
    }

    /// The number of open accounts.
    #[must_use]
    pub fn accounts(&self) -> usize {
        self.balances.len()
    }

    /// Total fuel currently held across all accounts.
    #[must_use]
    pub fn circulating(&self) -> u128 {
        self.balances.values().map(|&b| u128::from(b)).sum()
    }

    /// Lifetime fuel imported into this ledger.
    #[must_use]
    pub fn imported(&self) -> u128 {
        self.imported
    }

    /// Lifetime fuel burned (consumed by execution).
    #[must_use]
    pub fn burned(&self) -> u128 {
        self.burned
    }

    /// Lifetime fuel exported back out of this ledger.
    #[must_use]
    pub fn exported(&self) -> u128 {
        self.exported
    }

    /// The conservation law: everything imported was burned, exported, or is still held.
    #[must_use]
    pub fn is_conserved(&self) -> bool {
        self.imported == self.burned + self.exported + self.circulating()
    }

    /// The balance of `account`, or an error if it is not open.
    fn get(&self, account: A) -> Result<Fuel, FuelError<A>> {
        self.balances
            .get(&account)
            .copied()
            .ok_or(FuelError::NoSuchAccount(account))
    }

    /// What `account`'s balance would be after debiting `amount`, or an error if it is not
    /// open or does not hold that much.
    fn debited(&self, account: A, amount: Fuel) -> Result<Fuel, FuelError<A>> {
        let available = self.get(account)?;
        available
            .checked_sub(amount)
            .ok_or(FuelError::Insufficient {
                account,
                requested: amount,
                available,
            })
    }

    /// Overwrites the balance of an account known to exist.
    fn set(&mut self, account: A, value: Fuel) {
        *self
            .balances
            .get_mut(&account)
            .expect("account existence was checked before mutation") = value;
    }
}

impl<A: Ord + Copy> Default for FuelLedger<A> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Fuel, FuelError, FuelLedger};

    #[test]
    fn import_transfer_burn_export_round_trip() {
        let mut ledger = FuelLedger::new();
        ledger.open("pool").unwrap();
        ledger.open("task").unwrap();

        ledger.import("pool", 100).unwrap();
        ledger.transfer("pool", "task", 60).unwrap();
        ledger.burn("task", 45).unwrap();
        ledger.transfer("task", "pool", 15).unwrap();
        ledger.export("pool", 5).unwrap();

        assert_eq!(ledger.balance("pool"), Some(50));
        assert_eq!(ledger.balance("task"), Some(0));
        assert_eq!(ledger.imported(), 100);
        assert_eq!(ledger.burned(), 45);
        assert_eq!(ledger.exported(), 5);
        assert!(ledger.is_conserved());

        ledger.close("task").unwrap();
        assert!(!ledger.is_open("task"));
        assert!(ledger.is_conserved());
    }

    #[test]
    fn failed_operations_change_nothing() {
        let mut ledger = FuelLedger::new();
        ledger.open(1u8).unwrap();
        ledger.open(2u8).unwrap();
        ledger.import(1, 10).unwrap();
        let snapshot = ledger.clone();

        assert_eq!(ledger.open(1), Err(FuelError::AccountExists(1)));
        assert_eq!(ledger.import(3, 5), Err(FuelError::NoSuchAccount(3)));
        assert_eq!(
            ledger.transfer(1, 2, 11),
            Err(FuelError::Insufficient {
                account: 1,
                requested: 11,
                available: 10
            })
        );
        assert_eq!(
            ledger.burn(2, 1),
            Err(FuelError::Insufficient {
                account: 2,
                requested: 1,
                available: 0
            })
        );
        assert_eq!(
            ledger.close(1),
            Err(FuelError::NonEmptyClose {
                account: 1,
                balance: 10
            })
        );

        assert_eq!(ledger, snapshot);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn balance_overflow_is_rejected() {
        let mut ledger = FuelLedger::new();
        ledger.open('a').unwrap();
        ledger.open('b').unwrap();
        ledger.import('a', Fuel::MAX).unwrap();
        ledger.import('b', 1).unwrap();

        assert_eq!(ledger.import('a', 1), Err(FuelError::BalanceOverflow('a')));
        assert_eq!(
            ledger.transfer('b', 'a', 1),
            Err(FuelError::BalanceOverflow('a'))
        );
        assert!(ledger.is_conserved());
    }

    #[test]
    fn self_transfer_requires_the_balance_but_moves_nothing() {
        let mut ledger = FuelLedger::new();
        ledger.open(0u32).unwrap();
        ledger.import(0, 7).unwrap();

        ledger.transfer(0, 0, 7).unwrap();
        assert_eq!(ledger.balance(0), Some(7));
        assert_eq!(
            ledger.transfer(0, 0, 8),
            Err(FuelError::Insufficient {
                account: 0,
                requested: 8,
                available: 7
            })
        );
        assert!(ledger.is_conserved());
    }
}
