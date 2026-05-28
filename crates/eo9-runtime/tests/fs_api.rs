//! Host wiring tests for `eo9:io/buffers` and `eo9:fs/fs` using hand-written guests. The
//! async WIT operations are imported as async functions and sync-lowered from an
//! async-lifted `main` (sync-lifted exports cannot block in wasmtime 45).

use eo9_runtime::providers::MemFs;
use eo9_runtime::task::FUEL_QUANTUM;
use eo9_runtime::{
    EngineOptions, Image, Outcome, Providers, ResumeOutcome, SpawnLimits, Task, new_engine,
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
            ResumeOutcome::Blocked => panic!("in-memory providers never leave a task blocked"),
        }
    }
}

fn success_value(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Success(value) => &value.value,
        other => panic!("expected success, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// eo9:io/buffers: constructor + accessors backed by the task's buffer table
// ---------------------------------------------------------------------------------------

/// Builds an 8-byte buffer, writes [7, 9, 11] at offset 1, reads back the byte at offset 2
/// and the total length, and returns byte * 100 + len = 9 * 100 + 8 = 908.
const BUFFERS_WAT: &str = r#"
(component
  (import "eo9:io/buffers@0.1.0" (instance $buffers
    (export "buffer" (type $buffer (sub resource)))
    (export "[constructor]buffer" (func (param "len" u64) (result (own $buffer))))
    (export "[method]buffer.len" (func (param "self" (borrow $buffer)) (result u64)))
    (export "[method]buffer.read" (func (param "self" (borrow $buffer)) (param "offset" u64) (param "len" u64) (result (list u8))))
    (export "[method]buffer.write" (func (param "self" (borrow $buffer)) (param "offset" u64) (param "bytes" (list u8))))))

  (alias export $buffers "[constructor]buffer" (func $new))
  (alias export $buffers "[method]buffer.len" (func $len))
  (alias export $buffers "[method]buffer.read" (func $read))
  (alias export $buffers "[method]buffer.write" (func $write))

  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (data (i32.const 32) "\07\09\0b")
    (func (export "realloc") (param $old i32) (param $old-size i32) (param $align i32) (param $new-size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new-size)))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))

  (core func $new-lowered (canon lower (func $new)))
  (core func $len-lowered (canon lower (func $len)))
  (core func $read-lowered (canon lower (func $read) (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $write-lowered (canon lower (func $write) (memory $libc "memory")))

  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "new" (func $new (param i64) (result i32)))
    (import "host" "len" (func $len (param i32) (result i64)))
    ;; read(self, offset, len, retptr) -> list written as (ptr, len32) at retptr
    (import "host" "read" (func $read (param i32 i64 i64 i32)))
    (import "host" "write" (func $write (param i32 i64 i32 i32)))

    (func (export "main") (result i32)
      (local $b i32) (local $byte i32) (local $l i32)
      (local.set $b (call $new (i64.const 8)))
      ;; write the 3 bytes stored at memory[32] to offset 1
      (call $write (local.get $b) (i64.const 1) (i32.const 32) (i32.const 3))
      ;; read 1 byte back from offset 2 (the value 9); the list lands via retptr at 64
      (call $read (local.get $b) (i64.const 2) (i64.const 1) (i32.const 64))
      (local.set $byte (i32.load8_u (i32.load (i32.const 64))))
      (local.set $l (i32.wrap_i64 (call $len (local.get $b))))
      (i32.add (i32.mul (local.get $byte) (i32.const 100)) (local.get $l))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "new" (func $new-lowered))
      (export "len" (func $len-lowered))
      (export "read" (func $read-lowered))
      (export "write" (func $write-lowered))))))

  (func (export "main") (result u32) (canon lift (core func $i "main")))
)
"#;

#[test]
fn buffers_are_backed_by_the_task_buffer_table() {
    let image = compile(BUFFERS_WAT);
    // Buffers carry no authority, so they are available even to a task with no providers.
    let mut task = Task::spawn(&image, &[], SpawnLimits::default(), Providers::none()).unwrap();
    assert_eq!(success_value(&run_to_done(&mut task)), "908");
}

