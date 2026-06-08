# The misaligned fiber stack: riscv64/x86_64 spinner-kill panic, bisect and root cause

Two reviewers independently flagged the same master regression: `cargo xtask qemu riscv64
demo` (and the x86_64 run) panics at the sched-demo spinner-kill step —

```
KERNEL PANIC: panicked at vendor/wasmtime-unwinder/src/arch/riscv64.rs:49:5:
assertion `left == right` failed: stack should always be aligned to 16
  left: 8
 right: 0
```

Both digests print first; the run ends panic-poweroff instead of the canonical
`abnormal(killed)` + clean power-off. aarch64 completes the same kill cleanly. This doc
records the reproduction, the bisect, and the root cause; the fix lands separately
(kernel/vendor/wasmtime-fiber, see kernel/vendor/README.md).

## Repro and first cut

Reproduced on master exactly as reported (riscv64, both digests, then the panic). The two
named suspects both checked out clean before any bisect step ran:

* **first-poll-inline is not the trigger.** `EO9_KERNEL_FEATURES_REMOVE=first-poll-inline
  cargo xtask qemu riscv64 demo` panics identically with the feature off.
* **the refreshed cruncher is not the trigger.** The bundle refresh (`65bfe16`) touched
  `eosh`/`init` and added `time`; `cruncher.wasm` is byte-identical across the window.

## Bisect

First-parent walk, then commits inside the guilty merge. Each step is a full
build + `qemu riscv64 demo` run; PASS means the spinner kill completes
(`abnormal(killed)`, clean SBI shutdown, exit 0).

| commit | what it is | verdict |
|---|---|---|
| `b06044d` (master) | boot-beacons merge | PANIC |
| `c5fc471` | merge area/03-component-args | PANIC |
| `a8bf902` | merge area/12-serial-loader (first parent of the merge) | PASS |
| `bc8e639` | examples: time joins the baked-in store | **PANIC — first bad** |
| `0ab1868` | eosh component-typed parameters (its parent) | PASS |

`bc8e639` adds **zero kernel, runtime, or vendored code**: a new guest crate
(`guest/examples/time`) and five xtask lines adding `time` to `GUEST_COMPONENTS` /
`KERNEL_STORE_COMPONENTS`. A commit that only adds a component to the baked-in store cannot
break the unwinder by semantics — it can only *shift things in memory*. That reframed the
question from "what changed" to "what was always broken and layout-sensitive".

## Root cause

Instrumenting the unwinder showed the very first walked frame is already misaligned, and so
is the walk's terminal:

```
misaligned fp during walk: pc=0x832152f4 fp=0x83e864c8 trampoline_fp=0x83e865a8 steps=0
```

`fp % 16 == 8` **and** `trampoline_fp % 16 == 8`: not one bad frame — the entire stack the
frames live on is shifted by 8. Those addresses sit on a wasmtime **fiber stack** (the
async machinery suspends each fuel-sliced child on its own fiber; the kill drops the
suspended store, and the cancellation path walks the suspended activation's frames).

The fiber stacks come from `wasmtime-internal-fiber` 45.0.0's no_std backend
(`src/nostd.rs`, selected because the kernel is bare metal). Its `FiberStack::new`:

```rust
let mut storage = TryVec::new();          // a byte vector: align 1 from the allocator
storage.reserve_exact(size)?;
let (base, len) = align_ptr(storage.as_mut_ptr(), size, STACK_ALIGN);  // base aligned UP to 16,
                                                                       // top stays at storage end
```

`align_ptr` rounds the **base** up to 16 but deliberately keeps `base + len` at the raw
allocation's end ("Also updates the length as appropriate so that `ptr + len` points to the
same endpoint"). The **top** of the stack — where `wasmtime_fiber_init` writes the saved-state
words and where execution starts — therefore carries whatever alignment the global allocator
happened to hand out for a byte vector. The kernel's `linked_list_allocator` returns 8-aligned
blocks for align-1 requests; whether a given fiber stack's end lands on a 16- or 8-aligned
address depends on every allocation that preceded it.

A tripwire assert in `FiberStack::new` confirmed it on the failing build:

```
fiber stack top is misaligned: base=0x83c86e50 len=0x1ffff8 top=0x83e86e48
```

`top % 16 == 8`, and the panicking walk's FP range sits inside exactly this stack. Cranelift
code preserves SP/FP alignment *relative to entry*, so a stack entered at `top ≡ 8 (mod 16)`
has **every** frame pointer on it `≡ 8 (mod 16)` — perfectly self-consistent, walkable, and
8 bytes off the ABI.

The upstream unix backend can never see this: it mmaps fiber stacks, so the top is
page-aligned for free. The bug is specific to the no_std path, and to allocators that return
sub-16 alignment for byte buffers.

### Why adding `time` to the store flips it

The baked-in store is parsed/registered at boot before the demo spawns children; one more
component means different heap traffic, which moves the parity of the address at which the
spinner's fiber-stack `TryVec` lands. `bc8e639` did not break anything — it rolled the dice
the unlucky way. Any prior commit could have done the same (entry-76-era smokes were green
by the same luck), and any future allocation change could have flipped it back.

### Why aarch64 "passes"

The arch asymmetry is in the **assert**, not the walk. `wasmtime-unwinder`'s
`assert_fp_is_aligned` checks `fp % 16 == 0` on riscv64 and x86_64; on aarch64 it is
deliberately a no-op, because AAPCS64 does not constrain where the frame record lives.
aarch64 runs the same misaligned fiber stacks whenever its heap parity falls that way — it
just never checks, the (self-consistent) walk succeeds, and the kill completes. So aarch64
was not unaffected; it was silently tolerating the same defect the other two arches turn
into a panic. The fix removes the misalignment on all three.

## The fix (separate commit)

`wasmtime-internal-fiber` is now vendored (`kernel/vendor/wasmtime-fiber`, kernel workspace
only) with one change in `src/nostd.rs::FiberStack::new`: after aligning the base up, round
the usable length down to a multiple of `STACK_ALIGN`, so `base + len` — the top — is
16-aligned too. Upstream-shaped, candidate for an upstream PR. Blast radius: the no_std
fiber backend only — host and guest workspaces keep the registry crate and its mmap'd
(page-aligned) stacks; on the kernel side every fiber stack now starts aligned regardless
of allocator parity, and the kill/cancel unwind walks aligned frames on all three arches.

Residual, recorded: `FiberStack::from_raw_parts` (used by the pooling allocator, which the
kernel does not use) still trusts the caller's alignment; the demo/store paths all go
through `FiberStack::new`.

## Verification

With the fix: riscv64 + x86_64 + aarch64 demos canonical including the spinner kill
(`abnormal(killed)`, clean SBI/ACPI/PSCI power-off, exit 0); `cargo xtask firstpoll-ab
--gate-only` semantic-identity PASS, both arms; full `cargo xtask ci` green. The chaos
harness was not run: it drives the host `eo9` binary (std fibers, registry wasmtime),
which this change cannot reach, and the kill path's own code is untouched.
