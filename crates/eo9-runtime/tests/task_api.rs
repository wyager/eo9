//! Task API surface tests: spawn / resume / runnable / kill, fuel slicing, memory limits,
//! blocking on provider futures, and argument checking (plan/04-runtime.md milestone 2).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use eo9_runtime::providers::{BoxOp, Datetime, TimeProvider};
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    EngineOptions, Image, NamedArg, Outcome, Providers, ResumeOutcome, SpawnError, SpawnLimits,
    Task, new_engine,
};

// ---------------------------------------------------------------------------------------
// Guest components (WAT)
// ---------------------------------------------------------------------------------------

/// Pure-compute guest: sums 0..10_000 (several fuel quanta) and returns the total.
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
const COMPUTE_EXPECTED: &str = "49995000";

/// Guest with a typed argument: `main: func(n: u32) -> u32` returning `n + 1`.
const ARG_WAT: &str = r#"
(component
  (core module $m
    (func (export "main") (param i32) (result i32)
      (i32.add (local.get 0) (i32.const 1))))
  (core instance $i (instantiate $m))
  (func (export "main") (param "n" u32) (result u32) (canon lift (core func $i "main")))
)
"#;

/// Guest that tries to grow linear memory by 64 pages (4 MiB) and returns `memory.grow`'s
/// result: the previous page count on success, 0xffff_ffff on failure.
const GROW_WAT: &str = r#"
(component
  (core module $m
    (memory 1)
    (func (export "main") (result i32)
      (memory.grow (i32.const 64))))
  (core instance $i (instantiate $m))
  (func (export "main") (result u32) (canon lift (core func $i "main")))
)
"#;

/// Guest that imports `eo9:time/time`, calls the async `sleep(1ms)` operation, and returns
/// 7. `sleep` is sync-lowered from an async-lifted `main`: the call parks the task until
/// the host's concurrent implementation completes (sync-lifted exports cannot block in
/// wasmtime 45, hence the async lift).
const SLEEP_WAT: &str = r#"
(component
  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))
  (import "eo9:time/time@0.1.0" (instance $time
    (export "time-impl" (type $ti (eq $time-impl)))
    (export "default" (func (result (own $ti))))
    (export "sleep" (func async (param "t" (borrow $ti)) (param "duration-ns" u64)))))

  (alias export $time "default" (func $default))
  (alias export $time "sleep" (func $sleep))

  (core func $default-lowered (canon lower (func $default)))
  (core func $sleep-lowered (canon lower (func $sleep)))
  (core func $task-return (canon task.return (result u32)))

  (core module $m
    (import "host" "default" (func $default (result i32)))
    (import "host" "sleep" (func $sleep (param i32 i64)))
    (import "host" "task-return" (func $task-return (param i32)))

    (func (export "main")
      ;; The sync-lowered call to the async `sleep` operation blocks this (async-lifted)
      ;; task until the host completes it.
      (call $sleep (call $default) (i64.const 1000000))
      (call $task-return (i32.const 7))))

  (core instance $i (instantiate $m
    (with "host" (instance
      (export "default" (func $default-lowered))
      (export "sleep" (func $sleep-lowered))
      (export "task-return" (func $task-return))))))

  (func (export "main") async (result u32) (canon lift (core func $i "main") async))
)
"#;

fn compile(wat: &str) -> Image {
    let engine = new_engine(&EngineOptions::default()).unwrap();
    Image::compile(&engine, wat).unwrap()
}

