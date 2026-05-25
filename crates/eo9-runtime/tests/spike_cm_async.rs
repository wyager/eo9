//! Component-model-async + fuel spike against wasmtime 45 (plan/04-runtime.md, milestone 1).
//!
//! These tests talk to wasmtime directly (using only this crate's pinned engine config) and
//! answer the two spike questions:
//!
//! 1. Can a guest component await a WIT-level `future<T>` returned by a host function that
//!    is completed later, from another thread? (`guest_awaits_host_future_completed_later`)
//! 2. Does a fuel-bounded, resumable "run until out-of-fuel | blocked | done" drive work —
//!    in particular, does guest execution state survive dropping the `run_concurrent`
//!    future between fuel donations? (`fuel_bounded_drives_are_resumable`)
//!
//! The findings (what wasmtime 45 can and cannot do) are recorded in
//! plan/04-runtime.md § Decisions; the guest-facing strategy used here (sync-lifted export,
//! sync-lowered imports, `future.read async` + `waitable-set.wait`) is the same one the
//! guest SDK (area 07) needs to implement.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use eo9_runtime::{EngineOptions, new_engine};
use wasmtime::component::{Component, FutureReader, Linker, Val};
use wasmtime::{Store, StoreContextMut};

// ---------------------------------------------------------------------------------------
// Test plumbing: a doorbell waker and a oneshot value completed from another thread.
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Doorbell(AtomicBool);

impl Doorbell {
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

impl Wake for Doorbell {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A `Future<Output = Result<u32>>` completed by calling `Oneshot::complete`, possibly from
/// another thread. Used as the producer for the host-created Component Model future.
#[derive(Default, Clone)]
struct Oneshot {
    inner: Arc<Mutex<OneshotInner>>,
}

#[derive(Default)]
struct OneshotInner {
    value: Option<u32>,
    waker: Option<Waker>,
    polled_pending: bool,
}

impl Oneshot {
    fn complete(&self, value: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.value = Some(value);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }

    /// Whether the guest-side read got far enough to poll this future and find it pending.
    fn was_polled_pending(&self) -> bool {
        self.inner.lock().unwrap().polled_pending
    }
}

impl Future for Oneshot {
    type Output = wasmtime::Result<u32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.lock().unwrap();
        match inner.value {
            Some(value) => Poll::Ready(Ok(value)),
            None => {
                inner.polled_pending = true;
                inner.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Spike (a): guest awaits a host future completed later from another thread
// ---------------------------------------------------------------------------------------

/// Guest strategy: `main` lifted with the **async (stackful)** ABI — wasmtime 45 refuses
/// any potentially-blocking canonical built-in (`waitable-set.wait`, blocking reads) inside
/// a sync-lifted export with `Trap::CannotBlockSyncTask` — plus a sync-lowered import
/// returning the future handle, `future.read async`, and `waitable-set.wait` to park until
/// the host completes the future. The result is delivered with `task.return`.
const AWAIT_HOST_FUTURE_WAT: &str = r#"
(component
  (type $ft (future u32))
  (import "get-value" (func $get (result $ft)))

  (core module $libc (memory (export "memory") 1))
  (core instance $libc (instantiate $libc))

  (core func $get-lowered (canon lower (func $get)))
  (core func $future-read (canon future.read $ft async (memory $libc "memory")))
  (core func $ws-new (canon waitable-set.new))
  (core func $ws-join (canon waitable.join))
  (core func $ws-wait (canon waitable-set.wait (memory $libc "memory")))
  (core func $future-drop (canon future.drop-readable $ft))
  (core func $ws-drop (canon waitable-set.drop))
  (core func $task-return (canon task.return (result u32)))

  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "get" (func $get (result i32)))
    (import "host" "future-read" (func $future-read (param i32 i32) (result i32)))
    (import "host" "waitable-set-new" (func $ws-new (result i32)))
    (import "host" "waitable-join" (func $ws-join (param i32 i32)))
    (import "host" "waitable-set-wait" (func $ws-wait (param i32 i32) (result i32)))
    (import "host" "future-drop" (func $future-drop (param i32)))
    (import "host" "waitable-set-drop" (func $ws-drop (param i32)))
    (import "host" "task-return" (func $task-return (param i32)))

    (func (export "main")
      (local $f i32) (local $ws i32) (local $status i32)
      ;; Ask the host for a future<u32>; the sync-lowered call returns its readable end.
      (local.set $f (call $get))
      ;; Start an async read of the future into memory[16].
      (local.set $status (call $future-read (local.get $f) (i32.const 16)))
      ;; BLOCKED (0xffff_ffff) means the value is not there yet: park on a waitable-set.
      (if (i32.eq (local.get $status) (i32.const -1))
        (then
          (local.set $ws (call $ws-new))
          (call $ws-join (local.get $f) (local.get $ws))
          (drop (call $ws-wait (local.get $ws) (i32.const 32)))
          (call $ws-join (local.get $f) (i32.const 0))
          (call $ws-drop (local.get $ws))))
      (call $future-drop (local.get $f))
      ;; The payload was written to memory[16] when the read completed.
      (call $task-return (i32.load (i32.const 16)))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "get" (func $get-lowered))
      (export "future-read" (func $future-read))
      (export "waitable-set-new" (func $ws-new))
      (export "waitable-join" (func $ws-join))
      (export "waitable-set-wait" (func $ws-wait))
      (export "future-drop" (func $future-drop))
      (export "waitable-set-drop" (func $ws-drop))
      (export "task-return" (func $task-return))))))

