# kernel/vendor — locally patched dependencies for the bare-metal build

This directory holds source copies of upstream crates that the kernel workspace patches via
`[patch.crates-io]` in `kernel/Cargo.toml`. The patches apply to the **kernel workspace
only** — the host and guest workspaces keep using the unmodified registry crates.

Nothing here is a fork we intend to keep: every change is the minimal, upstream-shaped
relaxation needed for the bare-metal target, recorded below (and in plan/12-kernel.md
Decisions) so it can be turned into an upstream PR or dropped when upstream catches up.

## wasmtime (45.0.0)

`wasmtime/` is the crates.io `wasmtime-45.0.0` source with one goal: let the
`component-model-async` host machinery build for `no_std + alloc` targets
(`aarch64-unknown-none`), because every real Eo9 guest uses the Component Model async ABI
(async `main`/`configure`, async I/O operations). Upstream gates that machinery on `std`;
the std surface it actually uses is small and mostly incidental. Changes, by file:

- `Cargo.toml` — the `component-model-async` feature no longer requires `std` or
  `futures/std` (the `futures` items used — oneshot channels, `FuturesUnordered`,
  `StreamExt` — are all available with `futures/alloc`).
- `src/runtime/component/concurrent.rs`, `concurrent/{table,abort,future_stream_any}.rs`,
  `concurrent/futures_and_streams/buffers.rs` — `std::` paths that are really
  `core`/`alloc` (imports, `std::slice`, `std::cmp`), `std::collections::HashSet` →
  the crate's own `crate::hash_set::HashSet`, `std::sync::{Arc, Mutex}` →
  `alloc::sync::Arc` + `crate::sync::Mutex`.
- `src/runtime/component/concurrent/futures_and_streams.rs` — same import treatment, plus:
  the internal host-to-host buffer cursor no longer uses `std::io::Cursor` (replaced by a
  ~30-line private `VecCursor` with the same behavior); the `std::io::Read`/`Write`
  convenience impls on `DirectSource`/`DirectDestination` are `#[cfg(feature = "std")]`;
  the two places that converted `futures::channel::oneshot::Canceled` into an error via
  `?`/`.into()` (which needs the `std`-only `Error` impl on `Canceled`) construct the
  error explicitly instead.
- `src/runtime/component/concurrent/tls.rs` — the `std::thread_local!` slot is now
  `#[cfg(feature = "std")]`; without `std` the same single pointer lives in an
  embedder-provided slot reached through the custom platform layer
  (`wasmtime_concurrent_tls_get/set`), exactly like the existing `wasmtime_tls_get/set`
  slot used by the trap handlers. The kernel provides both pairs (src/wasm/mod.rs).
- `src/runtime/vm/sys/custom/capi.rs`, `sys/custom/mod.rs` — declare the two new
  embedder symbols (gated on `component-model-async`); `src/runtime/vm.rs` makes the `sys`
  module `pub(crate)` so the component layer can reach it.
- `src/sync_std.rs` / `src/sync_nostd.rs` — `crate::sync` now also offers `Mutex` /
  `MutexGuard`: a re-export of `std::sync`'s on `std`, and a small non-blocking
  (panic-on-contention `lock`, `WouldBlock` from `try_lock`) implementation on `no_std`,
  matching the existing philosophy of `sync_nostd.rs`.
- `src/runtime/component/linker.rs`, `src/runtime/vm/component/libcalls.rs` — two
  stray `std::` paths that are really `core` (`pin!`, `poll_fn`, `MaybeUninit`).

Behavior on `std` builds is unchanged (the std paths are kept under `cfg(feature = "std")`
or are re-exports). `wasmtime-fiber` needed no changes: upstream already ships a `no_std`
backend with the aarch64 stack-switching code.

The kernel additionally provides, as the embedder: `wasmtime_tls_get/set`,
`wasmtime_concurrent_tls_get/set`, and a `CustomCodeMemory` publisher (D-cache clean +
I-cache invalidate over published code) — see `kernel/eo9-kernel/src/wasm/mod.rs`.

## On-target codegen (planned, not yet vendored) — plan/12 Decision 26

The next rung (the kernel compiling components on the machine, behind an off-by-default
`wasm-codegen` cargo feature) will add two more vendored crates here, both kept
kernel-workspace-only via `[patch.crates-io]`. The fork surface was surveyed before
vendoring (so the diff stays minimal):

- **`wasmtime-environ` 45.0.0** — already `#![no_std]`; its `compile` feature currently
  requires `std`. Planned changes: drop `std` from the `compile` feature (it already pulls
  the alloc-friendly `object/write_core` + `gimli/write` paths) and fix the residual
  `std::` in the `compile` module — almost all mechanical core/alloc swaps, with a small
  number of genuine touchpoints (notably `std::path::PathBuf` in `compile/module_environ.rs`).
- **`wasmtime-internal-cranelift` 45.0.0** — not yet `#![no_std]` (~43 `std::` lines); the
  `Compiler` glue. Planned: add `#![no_std]` + `extern crate alloc`, convert std→core/alloc,
  and drive `object`/`gimli` through their alloc-only write paths.

`cranelift-codegen` 0.132 and the small cranelift sub-crates are **not** vendored: they build
no_std purely via features (`default-features = false` + `core`, no `std`/`timing`/
`souper-harvest`; hashbrown is the no_std `HashMap` fallback). The existing `wasmtime` patch
will gain the `cranelift`/`compile` features on the kernel build path. Cranelift emits native
aarch64 (not Pulley), so the publisher's cache maintenance above is what makes freshly
generated code executable.
