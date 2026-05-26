# 12 — Bare metal / QEMU images (`kernel/`)

## Scope
Bootable Eo9 images for AMD64, AArch64, rv64gc per the spec deliverable: boot, run a headless program, and
boot-to-eosh over serial. Arch order is confirmed (aarch64 → riscv64 → x86_64). Execution strategy is
decided: **on-target codegen is part of the MVP** — the kernel is no_std **+ alloc** and carries the
compiler; host-side AOT/cross-compilation is only a dev convenience and bootstrap seed.

## Spec references
"Hardware Support", "Bootable QEMU Images" deliverable, "Performance" (no MMU for privilege; bounds-check
caveat), "Execution APIs" (hardware roots; schedulers), Implementation Details (shared scheduler).

## Deliverables
- `kernel/eo9-kernel` (no_std core, arch-independent): entry glue → heap allocator → serial console →
  platform impl of the `eo9-sched` traits → load components/images from a baked-in read-only store image
  (plan 06) → run the configured program headless, or eosh with serial as the text provider.
- Arch ports under `kernel/arch/`: aarch64 (QEMU `virt`) first, then riscv64 (`virt`), then x86_64
  (bootloader crate or Limine). Each: linker script/target json, entry, timer, interrupt glue, serial.
- Root providers on metal (MVP): text = serial; time = arch timer; entropy = seeded-from-boot-entropy or
  virtio-rng; disk = virtio-blk (stretch); fs = read-only store image first; net = out of MVP scope on metal.
- Execution strategy (the spike, do this first): on-target codegen under no_std + alloc. Step 1: the runtime
  half — load and run a component on target (Wasmtime "min-platform"-style no_std embedding), seeded by a
  host-compiled artifact if needed. Step 2: the compiler half — build cranelift-codegen and the wasm→CLIF
  translation path for the no_std+alloc target, then compile and run a trivial module entirely on the
  machine under QEMU. Success = hello-over-serial where the hello was compiled on target. Report exactly
  which wasmtime/cranelift crates build cleanly for no_std+alloc and which need patching or replacing; if the
  upstream compilation crates fundamentally require std, stop and bring findings + options (vendored patches,
  driving cranelift directly, staging via host AOT) to the planner — this is the single riskiest assumption
  in the whole plan.
- `xtask qemu <arch>`: build store image + kernel, launch QEMU with serial on stdio; used by plan 13.

## Dependencies
01, 04 (cross-compiled AOT artifacts + any runtime code reuse), 05, 06 (store image). Start after the
Phase-1 areas have their first milestones; the spike can start as soon as 04's compile path can cross-compile.

## Milestones
1. Spike step 1: hello over serial on aarch64/QEMU (runtime half; seed artifact may be host-compiled) (I4).
2. Spike step 2: on-target codegen — compile and run a module on the machine itself.
3. Scheduler + multiple tasks + store image; headless program selection via kernel cmdline.
4. eosh over serial (boot-to-shell); riscv64 port; x86_64 port (I5).

## Decisions

1. **Spike step 1 is done (aarch64/QEMU `virt`).** `cargo xtask build-kernel aarch64` builds the image;
   `cargo xtask qemu aarch64` boots it (`qemu-system-aarch64 -M virt -cpu max -smp 1 -m 512M -nographic
   -kernel kernel/target/aarch64-unknown-none/release/eo9-kernel`). Serial shows: banner + EL + counter
   frequency, heap init and self-test, generic-timer readings plus a polled 10 ms timer condition, then the
   embedded wasm component's `hello()` string and `add(17,25)` result, and a PSCI SYSTEM_OFF so QEMU exits by
   itself (Ctrl-A then X to quit early). The kernel milestone 1 "hello over serial, seed host-compiled" is
   therefore met including the wasm half.