// ---------------------------------------------------------------------------------------
// eo9:fs/fs: awaiting the async path operations through the host wiring
// ---------------------------------------------------------------------------------------

/// Creates `/data` through the fs capability, stats it, and returns
/// `kind-discriminant * 10 + create-ok`, i.e. 11 when the directory was created and stats
/// as a directory. Both operations are async functions; the guest sync-lowers them from an
/// async-lifted `main`, so each call parks the task until the host completes it.
const FS_DIR_WAT: &str = r#"
(component
  (import "eo9:fs/fs@0.1.0" (instance $fs
    (export "fs-impl" (type $fsi (sub resource)))
    (type $fs-error-def (variant
      (case "not-found") (case "already-exists") (case "not-a-directory")
      (case "is-a-directory") (case "denied") (case "read-only") (case "no-space")
      (case "not-immutable") (case "io" string)))
    (export "fs-error" (type $fs-error (eq $fs-error-def)))
    (type $node-kind-def (enum "file" "directory"))
    (export "node-kind" (type $node-kind (eq $node-kind-def)))
    (type $node-stat-def (record (field "kind" $node-kind) (field "size" u64)))
    (export "node-stat" (type $node-stat (eq $node-stat-def)))
    (export "default" (func (result (own $fsi))))
    (export "create-directory" (func async (param "fs" (borrow $fsi)) (param "path" string)
      (result (result (error $fs-error)))))
    (export "stat" (func async (param "fs" (borrow $fsi)) (param "path" string)
      (result (result $node-stat (error $fs-error)))))))

  (alias export $fs "default" (func $default))
  (alias export $fs "create-directory" (func $create-dir))
  (alias export $fs "stat" (func $stat))

  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (data (i32.const 32) "/data")
    (func (export "realloc") (param $old i32) (param $old-size i32) (param $align i32) (param $new-size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new-size)))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))

  (core func $default-lowered (canon lower (func $default)))
  ;; Sync lowers of async callees: results land via the trailing return pointer.
  (core func $create-lowered (canon lower (func $create-dir)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $stat-lowered (canon lower (func $stat)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $task-return (canon task.return (result u32)))

  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "default" (func $default (result i32)))
    ;; create-directory(fs, path-ptr, path-len, retptr)
    (import "host" "create-directory" (func $create (param i32 i32 i32 i32)))
    ;; stat(fs, path-ptr, path-len, retptr)
    (import "host" "stat" (func $stat (param i32 i32 i32 i32)))
    (import "host" "task-return" (func $task-return (param i32)))

    (func (export "main")
      (local $fs i32) (local $create-ok i32) (local $kind i32)
      (local.set $fs (call $default))

      ;; create-directory("/data"); result<_, fs-error> lands at 160 (discriminant 0 = ok)
      (call $create (local.get $fs) (i32.const 32) (i32.const 5) (i32.const 160))
      (local.set $create-ok (i32.eqz (i32.load8_u (i32.const 160))))

      ;; stat("/data"); result<node-stat, fs-error> lands at 192
      (call $stat (local.get $fs) (i32.const 32) (i32.const 5) (i32.const 192))
      ;; ok payload: node-stat { kind (u8 enum), size (u64) } starts at 192 + 8
      (local.set $kind (i32.load8_u (i32.const 200)))

      (call $task-return
        (i32.add (i32.mul (local.get $kind) (i32.const 10)) (local.get $create-ok)))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "default" (func $default-lowered))
      (export "create-directory" (func $create-lowered))
      (export "stat" (func $stat-lowered))
      (export "task-return" (func $task-return))))))

  (func (export "main") async (result u32) (canon lift (core func $i "main") async))
)
"#;

#[test]
fn fs_path_operations_are_awaitable_through_the_host_wiring() {
    let image = compile(FS_DIR_WAT);
    let memfs = MemFs::new();
    let mut task = Task::spawn(
        &image,
        &[],
        SpawnLimits::default(),
        Providers {
            fs: Some(Box::new(memfs.clone())),
            ..Providers::none()
        },
    )
    .unwrap();

    // kind=directory (1) * 10 + create-ok (1) = 11.
    assert_eq!(success_value(&run_to_done(&mut task)), "11");
}
