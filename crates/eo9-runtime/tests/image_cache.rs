//! Image serialization round-trip: the hook the usermode compilation cache uses to skip
//! codegen on a hit (compile → serialize → deserialize → run).

use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    CompileError, EngineOptions, Image, Outcome, Providers, ResumeOutcome, SpawnLimits, Task,
    new_engine,
};

/// Pure-compute guest: sums 0..10_000 and returns the total (same fixture as task_api.rs).
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

fn run_to_done(image: &Image) -> Outcome {
    let mut task = Task::spawn(image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    loop {
        match task.resume(100 * FUEL_QUANTUM) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => continue,
            ResumeOutcome::Blocked => panic!("compute guest cannot block"),
        }
    }
}

fn success_value(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Success(value) => &value.value,
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn serialized_image_round_trips_and_runs() {
    let engine = new_engine(&EngineOptions::default()).unwrap();

    let compiled = Image::compile(&engine, COMPUTE_WAT).unwrap();
    assert_eq!(success_value(&run_to_done(&compiled)), COMPUTE_EXPECTED);

    let bytes = compiled.serialize().unwrap();

    // A fresh engine built from the same options accepts the bytes (this is what a cache
    // hit in a later `eo9 run` invocation looks like) and the reloaded image behaves
    // identically — no recompilation involved.
    let other_engine = new_engine(&EngineOptions::default()).unwrap();
    assert_eq!(
        eo9_runtime::compatibility_hash(&engine),
        eo9_runtime::compatibility_hash(&other_engine)
    );
    let reloaded = unsafe { Image::deserialize(&other_engine, &bytes) }.unwrap();
    assert_eq!(success_value(&run_to_done(&reloaded)), COMPUTE_EXPECTED);
}

#[test]
fn deserialize_rejects_bytes_that_are_not_a_serialized_image() {
    let engine = new_engine(&EngineOptions::default()).unwrap();

    // Arbitrary junk is rejected cleanly rather than loaded as code.
    let err = unsafe { Image::deserialize(&engine, b"definitely not an image") }.unwrap_err();
    assert!(matches!(err, CompileError::BadImage(_)), "{err}");

    // Un-precompiled input (the WAT text itself) is also not a serialized image.
    let err = unsafe { Image::deserialize(&engine, COMPUTE_WAT.as_bytes()) }.unwrap_err();
    assert!(matches!(err, CompileError::BadImage(_)), "{err}");
}