  ;; `main` is an *async* function (the async-ness is part of the component-level function
  ;; type in this wasm-tools/wasmtime generation) lifted with the stackful async ABI.
  (func (export "main") async (result u32) (canon lift (core func $i "main") async))
)
"#;

#[test]
fn guest_awaits_host_future_completed_later() {
    let engine = new_engine(&EngineOptions::default()).unwrap();
    let component = Component::new(&engine, AWAIT_HOST_FUTURE_WAT).unwrap();

    let oneshot = Oneshot::default();
    let handout = oneshot.clone();

    let mut linker: Linker<()> = Linker::new(&engine);
    linker
        .root()
        .func_wrap(
            "get-value",
            move |mut store: StoreContextMut<'_, ()>,
                  (): ()|
                  -> wasmtime::Result<(FutureReader<u32>,)> {
                let reader = FutureReader::new(&mut store, handout.clone())?;
                Ok((reader,))
            },
        )
        .unwrap();

    let mut store = Store::new(&engine, ());
    store.set_fuel(10_000_000).unwrap();

    let doorbell = Arc::new(Doorbell::default());
    let waker = Waker::from(doorbell.clone());
    let mut cx = Context::from_waker(&waker);

    // Instantiation and the whole call are driven by manually polling a single
    // `run_concurrent` future with a no-op executor — no async runtime anywhere.
    let instance = {
        let mut instantiate = pin!(linker.instantiate_async(&mut store, &component));
        loop {
            match instantiate.as_mut().poll(&mut cx) {
                Poll::Ready(result) => break result.unwrap(),
                Poll::Pending => assert!(doorbell.take(), "instantiation stalled"),
            }
        }
    };
    let main = instance.get_func(&mut store, "main").unwrap();

    let result_cell: Arc<Mutex<Option<wasmtime::Result<Vec<Val>>>>> = Arc::new(Mutex::new(None));
    let cell = result_cell.clone();
    let drive = store.run_concurrent(async move |accessor| {
        let mut results = vec![Val::Bool(false)];
        let call = main.call_concurrent(accessor, &[], &mut results).await;
        *cell.lock().unwrap() = Some(call.map(|()| results));
    });
    let mut drive = pin!(drive);

    // Phase 1: drive until the guest is parked on the host future. That point is reached
    // when a poll returns Pending without having rung the doorbell: nothing in the store
    // can progress until the external completion arrives.
    doorbell.take();
    let mut polls = 0;
    loop {
        match drive.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                panic!("guest finished before the host future resolved: {result:?}")
            }
            Poll::Pending => {
                polls += 1;
                assert!(polls < 10_000, "guest never parked on the host future");
                if !doorbell.take() {
                    break;
                }
            }
        }
    }
    assert!(
        oneshot.was_polled_pending(),
        "the guest's future.read never reached the host producer"
    );
    assert!(result_cell.lock().unwrap().is_none());

    // Phase 2: complete the future from another thread, which must ring the doorbell
    // (the waker captured by the producer) and let the guest finish.
    let completer = oneshot.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        completer.complete(42);
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let results = loop {
        if let Some(result) = result_cell.lock().unwrap().take() {
            break result.unwrap();
        }
        match drive.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                result.unwrap();
                if let Some(result) = result_cell.lock().unwrap().take() {
                    break result.unwrap();
                }
                panic!("drive finished without a main result");
            }
            Poll::Pending => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for completion"
                );
                if !doorbell.take() {
                    // Nothing to do until the other thread completes the future.
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    };
    thread.join().unwrap();

    assert_eq!(results, vec![Val::U32(42)]);
}

// ---------------------------------------------------------------------------------------
// Spike (b): fuel-bounded, resumable drives
// ---------------------------------------------------------------------------------------

