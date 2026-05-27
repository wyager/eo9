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

## Fiberless component-model-async (browser/wasm32 hosts)

For the in-browser Eo9 (wasm32 as the *host* architecture, plan/15 "wasm32 embed spike" /
area 18), `wasmtime/` additionally gains an **opt-in** `component-model-async-fiberless`
feature (off by default; never enabled by the kernel or host builds — with it off the code
is unchanged):

- `Cargo.toml` — declares the feature (depends on `component-model-async` only).
- `src/runtime/component/concurrent.rs` — `run_on_worker` gains a feature-gated arm that
  executes the worker item (a guest call or a queued worker function) directly on the
  current stack instead of creating/resuming a worker fiber. Callback-ABI ("stackless")
  guests — which is what every Eo9 guest is — return to the host with a status code rather
  than blocking mid-frame, so they do not need a fiber; code that genuinely needs to block
  mid-guest-frame already checks `can_block()` (false here, no fiber context installed) and
  fails cleanly. wasmtime-fiber has no wasm32 stack-switching backend, which is what this
  works around.
- `src/runtime/module.rs` — one line: `Module::from_trusted_file` passes the path to
  `wasm_binary_or_text` as `to_str()` (the debug-path surface is `&str` since the on-target
  codegen changes); std-only path, only compiled by std builds of the vendored copy (the
  embed-spike / web blob native driver).

Verified by `www/embed-spike` with `--features fiberless`: the unmodified
`entropy.seeded` stub (async-lifted `configure` + `get-u64`) runs to completion on a
wasm32-hosted wasmtime via Pulley with the exact SplitMix64 sequence the kernel/native
embeddings produce, through both the `call_async` and `run_concurrent`/`call_concurrent`
entry points.

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
is what makes freshly generated code executable. All of this is behind `wasm-codegen`
(off by default), so the standard builds are unaffected.

**Status: the on-target codegen path works** (plan/12 Decision 29). The full no_std source port
landed; the remaining source-level changes beyond the dependency-feature edits above were:

- **`wasmtime-environ`** — `component/` module mechanical `std::`→`core`/`alloc`/`hashbrown`
  swaps (its `compile`/`fact` modules were already done); `IndexMap` given an explicit
  `hashbrown::DefaultHashBuilder` (the registry default `RandomState` is std-only); one
  `HashMap::get` made to pass `&K` explicitly (hashbrown's `Equivalent` get doesn't deref-coerce
  like std's); a declaration-order fix in the component inliner so `inliner`'s drop does not
  outlive a `types_ref` borrow (std's `HashMap` has a `#[may_dangle]` dropck eyepatch that
  hashbrown lacks); `WasmFileInfo.path`/`clif_dir` switched from `PathBuf`/`&Path` to
  `String`/`&str` so the trait is no_std-expressible.
- **`wasmtime-internal-cranelift`** — `#[macro_use]` on `extern crate alloc` (for `vec!`/`format!`)
  + a `hashbrown` dependency + per-module `use crate::*;` so `Vec`/`String`/`Box` resolve under
  `#![no_std]`; mechanical `std::`→`core`/`alloc`/`hashbrown` swaps; a small `sync::Mutex`
  (non-blocking spinlock, mirroring the vendored `wasmtime`'s) for the compiler-context pool
  since there is no `std::sync::Mutex`; the CLIF-dump filesystem write gated behind
  `feature = "std"` (the `clif_dir`/`emit_clif` path types are `String`/`&str`, the DWARF
  synthetic-path `PathBuf` became `String`); `cranelift_codegen::timing::take_current` (only
  exists with the std-only `timing` feature) dropped from the two compile-time log lines.
- **`cranelift-frontend`** — `#[macro_use]` on `extern crate alloc` and a few `std::`→`core::`
  swaps (`error::Error`, `num::NonZeroU8`, `iter::once`) the upstream no_std path had missed.
- **`wasmtime`** — its own `compile` module + `config.rs` `std::`→`core`/`alloc` swaps; the
  file-reading `CodeBuilder` methods (`wasm_binary_file`, `dwarf_package_file`, the `.dwp`
  auto-probe) and `Config::emit_clif` gated behind `feature = "std"`, with the source-path
  fields carried as `str`/`String`; `precompile_compatibility_hash`'s `std::hash::Hash` →
  `core::hash::Hash`. The kernel engine now sets `target("aarch64-unknown-none")` explicitly
  (host CPU inference needs the std-only `cranelift-native`) and the OS-less tunables, so
  wasmtime's native-host compatibility check — which the linked compiler runs on *every*
  engine, deserialize and on-target alike — passes (`Triple::host()` equals the build target).
