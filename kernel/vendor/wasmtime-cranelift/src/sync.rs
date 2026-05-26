//! A minimal `no_std` stand-in for `std::sync::Mutex`.
//!
//! plan/12 Decision 29: the Cranelift compiler glue keeps a small pool of reusable
//! `CompilerContext`s behind a `Mutex` so the `Compiler` can stay `Send + Sync`.
//! On the bare-metal kernel target there is no `std::sync::Mutex`, and the kernel
//! compiles on a single core, so this provides the same call-site shape
//! (`new`/`lock()` returning a `Result`, a nameable [`MutexGuard`], `Default`)
//! as a non-blocking spinlock. It mirrors the equally-minimal `Mutex` the vendored
//! `wasmtime` crate carries in `sync_nostd.rs`: contention panics rather than
//! blocking, which keeps it correct (never two guards at once) while remaining
//! usable without host locking support.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Returned by [`Mutex::try_lock`] when the lock is already held.
#[derive(Debug)]
pub struct WouldBlock;

/// A non-blocking `no_std` `Mutex`.
#[derive(Debug, Default)]
pub struct Mutex<T> {
    locked: AtomicBool,
    val: UnsafeCell<T>,
}

// SAFETY: access to `val` is guarded by `locked`, and a guard is handed out only
// after the flag is won, so there is never aliasing mutable access.
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(val: T) -> Mutex<T> {
        Mutex {
            locked: AtomicBool::new(false),
            val: UnsafeCell::new(val),
        }
    }

    pub fn lock(&self) -> Result<MutexGuard<'_, T>, WouldBlock> {
        match self.try_lock() {
            Ok(guard) => Ok(guard),
            Err(_) => panic!("concurrent lock request on a single-core no_std build"),
        }
    }

    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, WouldBlock> {
        if self.locked.swap(true, Ordering::Acquire) {
            Err(WouldBlock)
        } else {
            Ok(MutexGuard { lock: self })
        }
    }
}

pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: constructing a guard requires having taken the lock.
        unsafe { &*self.lock.val.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: constructing a guard requires having taken the lock.
        unsafe { &mut *self.lock.val.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