/// Pure-compute guest: sums 0..10_000 in a loop (tens of thousands of fuel units) and
/// returns the total. No imports.
const COMPUTE_WAT: &str = r#"
(component
  (core module $m
    (func (export "main") (result i32)
      (local $i i32) (local $sum i32)
      (loop $l
        (local.set $sum (i32.add (local.get $sum) (local.get $i)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br_if $l (i32.lt_u (local.get $i) (i32.const 10000))))
      (local.get $sum)))
  (core instance $i (instantiate $m))
  (func (export "main") (result u32) (canon lift (core func $i "main")))
)
"#;

/// The sum the guest computes: 0 + 1 + … + 9999.
const COMPUTE_EXPECTED: u32 = 49_995_000;

/// Resumable, fuel-quantum-bounded execution.
///
/// Wasmtime 45's fuel yield suspends the *currently executing fiber in place*: the fiber is
/// held by the in-flight `run_concurrent` poll (not parked in the store), so the embedder
/// can neither drop that future and re-drive later (the fiber is disposed on drop, killing
/// the guest) nor touch the store (it is mutably borrowed) to add fuel between donations.
/// The workable shape is therefore:
///
/// * one long-lived drive future that owns the store for the task's whole life,
/// * a fixed fuel-yield quantum configured before the drive starts, and
/// * an embedder-side ledger that counts quanta: each poll that returns `Pending` after a
///   synchronous wake is one quantum of guest execution; stop polling when the donated
///   budget is spent and continue later.
///
/// This test demonstrates exactly that: a pure-compute guest is run in 10k-fuel quanta,
/// stopping after every quantum, and still produces the right answer — i.e. "run until
/// out-of-fuel | done" with real suspension between donations.
#[test]
fn fuel_bounded_drives_are_resumable() {
    const QUANTUM: u64 = 10_000;

    let engine = new_engine(&EngineOptions::default()).unwrap();
    let component = Component::new(&engine, COMPUTE_WAT).unwrap();
    let linker: Linker<()> = Linker::new(&engine);

    let mut store = Store::new(&engine, ());
    // Lifetime fuel pool plus the fixed yield quantum, both configured before the
    // long-lived drive takes ownership of the store.
    store.set_fuel(u64::MAX).unwrap();
    store.fuel_async_yield_interval(Some(QUANTUM)).unwrap();

    let doorbell = Arc::new(Doorbell::default());
    let waker = Waker::from(doorbell.clone());
    let mut cx = Context::from_waker(&waker);

    let instance = {
        let mut instantiate = pin!(linker.instantiate_async(&mut store, &component));
        loop {
            match instantiate.as_mut().poll(&mut cx) {
                Poll::Ready(result) => break result.unwrap(),
                Poll::Pending => assert!(doorbell.take(), "instantiation stalled"),
            }
        }
    };
    let main = instance.get_func(&mut store, "main").unwrap();

    // The long-lived drive: owns the store, runs the event loop, and parks `main`'s result
    // in a shared cell. Nothing borrows the store from outside once this exists.
    let result_cell: Arc<Mutex<Option<wasmtime::Result<Vec<Val>>>>> = Arc::new(Mutex::new(None));
    let cell = result_cell.clone();
    let mut drive = Box::pin(async move {
        let mut store = store;
        store
            .run_concurrent(async move |accessor| {
                let mut results = vec![Val::Bool(false)];
                let call = main.call_concurrent(accessor, &[], &mut results).await;
                *cell.lock().unwrap() = Some(call.map(|()| results));
            })
            .await
    });

    // Scheduler loop: donate one quantum at a time; each synchronously-woken Pending is one
    // quantum of guest execution consumed.
    let mut quanta = 0u32;
    let final_results = loop {
        assert!(quanta < 1_000, "guest never finished");

        doorbell.take();
        match drive.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                result.unwrap();
                break result_cell
                    .lock()
                    .unwrap()
                    .take()
                    .expect("drive finished without a main result")
                    .unwrap();
            }
            Poll::Pending => {
                assert!(
                    doorbell.take(),
                    "compute guest stalled without a fuel yield (it cannot block)"
                );
                // One quantum consumed; between here and the next poll the guest is
                // genuinely suspended — this is the "out of fuel, resume later" point.
                quanta += 1;
            }
        }
    };

    assert_eq!(final_results, vec![Val::U32(COMPUTE_EXPECTED)]);
    // The loop needs several times QUANTUM fuel, so finishing correctly proves execution
    // was suspended and resumed across multiple fuel donations.
    assert!(quanta > 1, "expected multiple fuel quanta, got {quanta}");
}