fn success_value(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Success(value) => &value.value,
        other => panic!("expected success, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// Fuel slicing
// ---------------------------------------------------------------------------------------

#[test]
fn compute_guest_is_fuel_sliced_and_resumable() {
    let image = compile(COMPUTE_WAT);
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();

    let mut out_of_fuel = 0u32;
    let outcome = loop {
        match task.resume(FUEL_QUANTUM) {
            ResumeOutcome::OutOfFuel => {
                out_of_fuel += 1;
                assert!(out_of_fuel < 1_000, "compute guest never finished");
            }
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::Blocked => panic!("compute guest cannot block"),
        }
    };

    assert_eq!(success_value(&outcome), COMPUTE_EXPECTED);
    assert!(
        out_of_fuel > 1,
        "expected the guest to be suspended by fuel more than once, got {out_of_fuel}"
    );
    // Resuming a finished task just reports the outcome again.
    assert_eq!(task.resume(FUEL_QUANTUM), ResumeOutcome::Done(outcome));
    assert!(!task.is_runnable());
}

#[test]
fn sub_quantum_donations_accumulate() {
    let image = compile(COMPUTE_WAT);
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();

    // Donations smaller than a quantum do not run the guest but are carried forward.
    assert_eq!(task.resume(FUEL_QUANTUM / 4), ResumeOutcome::OutOfFuel);
    assert_eq!(task.unspent_fuel(), FUEL_QUANTUM / 4);
    assert_eq!(task.resume(FUEL_QUANTUM / 4), ResumeOutcome::OutOfFuel);
    assert_eq!(task.unspent_fuel(), FUEL_QUANTUM / 2);

    // Once whole quanta are available they are spent on guest execution.
    let mut donated = task.unspent_fuel();
    loop {
        match task.resume(FUEL_QUANTUM) {
            ResumeOutcome::OutOfFuel => {
                donated += FUEL_QUANTUM;
                assert!(donated < 100 * FUEL_QUANTUM, "guest never finished");
            }
            ResumeOutcome::Done(outcome) => {
                assert_eq!(success_value(&outcome), COMPUTE_EXPECTED);
                break;
            }
            ResumeOutcome::Blocked => panic!("compute guest cannot block"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Arguments and outcomes
// ---------------------------------------------------------------------------------------

#[test]
fn wave_arguments_are_parsed_and_type_checked() {
    let image = compile(ARG_WAT);

    // Happy path: `--n 41` -> 42.
    let mut task = Task::spawn(
        &image,
        &[NamedArg::new("n", "41")],
        SpawnLimits::default(),
        Providers::none(),
    )
    .unwrap();
    let outcome = match task.resume(10 * FUEL_QUANTUM) {
        ResumeOutcome::Done(outcome) => outcome,
        other => panic!("expected done, got {other:?}"),
    };
    assert_eq!(success_value(&outcome), "42");

    // Missing argument.
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap_err();
    assert!(matches!(err, SpawnError::BadArguments(_)), "{err}");

    // Unknown argument.
    let err = Task::spawn(
        &image,
        &[NamedArg::new("n", "1"), NamedArg::new("bogus", "2")],
        SpawnLimits::default(),
        Providers::none(),
    )
    .unwrap_err();
    assert!(matches!(err, SpawnError::BadArguments(_)), "{err}");

    // Ill-typed argument.
    let err = Task::spawn(
        &image,
        &[NamedArg::new("n", "\"not a number\"")],
        SpawnLimits::default(),
        Providers::none(),
    )
    .unwrap_err();
    assert!(matches!(err, SpawnError::BadArguments(_)), "{err}");
}

// ---------------------------------------------------------------------------------------
// Memory limits
// ---------------------------------------------------------------------------------------

#[test]
fn memory_ceiling_is_enforced_at_grow() {
    let image = compile(GROW_WAT);

    // Without a limit the 4 MiB growth succeeds (previous size was 1 page).
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    let outcome = match task.resume(10 * FUEL_QUANTUM) {
        ResumeOutcome::Done(outcome) => outcome,
        other => panic!("expected done, got {other:?}"),
    };
    assert_eq!(success_value(&outcome), "1");

    // With a 2 MiB ceiling the growth is refused (memory.grow returns -1); the guest keeps
    // running and reports the refusal rather than trapping.
    let mut task = Task::spawn(
        &image,
        &[],
        SpawnLimits {
            max_memory: Some(2 * 1024 * 1024),
            ..SpawnLimits::default()
        },
        Providers::none(),
    )
    .unwrap();
    let outcome = match task.resume(10 * FUEL_QUANTUM) {
        ResumeOutcome::Done(outcome) => outcome,
        other => panic!("expected done, got {other:?}"),
    };
    assert_eq!(success_value(&outcome), "4294967295");
}

// ---------------------------------------------------------------------------------------
// Kill
// ---------------------------------------------------------------------------------------

#[test]
fn kill_before_completion_reports_killed() {
    let image = compile(COMPUTE_WAT);
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    assert_eq!(task.resume(FUEL_QUANTUM), ResumeOutcome::OutOfFuel);
    assert_eq!(task.kill(), Outcome::Killed);
}

#[test]
fn kill_after_completion_reports_the_outcome() {
    let image = compile(COMPUTE_WAT);
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    loop {
        if let ResumeOutcome::Done(_) = task.resume(10 * FUEL_QUANTUM) {
            break;
        }
    }
    match task.kill() {
        Outcome::Success(value) => assert_eq!(value.value, COMPUTE_EXPECTED),
        other => panic!("expected success, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// Blocking on a provider future completed from another thread
// ---------------------------------------------------------------------------------------

/// A time provider whose `sleep` resolves only when the test completes it (from another
/// thread), so the guest genuinely parks on the host future.
#[derive(Clone, Default)]
struct ManualTime {
    sleepers: Arc<Mutex<Vec<SleepCell>>>,
}

#[derive(Clone, Default)]
struct SleepCell {
    inner: Arc<Mutex<(bool, Option<Waker>)>>,
}

impl SleepCell {
    fn complete(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.0 = true;
        if let Some(waker) = inner.1.take() {
            waker.wake();
        }
    }
}

struct SleepFuture(SleepCell);

impl Future for SleepFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut inner = self.0.inner.lock().unwrap();
        if inner.0 {
            Poll::Ready(())
        } else {
            inner.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl TimeProvider for ManualTime {
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
        let cell = SleepCell::default();
        self.sleepers.lock().unwrap().push(cell.clone());
        Box::pin(SleepFuture(cell))
    }
}

#[test]
fn guest_blocks_on_sleep_until_the_provider_completes_from_another_thread() {
    let image = compile(SLEEP_WAT);
    let time = ManualTime::default();
    let sleepers = time.sleepers.clone();

    let mut task = Task::spawn(
        &image,
        &[],
        SpawnLimits::default(),
        Providers {
            time: Some(Box::new(time)),
            ..Providers::none()
        },
    )
    .unwrap();

    // The guest runs up to the await on the sleep future and parks.
    assert_eq!(task.resume(100 * FUEL_QUANTUM), ResumeOutcome::Blocked);
    assert!(!task.is_runnable());
    assert_eq!(sleepers.lock().unwrap().len(), 1);

    // Donating more fuel to a blocked task does not run it (the donation is carried).
    let carried_before = task.unspent_fuel();
    assert_eq!(task.resume(FUEL_QUANTUM), ResumeOutcome::Blocked);
    assert_eq!(task.unspent_fuel(), carried_before + FUEL_QUANTUM);

    // Complete the sleep from another thread; the doorbell must make the task runnable.
    let completer = sleepers.lock().unwrap()[0].clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        completer.complete();
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !task.is_runnable() {
        assert!(
            std::time::Instant::now() < deadline,
            "provider completion never rang the doorbell"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    thread.join().unwrap();

    let outcome = match task.resume(100 * FUEL_QUANTUM) {
        ResumeOutcome::Done(outcome) => outcome,
        other => panic!("expected done, got {other:?}"),
    };
    assert_eq!(success_value(&outcome), "7");
}

// ---------------------------------------------------------------------------------------
// Loader rule: unsatisfied imports are spawn errors
// ---------------------------------------------------------------------------------------

#[test]
fn unsatisfied_import_is_a_spawn_error() {
    let image = compile(SLEEP_WAT);
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap_err();
    match err {
        SpawnError::Internal(message) => {
            assert!(
                message.contains("eo9:time"),
                "unexpected message: {message}"
            )
        }
        other => panic!("expected an internal spawn error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// Loader rule: optional imports are auto-sealed with the absent provider
// ---------------------------------------------------------------------------------------

/// Guest that imports only the *optional* text flavor and reports what `default()`
/// answered: 1 when the capability is present, 0 when it is absent.
const OPTIONAL_TEXT_WAT: &str = r#"
(component
  (import "eo9:text/types@0.1.0" (instance $text-types
    (export "text-impl" (type (sub resource)))))
  (alias export $text-types "text-impl" (type $text-impl))
  (import "eo9:text/text-optional@0.1.0" (instance $text-opt
    (export "text-impl" (type $ti (eq $text-impl)))
    (export "default" (func (result (option (own $ti)))))))

  (alias export $text-opt "default" (func $default))

  (core module $libc (memory (export "memory") 1))
  (core instance $libc (instantiate $libc))
  (core func $default-lowered (canon lower (func $default) (memory $libc "memory")))

  (core module $m
    (import "libc" "memory" (memory 1))
    ;; option<own<text-impl>> does not fit in one flat result: the lowered import takes a
    ;; return pointer and writes (discriminant, handle) there.
    (import "host" "default" (func $default (param i32)))
    (func (export "main") (result i32)
      (call $default (i32.const 16))
      (i32.load (i32.const 16))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance (export "default" (func $default-lowered))))))

  (func (export "main") (result u32) (canon lift (core func $i "main")))
)
"#;

#[test]
fn optional_import_is_auto_sealed_when_not_granted() {
    use eo9_runtime::providers::CaptureText;

    let image = compile(OPTIONAL_TEXT_WAT);

    // Not granted: the spawn still succeeds (the loader auto-seals the optional import
    // with the absent provider) and the program observes `none`.
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    let outcome = match task.resume(10 * FUEL_QUANTUM) {
        ResumeOutcome::Done(outcome) => outcome,
        other => panic!("expected done, got {other:?}"),
    };
    assert_eq!(success_value(&outcome), "0");

    // Granted: the same program observes `some`.
    let mut task = Task::spawn(
        &image,
        &[],
        SpawnLimits::default(),
        Providers {
            text: Some(Box::new(CaptureText::new())),
            ..Providers::none()
        },
    )
    .unwrap();
    let outcome = match task.resume(10 * FUEL_QUANTUM) {
        ResumeOutcome::Done(outcome) => outcome,
        other => panic!("expected done, got {other:?}"),
    };
    assert_eq!(success_value(&outcome), "1");
}

#[test]
fn kill_in_place_then_outcome_reports_killed_without_trapping() {
    let image = compile(COMPUTE_WAT);
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    // Partially run it, then kill it in place (the exec provider's guest-facing `kill`).
    assert_eq!(task.resume(FUEL_QUANTUM), ResumeOutcome::OutOfFuel);
    assert_eq!(task.kill_in_place(), Outcome::Killed);
    // The same handle keeps answering: `outcome`/`wait`-style observation and even another
    // resume see the killed outcome (this is what `wait` after `kill` maps to
    // `abnormal(killed)` through eo9:exec/task).
    assert_eq!(task.outcome(), Some(&Outcome::Killed));
    assert_eq!(
        task.resume(FUEL_QUANTUM),
        ResumeOutcome::Done(Outcome::Killed)
    );
    assert!(!task.is_runnable());
}
