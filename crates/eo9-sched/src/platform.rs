//! What the scheduler's embedder needs from the machine it runs on.

/// The platform behind the scheduler: the little that differs between usermode and bare metal.
///
/// The embedder drives the scheduler from an ordinary loop; the only moment it needs the
/// machine is when there is nothing to run — every live task is blocked — and it must wait for
/// an external event (an I/O completion, a timer tick) to make one runnable again. Usermode
/// implements [`idle`](Platform::idle) by parking the scheduler thread until a completion
/// arrives; the kernel implements it with a `wfi`/`hlt`-style wait-for-interrupt.
///
/// # SMP hooks (documented, not yet part of the trait)
///
/// Multi-core support will add a cross-core wake — `wake(core: CoreId)`, an IPI on metal and a
/// thread unpark in usermode — so a completion arriving on one core can wake the core whose
/// run queue owns the newly runnable task. It is deliberately not in the trait yet: the
/// scheduler is single-core for now (see the crate docs), and an unused hook would only invite
/// guesses about semantics that the SMP design note (plan 05, milestone 3) has to pin down
/// first — per-core run queues, and the multi-core reading of the single-resumer rule.
pub trait Platform {
    /// Waits until an external event may have made a task runnable again.
    ///
    /// Called by the embedder when the scheduler [is idle](crate::Scheduler::is_idle): no task
    /// is runnable and none is being resumed. Spurious returns are fine — the embedder drains
    /// its completion queues and simply idles again if nothing became runnable.
    fn idle(&mut self);

    /// Monotonic time in nanoseconds from an arbitrary epoch, if this platform can tell time.
    ///
    /// Optional: a deterministic test platform has no clock and returns `None` (the default),
    /// and nothing in this crate's bookkeeping depends on time. Policies or embedders that
    /// want time-based accounting may use it where it exists.
    fn now(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::Platform;

    /// A platform for tests: counts idles, has no clock.
    struct CountingPlatform {
        idles: usize,
    }

    impl Platform for CountingPlatform {
        fn idle(&mut self) {
            self.idles += 1;
        }
    }

    #[test]
    fn now_defaults_to_none() {
        let mut platform = CountingPlatform { idles: 0 };
        platform.idle();
        platform.idle();
        assert_eq!(platform.idles, 2);
        assert_eq!(platform.now(), None);
    }
}
