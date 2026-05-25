//! The MVP blocking backend: a small pool of worker threads that run blocking host
//! syscalls and invoke the operation's [`Completer`](crate::completion::Completer).
//!
//! The pool is an implementation detail of the providers that use it (fs, disk): the
//! caller-visible contract is only "submit an op with a completer, receive exactly one
//! completion". An io_uring-style submission backend can replace the pool inside a
//! provider without changing any caller.
//!
//! Shutdown semantics: dropping the pool stops accepting new work, lets the workers
//! drain everything already submitted, and then joins them — so every accepted job runs
//! to completion and every completer fires, even across provider shutdown.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed-size pool of blocking worker threads.
pub struct BlockingPool {
    sender: Option<Sender<Job>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl BlockingPool {
    /// A pool with `threads` workers.
    ///
    /// # Panics
    ///
    /// Panics if `threads` is zero.
    pub fn new(threads: usize) -> Self {
        assert!(threads > 0, "a blocking pool needs at least one worker");
        let (sender, receiver) = mpsc::channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = (0..threads)
            .map(|index| {
                let receiver = Arc::clone(&receiver);
                thread::Builder::new()
                    .name(format!("eo9-blocking-{index}"))
                    .spawn(move || worker_loop(&receiver))
                    .expect("failed to spawn blocking pool worker")
            })
            .collect();
        Self {
            sender: Some(sender),
            workers,
        }
    }

    /// A pool sized for the host: `available_parallelism` clamped to 2..=8 workers.
    ///
    /// The clamp is deliberate for the MVP: the pool bounds how many blocking syscalls
    /// are in flight at once, and the io_uring-style backend is the real answer to
    /// very high concurrency, not more threads.
    pub fn with_default_size() -> Self {
        let threads = thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(4)
            .clamp(2, 8);
        Self::new(threads)
    }

    /// Number of worker threads.
    pub fn threads(&self) -> usize {
        self.workers.len()
    }

    /// Submits a blocking job. The job runs on some worker thread, in submission order
    /// per worker pick-up (no fairness guarantee beyond FIFO dispatch).
    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        self.sender
            .as_ref()
            .expect("blocking pool sender lives until drop")
            .send(Box::new(job))
            .expect("blocking pool workers live until drop");
    }
}

impl Default for BlockingPool {
    fn default() -> Self {
        Self::with_default_size()
    }
}

impl Drop for BlockingPool {
    fn drop(&mut self) {
        // Closing the channel lets each worker drain the remaining jobs and exit.
        drop(self.sender.take());
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<Job>>) {
    loop {
        // Hold the lock only while waiting for a job, never while running one.
        let job = {
            let guard = receiver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.recv()
        };
        match job {
            Ok(job) => {
                // A panicking job must not take the worker down with it: contain it, keep
                // serving. Providers map all expected failures into error values, so this
                // only catches outright bugs.
                if catch_unwind(AssertUnwindSafe(job)).is_err() {
                    eprintln!(
                        "eo9-providers-unix: a blocking job panicked (bug); worker continues"
                    );
                }
            }
            // Channel closed and drained: the pool is shutting down.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn jobs_run_and_complete() {
        let pool = BlockingPool::new(2);
        let (tx, rx) = mpsc::channel();
        for i in 0..32u32 {
            let tx = tx.clone();
            pool.submit(move || tx.send(i).unwrap());
        }
        drop(tx);
        let mut seen: Vec<u32> = rx.iter().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..32).collect::<Vec<_>>());
    }

    #[test]
    fn drop_drains_all_accepted_jobs() {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = BlockingPool::new(1);
        for _ in 0..16 {
            let counter = Arc::clone(&counter);
            pool.submit(move || {
                thread::sleep(Duration::from_millis(1));
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }
        drop(pool);
        assert_eq!(counter.load(Ordering::SeqCst), 16);
    }

    #[test]
    fn a_panicking_job_does_not_kill_the_worker() {
        let pool = BlockingPool::new(1);
        let (tx, rx) = mpsc::channel();
        pool.submit(|| panic!("deliberate test panic"));
        pool.submit(move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(10))
            .expect("worker should survive a panicking job");
    }

    #[test]
    fn default_size_is_sane() {
        let pool = BlockingPool::with_default_size();
        assert!((2..=8).contains(&pool.threads()));
    }
}
