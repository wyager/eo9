//! End-to-end "hello" (integration milestone I1, runtime side): a binary component that
//! imports `eo9:text/text` and `eo9:time/time`, receives a typed argument via WAVE, writes
//! through the host-provided text capability, and returns its own
//! `result<program-success, program-failure>` which the runtime renders back as WAVE.

use eo9_runtime::providers::{CaptureText, FrozenTime};
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    EngineOptions, Image, NamedArg, Outcome, Providers, ResumeOutcome, SpawnLimits, Task,
    new_engine,
};

/// `hello: func(name: string) -> result<string, string>`
///
/// Writes `name` to stdout via `eo9:text/text.write`, reads the wall clock via
/// `eo9:time/time.now` (value unused — it proves the time wiring), and returns `ok(name)`,
/// or `err("empty name")` when the argument is empty.
const HELLO_WAT: &str = r#"
(component
  ;; ----- imports: eo9:text and eo9:time -------------------------------------------------
  (import "eo9:text/types@0.1.0" (instance $text-types
    (export "text-impl" (type (sub resource)))))
  (alias export $text-types "text-impl" (type $text-impl))

  ;; Like wit-component's encoding of `use`d types, the instance type re-exports the
  ;; resource (eq-bounded) and exports every named type it defines; anonymous local types
  ;; inside an imported instance type are rejected by the validator.
  (import "eo9:text/text@0.1.0" (instance $text
    (export "text-impl" (type $text-impl-eq (eq $text-impl)))
    (type $output-stream-def (enum "out" "err"))
    (export "output-stream" (type $output-stream (eq $output-stream-def)))
    (type $text-error-def (variant (case "closed") (case "io" string)))
    (export "text-error" (type $text-error (eq $text-error-def)))
    (export "default" (func (result (own $text-impl-eq))))
    (export "write" (func
      (param "t" (borrow $text-impl-eq))
      (param "to" $output-stream)
      (param "text" string)
      (result (result (error $text-error)))))))

  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))

  (import "eo9:time/time@0.1.0" (instance $time
    (export "time-impl" (type $time-impl-eq (eq $time-impl)))
    (type $datetime-def (record (field "seconds" s64) (field "nanoseconds" u32)))
    (export "datetime" (type $datetime (eq $datetime-def)))
    (export "default" (func (result (own $time-impl-eq))))
    (export "now" (func (param "t" (borrow $time-impl-eq)) (result $datetime)))))

  ;; ----- libc: memory, bump realloc, constants -------------------------------------------
  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (data (i32.const 32) "empty name")
    (func (export "realloc") (param $old i32) (param $old-size i32) (param $align i32) (param $new-size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new-size)))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))

  ;; ----- lowered imports ------------------------------------------------------------------
  (alias export $text "default" (func $text-default))
  (alias export $text "write" (func $text-write))
  (alias export $time "default" (func $time-default))
  (alias export $time "now" (func $time-now))

  (core func $text-default-lowered (canon lower (func $text-default)))
  (core func $text-write-lowered (canon lower (func $text-write)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $time-default-lowered (canon lower (func $time-default)))
  (core func $time-now-lowered (canon lower (func $time-now) (memory $libc "memory")))

  ;; ----- the program ----------------------------------------------------------------------
  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "text-default" (func $text-default (result i32)))
    ;; write(handle, stream, text-ptr, text-len, retptr)
    (import "host" "text-write" (func $text-write (param i32 i32 i32 i32 i32)))
    (import "host" "time-default" (func $time-default (result i32)))
    ;; now(handle, retptr)
    (import "host" "time-now" (func $time-now (param i32 i32)))

    (func (export "main") (param $name-ptr i32) (param $name-len i32) (result i32)
      (local $text i32) (local $time i32)
      (local.set $text (call $text-default))
      (local.set $time (call $time-default))
      ;; Read the clock (result written to scratch memory at 64; unused).
      (call $time-now (local.get $time) (i32.const 64))
      ;; Write the name to stdout (stream 0 = out); write's result goes to scratch at 80.
      (call $text-write
        (local.get $text) (i32.const 0)
        (local.get $name-ptr) (local.get $name-len)
        (i32.const 80))
      ;; Build the result<string, string> return value at 96.
      (if (i32.eqz (local.get $name-len))
        (then
          (i32.store (i32.const 96) (i32.const 1))          ;; err
          (i32.store (i32.const 100) (i32.const 32))        ;; "empty name" (in libc data)
          (i32.store (i32.const 104) (i32.const 10)))
        (else
          (i32.store (i32.const 96) (i32.const 0))          ;; ok
          (i32.store (i32.const 100) (local.get $name-ptr))
          (i32.store (i32.const 104) (local.get $name-len))))
      (i32.const 96)))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "text-default" (func $text-default-lowered))
      (export "text-write" (func $text-write-lowered))
      (export "time-default" (func $time-default-lowered))
      (export "time-now" (func $time-now-lowered))))))

  ;; ----- export: main ---------------------------------------------------------------------
  (func (export "main")
    (param "name" string)
    (result (result string (error string)))
    (canon lift (core func $i "main") (memory $libc "memory") (realloc (func $libc "realloc"))))
)
"#;

fn hello_image() -> Image {
    let engine = new_engine(&EngineOptions::default()).unwrap();
    Image::compile(&engine, HELLO_WAT).unwrap()
}

fn run(image: &Image, name_wave: &str) -> (Outcome, CaptureText) {
    let capture = CaptureText::new();
    let providers = Providers {
        text: Some(Box::new(capture.clone())),
        time: Some(Box::new(FrozenTime::new(1_748_000_000, 0))),
        ..Providers::none()
    };
    let mut task = Task::spawn(
        image,
        &[NamedArg::new("name", name_wave)],
        SpawnLimits::default(),
        providers,
    )
    .unwrap();
    let outcome = loop {
        match task.resume(100 * FUEL_QUANTUM) {
            ResumeOutcome::Done(outcome) => break outcome,
            ResumeOutcome::OutOfFuel => continue,
            ResumeOutcome::Blocked => panic!("hello does not block"),
        }
    };
    (outcome, capture)
}

#[test]
fn hello_runs_end_to_end_with_host_text_and_time() {
    let image = hello_image();
    let (outcome, capture) = run(&image, "\"eo9\"");

    // Typed outcome out: the program's own success value, rendered as WAVE plus type text.
    match outcome {
        Outcome::Success(value) => {
            assert_eq!(value.ty, "string");
            assert_eq!(value.value, "\"eo9\"");
        }
        other => panic!("expected success, got {other:?}"),
    }

    // The host-provided text capability saw the write.
    assert_eq!(capture.stdout(), "eo9");
    assert_eq!(capture.stderr(), "");
}

#[test]
fn hello_reports_its_own_failure_vocabulary() {
    let image = hello_image();
    let (outcome, capture) = run(&image, "\"\"");

    match outcome {
        Outcome::Failure(value) => {
            assert_eq!(value.ty, "string");
            assert_eq!(value.value, "\"empty name\"");
        }
        other => panic!("expected failure, got {other:?}"),
    }
    assert_eq!(capture.stdout(), "");
}
