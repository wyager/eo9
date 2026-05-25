# 12 — Bare metal / QEMU images (`kernel/`)

## Scope
Bootable Eo9 images for AMD64, AArch64, rv64gc per the spec deliverable: boot, run a headless program, and
boot-to-eosh over serial. Arch order is confirmed (aarch64 → riscv64 → x86_64). The execution strategy is
still under discussion (PLAN.md Decisions item 2); this plan assumes the recommendation (host-side AOT + slim
no_std runtime) and must be revised if that changes.

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
- Execution strategy (the spike, do this first): run a host-AOT-compiled component on target using
  Wasmtime's no_std runtime support ("min-platform" style embedding). Success = hello-over-serial on
  aarch64 under QEMU. If the no_std embedding is not viable, stop and bring findings + alternatives
  (interpreter-first on metal, or a custom minimal runtime) to the planner — this is the single riskiest
  assumption in the whole plan.
- `xtask qemu <arch>`: build store image + kernel, launch QEMU with serial on stdio; used by plan 13.

## Dependencies
01, 04 (cross-compiled AOT artifacts + any runtime code reuse), 05, 06 (store image). Start after the
Phase-1 areas have their first milestones; the spike can start as soon as 04's compile path can cross-compile.

## Milestones
1. Spike: AOT hello over serial on aarch64/QEMU (I4).
2. Scheduler + multiple tasks + store image; headless program selection via kernel cmdline.
3. eosh over serial (boot-to-shell); riscv64 port; x86_64 port (I5).

## Decisions
(record here)
