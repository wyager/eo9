;; The seed component for the bare-metal spike (plan/12-kernel.md, spike step 1).
;;
;; A deliberately tiny, dependency-free Component Model component written directly in the
;; component text format: it exports `hello: func() -> string` and `add: func(a: u32,
;; b: u32) -> u32` through synchronous canonical lifts (the Component Model async ABI needs
;; wasmtime's `std`-only machinery, so the seed stays sync). `cargo xtask build-kernel
;; aarch64` assembles this file, precompiles it for `aarch64-unknown-none` with the host
;; wasmtime/Cranelift, and embeds the resulting artifact in the kernel image, where the
;; no_std wasmtime runtime deserializes, instantiates, and calls it.
(component
  (core module $m
    (memory (export "mem") 1)
    ;; 53 bytes of greeting at offset 16; the 8-byte return area lives at offset 0.
    (data (i32.const 16) "Hello from a WebAssembly component on bare-metal Eo9!")
    (func (export "hello") (result i32)
      ;; Store (ptr, len) of the greeting into the return area and return its address,
      ;; per the canonical ABI for a lifted `func() -> string`.
      (i32.store (i32.const 0) (i32.const 16))
      (i32.store (i32.const 4) (i32.const 53))
      (i32.const 0))
    (func (export "add") (param i32 i32) (result i32)
      (i32.add (local.get 0) (local.get 1))))
  (core instance $i (instantiate $m))
  (func (export "hello") (result string)
    (canon lift (core func $i "hello") (memory $i "mem") string-encoding=utf8))
  (func (export "add") (param "a" u32) (param "b" u32) (result u32)
    (canon lift (core func $i "add"))))
