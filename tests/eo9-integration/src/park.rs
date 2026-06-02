//! A controllable parking clock for the async-hardening suites.
//!
//! [`ParkBed`] is a [`TimeProvider`] backend whose `sleep` operations never resolve on
//! their own: each one registers as a numbered cell, and the test decides when (and
//! whether) to complete it. The bed also observes lifecycle — how many operations were
//! started, which were dropped — so kill/cancel suites can assert that in-flight
//! operations are released, not leaked (SPEC "Kill and linearity").

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use eo9_runtime::providers::{BoxOp, Datetime, TimeProvider};

#[derive(Default)]
struct Cell {
    completed: bool,
    dropped: bool,
    waker: Option<Waker>,
}

/// The shared observation/control surface (see the module docs).
#[derive(Default)]
pub struct ParkBed {
    cells: Mutex<Vec<Cell>>,
}

impl ParkBed {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A [`TimeProvider`] backed by this bed.
    pub fn clock(self: &Arc<Self>) -> Box<dyn TimeProvider> {
        Box::new(ParkClock { bed: self.clone() })
    }

    /// How many sleep operations have been started so far.
    pub fn started(&self) -> usize {
        self.cells.lock().unwrap().len()
    }

    /// How many sleep operations have been dropped (released by kill, cancel, or
    /// completion-and-return).
    pub fn dropped(&self) -> usize {
        self.cells
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.dropped)
            .count()
    }

    /// Whether the `idx`-th sleep (in start order) is currently parked: started, not
    /// completed, not dropped.
    pub fn parked(&self, idx: usize) -> bool {
        let cells = self.cells.lock().unwrap();
        cells
            .get(idx)
            .map(|c| !c.completed && !c.dropped)
            .unwrap_or(false)
    }

    /// Complete the `idx`-th sleep (in start order) and ring its waker.
    pub fn complete(&self, idx: usize) {
        let waker = {
            let mut cells = self.cells.lock().unwrap();
            let cell = cells.get_mut(idx).expect("no such sleep operation");
            cell.completed = true;
            cell.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct ParkClock {
    bed: Arc<ParkBed>,
}

impl TimeProvider for ParkClock {
    fn now(&mut self) -> Datetime {
        Datetime {
            seconds: 0,
            nanoseconds: 0,
        }
    }

    fn monotonic_now(&mut self) -> u64 {
        0
    }

    fn resolution(&mut self) -> u64 {
        1
    }

    fn sleep(&mut self, _duration_ns: u64) -> BoxOp<()> {
        let idx = {
            let mut cells = self.bed.cells.lock().unwrap();
            cells.push(Cell::default());
            cells.len() - 1
        };
        Box::pin(ParkOp {
            bed: self.bed.clone(),
            idx,
        })
    }
}

struct ParkOp {
    bed: Arc<ParkBed>,
    idx: usize,
}

impl Future for ParkOp {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut cells = self.bed.cells.lock().unwrap();
        let cell = &mut cells[self.idx];
        if cell.completed {
            Poll::Ready(())
        } else {
            cell.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for ParkOp {
    fn drop(&mut self) {
        self.bed.cells.lock().unwrap()[self.idx].dropped = true;
    }
}