2. **Image format and memory map.** The kernel is a plain ELF linked at 0x4020_0000 (RAM on `virt` starts at
   0x4000_0000; the first 2 MiB are left to QEMU's DTB), loaded directly by QEMU's `-kernel` ELF loader — no
   objcopy step, the ELF entry is the boot stub. Boot stack (512 KiB) and then the heap sit after the image;
   the heap runs to the top of the 512 MiB of guest RAM (`RAM_SIZE` in heap.rs must match xtask's `-m`).
3. **MMU off for the spike.** All kernel Rust is built for `aarch64-unknown-none` (strict-align, no FP), so
   Device-memory alignment rules are respected, and heap-resident code is executable because nothing is
   mapped non-executable. Follow-ups before this is more than a spike: identity-mapped translation tables
   (Normal cacheable RAM + Device MMIO), D/I-cache maintenance when publishing code (QEMU does not need it,
   real hardware does), and GIC bring-up for timer interrupts (the spike polls `CNTP_CTL_EL0.ISTATUS`).
   FP/SIMD is already enabled at EL1 for Cranelift-generated code.
4. **Exceptions and exit.** Any CPU exception is fatal: the vector table dumps ESR/ELR/FAR over serial and
   parks (wasm traps are explicit checks, not CPU exceptions, with signals-based traps disabled). Panics
   print and power off via PSCI (HVC conduit) so automated runs terminate.
5. **Dependencies (kept minimal, MMIO hand-rolled).** Kernel: `linked_list_allocator` 0.10 as the global
   allocator (small free-list, reclaims memory — wasmtime churns the heap; pulls only lock_api/spinning_top),
   and `wasmtime` 45 with `default-features = false, features = ["runtime", "component-model"]` behind the
   `wasm-seed` feature. No register-access crates; UART/timer/PSCI are hand-rolled MMIO and `mrs`/`msr`.
   xtask: `wat` (assemble the seed) and `wasmtime` (host-side precompile) — both already in the root pin
   table/lockfile. CI builds the kernel workspace without `wasm-seed`, so the gate stays lean; `build-kernel`
   always builds the full image.
6. **Wasm on target — what works today.** wasmtime 45 compiles and runs on `aarch64-unknown-none` (no_std +
   alloc) in the above configuration. Embedder obligations in this mode: the two custom-platform TLS symbols
   (`wasmtime_tls_get/set`) and a `CustomCodeMemory` publisher (no-op + barrier with the MMU off) supplied
   via `Config::with_custom_code_memory` — nothing else, because virtual memory, native signals, and custom
   sync are all compiled out. Linear memories are heap allocations with explicit bounds checks; traps are
   explicit checks. The host-precompiled artifact must match the kernel engine's compile-relevant settings:
   `target("aarch64-unknown-none")`, `signals_based_traps(false)`, `memory_reservation(0)`,
   `memory_reservation_for_growth(1<<20)`, `memory_guard_size(0)`, `memory_init_cow(false)`,
   `concurrency_support(false)`, and GC/threads wasm features off (the host xtask build has those cargo
   features via eo9-runtime, the kernel build does not). Measured under TCG: 133 KB artifact for the seed;
   deserialize + instantiate + two typed calls ≈ 21 ms.
7. **Seed component.** Hand-written component-format WAT (`kernel/seed/hello.wat`, exports `hello: func() ->
   string` and `add: func(u32, u32) -> u32` via sync canonical lifts), assembled and precompiled by
   `build-kernel`, embedded with `include_bytes!`. It lives outside the guest workspace because guest/ is out
   of this area's scope and the kernel workspace is built wholesale for the bare-metal target. The usermode
   engine config (`eo9-runtime::engine`) is not reused for precompiling: it pins the component-model async
   ABI and WAVE, both of which need wasmtime's `std` on the runtime side (see 8), so the seed engine is
   configured directly in xtask.
8. **Feasibility notes for the next steps (the honest blockers).**
   - *Component-model async ABI:* wasmtime's `component-model-async` feature requires `std` (and its `async`
     feature needs fibers). The eo9 WIT convention makes `main`/`configure` and all I/O async, so real eo9
     guests cannot run on the no_std runtime yet. Options: upstream no_std support for the CM-async host
     machinery, a sync execution profile for bare metal, or carrying patches. Needs a planner decision before
     kernel milestone 3 (run a real headless program).
   - *Fuel:* not exercised by the seed; `consume_fuel` is a plain tunable and should work no_std, but the
     resumable-task machinery eo9-runtime uses sits behind `async`/fibers — same constraint as above.
   - *Step 2, on-target codegen:* wasmtime's `cranelift` feature requires `std`, and wasmtime-environ's
     `compile` feature (wasm→CLIF translation + artifact assembly) requires `std` as well.
     `cranelift-codegen` 0.132 itself is `#![no_std]` with an optional `core` feature, so the code generator
     proper is plausible on metal; the work is in the layers above it (translation, object emission, and the
     relocation/linking path) plus a no_std artifact loader. Realistic routes, in the order worth trying:
     (a) port/feature-gate wasmtime-cranelift + wasmtime-environ/compile for no_std+alloc (upstream is
     receptive to no_std work), (b) drive cranelift-codegen directly with our own translator and loader
     (large, duplicates wasmtime), (c) keep host AOT as the bridge (what the spike does) and treat Pulley
     (wasmtime's portable interpreter backend, which is no_std-clean) as the interpreter-first fallback.
     This matches the plan's expectation that the compiler half is the risky part; step 1 required no
     patches at all.

