;; The async canary for kernel milestone 3 (plan/12-kernel.md).
;;
;; A small hand-written component that exercises the Component Model async ABI against the
;; kernel's root providers: it imports `eo9:time/time` (the same interface the real
;; providers serve), reads the monotonic clock, performs an awaited `sleep` — an async
;; operation that suspends the guest task until the kernel's timer says the duration has
;; elapsed — reads the clock again, and returns the measured elapsed nanoseconds through an
;; async-lifted `run` export (`task.return`, stackful lift). A returned value of at least
;; the requested 50 ms proves suspension and resumption of a guest task on bare metal.
;;
;; Like kernel/seed/hello.wat this lives outside the guest workspace because it is a
;; kernel-area test artifact; `cargo xtask build-kernel aarch64` assembles and precompiles
;; it and the kernel embeds it behind the `wasm-async` feature.
(component
  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))
  (import "eo9:time/time@0.1.0" (instance $time
    (export "time-impl" (type $ti (eq $time-impl)))
    (type $instant (record (field "nanoseconds" u64)))
    (export "instant" (type $instant' (eq $instant)))
    (export "default" (func (result (own $ti))))
    (export "monotonic-now" (func (param "t" (borrow $ti)) (result $instant')))
    (export "sleep" (func async (param "t" (borrow $ti)) (param "duration-ns" u64)))))

  (alias export $time "default" (func $default))
  (alias export $time "monotonic-now" (func $monotonic-now))
  (alias export $time "sleep" (func $sleep))

  (core func $default-lowered (canon lower (func $default)))
  (core func $monotonic-now-lowered (canon lower (func $monotonic-now)))
  (core func $sleep-lowered (canon lower (func $sleep)))
  (core func $task-return (canon task.return (result u64)))

  (core module $m
    (import "host" "default" (func $default (result i32)))
    (import "host" "monotonic-now" (func $monotonic-now (param i32) (result i64)))
    (import "host" "sleep" (func $sleep (param i32 i64)))
    (import "host" "task-return" (func $task-return (param i64)))

    (func (export "run")
      (local $handle i32)
      (local $start i64)
      (local.set $handle (call $default))
      (local.set $start (call $monotonic-now (local.get $handle)))
      ;; Sleep 50 ms against the kernel's generic timer; the sync-lowered call to the
      ;; async operation suspends this task until the host completes it.
      (call $sleep (local.get $handle) (i64.const 50000000))
      ;; Report the measured elapsed monotonic nanoseconds.
      (call $task-return
        (i64.sub (call $monotonic-now (local.get $handle)) (local.get $start)))))

  (core instance $i (instantiate $m
    (with "host" (instance
      (export "default" (func $default-lowered))
      (export "monotonic-now" (func $monotonic-now-lowered))
      (export "sleep" (func $sleep-lowered))
      (export "task-return" (func $task-return))))))

  (func (export "run") async (result u64) (canon lift (core func $i "run") async))
)
