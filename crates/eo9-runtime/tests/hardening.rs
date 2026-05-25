//! Resource-exhaustion hardening tests: host-side bounds that must hold regardless of what
//! a (potentially hostile) guest asks for. See plan/04-runtime.md § Decisions (hardening
//! note).

use eo9_runtime::providers::SeededEntropy;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    EngineOptions, Image, NamedArg, Outcome, Providers, ResumeOutcome, SpawnError, SpawnLimits,
    Task, new_engine,
};

fn compile(wat: &str) -> Image {
    let engine = new_engine(&EngineOptions::default()).unwrap();
    Image::compile(&engine, wat).unwrap()
}

fn run_to_done(task: &mut Task) -> Outcome {
    loop {
        match task.resume(100 * FUEL_QUANTUM) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => continue,
            ResumeOutcome::Blocked => panic!("guest unexpectedly blocked"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// 1. entropy get-bytes: guest-supplied length is capped before any host allocation
// ---------------------------------------------------------------------------------------

/// Guest that imports `eo9:entropy/entropy` and calls `get-bytes` with a caller-supplied
/// length, returning the number of bytes it actually received.
const ENTROPY_WAT: &str = r#"
(component
  (import "eo9:entropy/types@0.1.0" (instance $entropy-types
    (export "entropy-impl" (type (sub resource)))))
  (alias export $entropy-types "entropy-impl" (type $entropy-impl))

  (import "eo9:entropy/entropy@0.1.0" (instance $entropy
    (export "entropy-impl" (type $ei (eq $entropy-impl)))
    (export "default" (func (result (own $ei))))
    (export "get-bytes" (func (param "e" (borrow $ei)) (param "len" u64) (result (list u8))))))

  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 1024))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get 2) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get 2))))
      (global.set $heap (i32.add (local.get $ptr) (local.get 3)))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))

  (alias export $entropy "default" (func $default))
  (alias export $entropy "get-bytes" (func $get-bytes))
  (core func $default-lowered (canon lower (func $default)))
  (core func $get-bytes-lowered (canon lower (func $get-bytes)
    (memory $libc "memory") (realloc (func $libc "realloc"))))

  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "default" (func $default (result i32)))
    ;; get-bytes(handle, len, retptr); the returned list is written as (ptr, len) at retptr.
    (import "host" "get-bytes" (func $get-bytes (param i32 i64 i32)))
    (func (export "main") (param $len i64) (result i32)
      (call $get-bytes (call $default) (local.get $len) (i32.const 16))
      (i32.load (i32.const 20))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "default" (func $default-lowered))
      (export "get-bytes" (func $get-bytes-lowered))))))

  (func (export "main") (param "len" u64) (result u32) (canon lift (core func $i "main")))
)
"#;

fn spawn_entropy(image: &Image, len: &str) -> Task {
    Task::spawn(
        image,
        &[NamedArg::new("len", len)],
        SpawnLimits::default(),
        Providers {
            entropy: Some(Box::new(SeededEntropy::new(9))),
            ..Providers::none()
        },
    )
    .unwrap()
}

#[test]
fn entropy_request_within_the_cap_succeeds() {
    let image = compile(ENTROPY_WAT);
    let mut task = spawn_entropy(&image, "4096");
    match run_to_done(&mut task) {
        Outcome::Success(value) => assert_eq!(value.value, "4096"),
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn oversized_entropy_request_is_rejected_before_allocation() {
    let image = compile(ENTROPY_WAT);
    // 1 TiB: would be a host-memory DoS if the cap were not enforced before allocating.
    let mut task = spawn_entropy(&image, "1099511627776");
    match run_to_done(&mut task) {
        Outcome::Trapped(message) => assert!(
            message.contains("exceeds the per-call cap"),
            "unexpected trap message: {message}"
        ),
        other => panic!("expected the task to trap, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// 2. table growth is bounded
// ---------------------------------------------------------------------------------------

/// Guest that grows a funcref table by 100_000 elements and returns `table.grow`'s result
/// (previous size on success, 0xffff_ffff on refusal).
const TABLE_GROW_WAT: &str = r#"
(component
  (core module $m
    (table 1 funcref)
    (func (export "main") (result i32)
      (table.grow (ref.null func) (i32.const 100000))))
  (core instance $i (instantiate $m))
  (func (export "main") (result u32) (canon lift (core func $i "main")))
)
"#;

fn run_table_grow(image: &Image, limits: SpawnLimits) -> String {
    let mut task = Task::spawn(image, &[], limits, Providers::none()).unwrap();
    match run_to_done(&mut task) {
        Outcome::Success(value) => value.value,
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn table_growth_respects_the_explicit_element_ceiling() {
    let image = compile(TABLE_GROW_WAT);

    // Unlimited task: the growth succeeds (previous size was 1).
    assert_eq!(run_table_grow(&image, SpawnLimits::default()), "1");

    // Explicit table ceiling: the growth is refused.
    assert_eq!(
        run_table_grow(
            &image,
            SpawnLimits {
                max_table_elements: Some(1_000),
                ..SpawnLimits::default()
            }
        ),
        "4294967295"
    );
}

#[test]
fn memory_limited_tasks_get_a_derived_table_ceiling() {
    let image = compile(TABLE_GROW_WAT);

    // Only a memory ceiling is configured; the derived table bound (64 KiB / 8 = 8192
    // elements) still refuses the 100_000-element growth.
    assert_eq!(
        run_table_grow(
            &image,
            SpawnLimits {
                max_memory: Some(64 * 1024),
                ..SpawnLimits::default()
            }
        ),
        "4294967295"
    );
}

// ---------------------------------------------------------------------------------------
// 3. start-time code cannot burn unbounded CPU at spawn
// ---------------------------------------------------------------------------------------

/// Component whose core module spins forever in a start function. Spawning it must fail
/// with a bounded amount of work rather than hanging.
const RUNAWAY_START_WAT: &str = r#"
(component
  (core module $m
    (func $spin (loop $l (br $l)))
    (start $spin)
    (func (export "main") (result i32) (i32.const 0)))
  (core instance $i (instantiate $m))
  (func (export "main") (result u32) (canon lift (core func $i "main")))
)
"#;

#[test]
fn runaway_start_time_code_fails_spawn_instead_of_hanging() {
    let image = compile(RUNAWAY_START_WAT);
    let err = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap_err();
    match err {
        SpawnError::Internal(message) => assert!(
            message.contains("spawn fuel budget"),
            "unexpected spawn error: {message}"
        ),
        other => panic!("expected an internal spawn error, got {other:?}"),
    }
}
