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

## On-target codegen (in progress) — plan/12 Decisions 26 & 28

The on-target-codegen rung (the kernel compiling components on the machine, behind the
off-by-default `wasm-codegen` cargo feature) adds **four** vendored crates here, all kept
kernel-workspace-only via `[patch.crates-io]`. `cranelift-codegen` 0.132 and the small
cranelift sub-crates (entity, bitset, control) are **not** vendored — they build no_std purely
via features. The surprise (Decision 28) was that no_std-clean leaf crates are not enough:
some *dependents* hardcode `std` in their `cranelift-codegen` dependency feature list, and
Cargo feature unification then forces `std` (and std-only crates like `arbitrary` via
`cranelift-control/fuzz`, and `termcolor` via `wasmprinter`) back onto the no_std target. So
two of the four vendored crates exist only to change those dependency lines.

- **`wasmtime-environ` 45.0.0** — already `#![no_std]`; its `compile` feature required `std`.
  Changes: `compile` no longer pulls `std`; `wasm-encoder` taken `default-features = false`;
  `wasmprinter` dropped from `compile` (it pulled the std-only `termcolor`; its one
  Trace-level use in `component/translate/adapt.rs` became a byte-count log); mechanical
  `std::`→`core`/`alloc`/`hashbrown` swaps across the `compile` and `fact` modules. Remaining:
  the `std::path` debug touchpoints (`CompilerBuilder::clif_dir`, `ModuleTranslation.path`).
- **`wasmtime-internal-cranelift` 45.0.0** — the `Compiler` glue. Changes so far: `#![no_std]`
  + `extern crate alloc` + re-export of `wasmtime_environ::prelude`; dependency features
  de-std'd (`cranelift-codegen` → `core`/`unwind`/`host-arch`; `cranelift-frontend` → `core`;
  `gimli` → `read`; `object` → `write_core`; `itertools` → `use_alloc`; `thiserror`
  `default-features = false`); `cranelift-native` made optional and the host-flag-inference
  call (`isa_builder.rs`) gated behind it — the kernel always specifies its target triple, so
  it is never reached, and cranelift-native is std-only. Remaining: the crate's own ~43
  `std::` lines + per-module `use crate::*;` prelude threading.
- **`cranelift-frontend` 0.132.0** and **`wasmtime-internal-unwinder` 45.0.0** — vendored
  *only* to change their `cranelift-codegen` dependency from `features = ["std", …]` to the
  `core` profile. Without this, either edge forces `cranelift-codegen/std` → `gimli/std` +
  `cranelift-control/fuzz` → `arbitrary` onto the target and the build fails.

The existing `wasmtime` patch gained the `cranelift` feature on the kernel build path without
`std`. Cranelift emits native aarch64 (not Pulley), so the publisher's cache maintenance above
is what makes freshly generated code executable. Status: the codegen backend builds no_std;
`wasmtime-internal-cranelift`'s own source + the `clif_dir` touchpoint + the in-kernel compile
demo remain (plan/12 Decision 28 has the full punch list). All of this is behind `wasm-codegen`
(off by default), so the standard builds are unaffected.
