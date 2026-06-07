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

9. **Owner ruling on execution strategy (recorded for the record).** True on-target codegen — the
   wasmtime-environ / cranelift-layer no_std port — is **required for the MVP**. It is its own workstream,
   scheduled after (1) the CM-async-on-no_std runtime work and (2) boot-to-shell on metal via host AOT.
   Pulley (wasmtime's interpreter backend) is acceptable only as a stopgap, not as the MVP execution
   strategy.
10. **Milestone 2 is done: the real `eo9-example-hello` runs on bare metal.** `cargo xtask build-kernel
   aarch64` now builds the hello example from the guest workspace, precompiles it (unmodified) for
   `aarch64-unknown-none` alongside the seed, and embeds both; at boot the kernel instantiates hello against
   its own root providers and calls its typed `main(name, excited)`, and the program's greeting — timestamped
   via the kernel's time provider — appears on serial followed by `outcome = success(greeted)`
   (instantiate + main ≈ 14 ms under TCG; 302 KB artifact). Arguments are fixed in the kernel for now;
   feeding them from the QEMU `-append` cmdline belongs to the "headless program selection" milestone.
11. **No CM-async port was needed for hello — because hello is sync at the canonical ABI level.** Despite
   the WIT convention that entrypoints are async, the merged `eo9-example-hello` component lifts `main`
   synchronously and lowers its text/time imports synchronously (it validates without the `cm-async`
   feature and contains zero async canonical built-ins). It therefore runs on the already-working no_std
   wasmtime configuration from the spike. This is *not* a sync-profile fallback: the artifact is byte-for-byte
   the merged component. The components that do use the async ABI today are readwrite, eosh, and the
   configure-style stubs (fs.memfs, time.frozen, entropy.seeded, net.deny, …) — i.e. everything needed for
   boot-to-shell and for composed environments — so the CM-async-on-no_std work (decision 8, first bullet)
   remains the gate for kernel milestones 3–4, just not for this one.
12. **Kernel root providers (hardware roots).** `eo9:text/text` → PL011 (both output streams go to the one
   serial console), `eo9:time/time` → PL031 RTC for wall-clock seconds + generic timer for sub-second,
   monotonic-now, and resolution, `eo9:entropy/entropy` → splitmix64 seeded from the cycle counter at boot
   (explicitly a stub, not a CSPRNG; virtio-rng is the later real source). The linking mirrors
   `eo9-runtime::link` (same resource/`default()` token shape, same WIT-shaped host types, same 64 KiB
   get-bytes cap); that crate itself is std-only, so the shapes are mirrored rather than reused, as small and
   structurally identical code. Async interface members (`text.read-line`, `time.sleep`) and the `configure`
   interfaces are not registered yet — they arrive with the CM-async port.
13. **The MMU is now on (flat identity map).** Cranelift-generated programs perform unaligned accesses,
   which fault on Device-nGnRnE memory with translation off, so the kernel builds a one-table identity map
   (low 1 GiB Device non-executable for MMIO, 1–2 GiB Normal write-back cacheable for DRAM) and enables the
   MMU, D-cache, and I-cache before running wasm. Known caveat for real hardware (not QEMU): publishing code
   into cacheable memory needs DC CVAU / IC IVAU maintenance in the code-memory publisher, and a W^X mapping
   policy is future work once the map gets finer-grained.
14. **CM-async under no_std: findings and recommended path (escalation).** What blocks it in wasmtime 45 is
   narrower than feared:
   - The `component-model-async` cargo feature hard-requires `std` and `futures/std` (still true on upstream
     `main` as of this writing). The `futures` items actually used (oneshot channels, `FuturesUnordered`,
     `StreamExt`) are all available with `futures/alloc` in no_std.
   - `wasmtime-fiber` already ships a `no_std` backend (`src/nostd.rs`): heap-allocated stacks, no mmap/guard
     pages, and the hand-written aarch64 stack-switch asm is shared with the unix backend — so the `async`
     feature's fiber layer is not a blocker.
   - The concurrent machinery's direct `std::` surface is small and mostly mechanical (`std::io::Read/Write`
     convenience impls on stream endpoints, a test-only `LazyLock`, `std::` paths that are really core/alloc).
   - The embedder side needs a tiny no_std executor (a block_on that polls `run_concurrent` with a no-op
     waker, interleaved with servicing hardware) — straightforward in the kernel.
   Recommended path: feature/cfg work that can be upstreamed (relax the feature graph, alloc-ify the
   concurrent module, cfg the io impls), prototyped against a `[patch.crates-io]` copy pinned to v45 only as
   the bridge while the upstream PR is in flight. Not attempted in this branch: it is a focused workstream of
   its own (it also unlocks fuel-sliced resumable tasks on metal, which eo9-runtime's task model needs).
   Planner input wanted on sequencing this against the shell milestone and on who drives the upstream PR.
15. **Kernel milestone 3 (first rung of the ladder) is done: the component-model-async machinery runs on
   no_std, and async eo9 guests run on bare metal.** Demonstrated under `cargo xtask qemu aarch64` by two new
   artifacts embedded behind the `wasm-async` feature: (a) `kernel/seed/sleepy.wat`, a hand-written async
   canary whose async-lifted `run` export awaits `eo9:time/time.sleep` for 50 ms against the kernel's generic
   timer and returns the measured elapsed time (serial shows `sleepy.run() -> ~51.5e6 ns elapsed across the
   await, ok`); and (b) the **unmodified `entropy.seeded` stub from the guest workspace** — a real SDK-built
   component whose `configure` export uses the async canonical ABI — configured with a seed on the kernel and
   then sampled twice (`get-u64` returns the exact SplitMix64 sequence for the seed). The seed canary and the
   real hello program continue to run unchanged. Boot-to-eosh (milestone 4) now needs exec/store plumbing,
   not new execution machinery.
16. **How CM-async-on-no_std was achieved (the vendored patch).** The kernel workspace patches wasmtime 45
   via `[patch.crates-io]` → `kernel/vendor/wasmtime` (kernel workspace only; host/guest workspaces keep the
   registry crate). The patch is the minimal, upstream-shaped relaxation anticipated in Decision 14: the
   `component-model-async` cargo feature no longer requires `std`/`futures/std`; the concurrent host
   machinery uses core/alloc and the crate's own `crate::sync`/`crate::hash_set` types (a `Mutex` was added
   to `sync_nostd.rs` mirroring its existing philosophy); the internal host-buffer cursor no longer uses
   `std::io::Cursor`; the `std::io::Read`/`Write` convenience impls are `cfg(std)`; the two
   oneshot-`Canceled` conversions construct errors explicitly; and the concurrent TLS slot goes through the
   custom platform layer (`wasmtime_concurrent_tls_get/set` — a new embedder-provided pair, same contract as
   the existing `wasmtime_tls_get/set`) when `std` is off. `wasmtime-fiber` needed no changes (upstream
   already ships the no_std backend with the aarch64 stack switch). Every change is listed in
   `kernel/vendor/README.md`; upstreaming should be offered (the diff is small and behavior-preserving for
   std builds), at which point the vendor copy is dropped. Who drives the upstream PR is the planner's call.
17. **Execution model on metal for async guests.** The engine enables `wasm_component_model_async`,
   `_async_stackful`, and `_more_async_builtins` (matching xtask's precompile config — these are
   compile-relevant, so all embedded artifacts are precompiled with the same flags, and
   `concurrency_support` is on for both). Instantiation and calls go through `instantiate_async`/`call_async`
   driven by `wasm::block_on`, a single-threaded polling executor with a 30 s watchdog; the kernel's
   `time.sleep` future re-arms its waker each poll, so the busy poll is the only scheduling needed until the
   GIC/timer-interrupt work lands (then it becomes wait-for-interrupt). The root providers now register the
   async interface members: `time.sleep` (a real await on the generic timer) and `text.read-line` (reports
   end-of-input — no UART RX path yet). Fuel metering on metal is still not enabled; it arrives with the
   scheduler/multi-task milestone.
18. **Still open for kernel milestone 3's "supporting pieces" (not done on this branch):** the baked-in
   read-only store image and headless program selection via the QEMU `-append` cmdline (needs a minimal
   /chosen/bootargs FDT walk and capturing x0 in boot.rs), and adopting `eo9-sched` once more than one task
   runs at a time. These are plain plumbing with no open design questions and are the natural next kernel
   change before boot-to-eosh.
19. **The milestone-3 "supporting pieces" are done: a baked-in read-only store image and command-line
   program selection.** `cargo xtask build-kernel <arch>` assembles `store.img` — magic `EO9STOR1`, entry
   count, then per entry `name + component bytes + host-AOT artifact`, keyed by the same shell names the
   usermode store seeds (`hello`, `entropy.seeded`, `eosh`, …; list in xtask's `KERNEL_STORE_COMPONENTS`,
   currently eosh, the four examples, entropy.seeded, time.frozen) — and the kernel embeds and parses it
   (src/wasm/store.rs). The kernel command line selects what to run: `cargo xtask qemu aarch64
   program=<name> [arg=value …]` passes everything after the arch as QEMU `-append`; the kernel reads
   `/chosen/bootargs` with a minimal FDT walk (src/fdt.rs — the boot stub preserves x0, and the parser falls
   back to probing the DTB at the base of RAM since ELF entry does not get it in x0), runs the named entry
   headless against the kernel root providers with `key=value` arguments matched against `main`'s named,
   typed parameters (scalar types parsed; richer types reported as unsupported), prints the outcome, and
   powers off. Without `program=` the default demo sequence still runs. Verified: `program=hello
   name="bare metal" excited=true` → `success(greeted)`, `program=cruncher seed=9 rounds=200000` →
   `success(digest(…))`.
20. **Serial input works: PL011 receive + a real `text.read-line`.** `uart::try_get_byte` polls the RX FIFO
   and the `read-line` provider is a polled future that echoes printable characters, handles
   backspace/DEL, ends the line on CR/LF, and treats Ctrl-D on an empty line as end of input — the same
   busy-poll-with-self-waking shape as `time.sleep`, to be replaced by GIC-driven wakeups later.
21. **What remains for kernel milestone 4 (boot-to-eosh), with the surveyed requirements.** eosh imports
   `eo9:exec/{component-algebra, compile, task}`, `eo9:text/text`, and `eo9:fs/fs`; its concrete call
   surface (guest/eosh/eosh/src/lib.rs) is: fs `open-exec`/`exec-size`/`exec-read` of `/bin/<name>.wasm`
   (plus `stat`/`open`/`read` for the optional session manifest) using `eo9:io/buffers`, algebra
   `load`/`save`/`describe` (compose/extend/restrict/rename/configure only for algebra expressions),
   `compile`, and task `spawn` + `wait`. The planned kernel implementation (not started on this branch):
   an fs provider serving `/bin` read-only from the store image plus the io-buffers interface; algebra
   `load`/`describe` backed by metadata precomputed by xtask (content-hash keyed), `compile` as a lookup
   from content hash to the baked-in AOT artifact (a real compile is the on-target-codegen rung; unknown
   bytes get a clear codegen error), and `spawn`/`wait` instantiating the artifact against the kernel root
   providers with the child driven from the embedder loop (wasmtime forbids re-entering `run_concurrent`
   from a host function, same as usermode), plus a small scalar WAVE parse/render for arguments and
   outcomes. GIC/timer interrupts, fuel on metal, and eo9-sched adoption are also still open — the executor
   remains a polling loop.
22. **Kernel milestone 4 is done: the unmodified eosh boots as the bare-metal shell.** `cargo xtask qemu
   aarch64` (no arguments, or `program=eosh`) now boots to an interactive `eosh>` prompt on the serial
   console; programs from the baked-in store run as children with WAVE-rendered outcomes (`hello`,
   `cruncher`, `outcomes` — including the failure path), `env` shows the session's capability picture from
   a kernel-written manifest, `describe`/`imports` work from the store metadata, and `exit` powers the
   machine off. The original demo sequence stays reachable via the bare `demo` cmdline token, and
   `program=<name> [arg=value …]` headless selection is unchanged. Note the behavioral consequence: the
   no-argument boot is now interactive and does not power off by itself; automated runs should use `demo`
   or `program=…`.
23. **Store image v2 carries component metadata (and the format is versioned).** The image magic is now
   `EO9STOR2`; each entry carries, alongside the component bytes and the host-AOT artifact, a plain-text
   metadata block — the component's `describe` output (kind, imports, exports, `main`'s arg specs) computed
   at image-assembly time by xtask through the same `eo9-component` crate the usermode runtime uses (xtask
   gained that workspace dependency). The kernel cannot parse component binaries itself before on-target
   codegen, so `describe` on metal replays this metadata. Hardening from the last review landed here too:
   the parser caps the declared entry count before allocating, and `read-line` bounds its line buffer.
24. **How the shell session is provided (src/wasm/shell.rs, shellfs.rs, shellexec.rs, wave.rs).** eosh runs
   unmodified against: (a) **fs** — a read-only view of the store image (`/bin/<name>.wasm` per entry plus
   the `/session` manifest in the `eo9-session 1` format), with the same WIT shapes, owned-buffer
   round-trip, and buffer-table bounds as the usermode runtime; writes answer `read-only`. (b) **exec** —
   `load` recognises exactly the baked-in components (matched by content), `describe` replays the image
   metadata, `compile` is a lookup that deserializes the baked-in artifact (a provider answers
   `not-a-binary`), and `spawn` instantiates the artifact against the kernel root providers
   (text/time/entropy — children never receive fs or exec, the usermode child policy), binds `main`'s
   arguments with a small kernel-side WAVE codec (scalars, strings, enums, options; richer shapes are
   rejected with a clear message), and parks the child in a registry. The algebra combinators
   (`$`/`&`/`only`/`rename`/`configure`) fail with an explicit "not implemented on the bare-metal kernel
   yet" error — they need the component tooling that arrives with on-target codegen. (c) **the drive
   loop** — the kernel polls eosh's `main` and, between polls, every running child once (the bare-metal
   counterpart of usermode children executing inside their parent's resume; wasmtime forbids re-entering
   the event loop from a host function). `wait`/`runnable`/`kill` observe the registry; `resume` is
   unsupported exactly as in usermode (E5). There is no watchdog on the interactive session — it is paced
   by the user at the console.
25. **Milestone-4 follow-ups (deliberately not in this change).** GIC bring-up (the executor and
   `read-line` still busy-poll), fuel metering for children (`consume_fuel` is compile-relevant, so it must
   land together with re-precompiled artifacts and the scheduler work), eo9-sched adoption (the registry
   handles multiple children but eosh's flow is sequential today), linking `eo9:io/buffers` and the
   types-only `eo9:fs/types` for children (the always-available convention; today a child that imports
   them, e.g. `readwrite`, is refused at instantiation with the linker's missing-import message rather than
   the friendlier missing-fs story), session manifests for headless `program=` runs, and the riscv64/x86_64
   ports. On-target codegen remains the next rung and is what unlocks composition in the bare-metal shell.

26. **On-target codegen — checkpoint 1: the fork surface is mapped and far smaller than feared (no_std
    cranelift + wasmtime compile layers).** Owner ruling (2026-05-26): fork cranelift now rather than wait
    for upstream's in-flight no_std work; keep forks under `kernel/vendor`, kernel-workspace-only, behind a
    new off-by-default `wasm-codegen` cargo feature so `cargo xtask ci` (which builds the kernel workspace
    featureless — wasmtime isn't compiled) stays untouched. Survey of the wasmtime 45 / cranelift 0.132
    compile path, by crate:
    - **cranelift-codegen 0.132 — no source edits needed; controlled by features.** Already `#![no_std]`
      with a `core` feature and a hashbrown fallback for `HashMap`/`HashSet`; `extern crate std` is
      `#[cfg(feature = "std")]`; the only non-`core`/`alloc` uses are `souper_harvest` (behind
      `#[cfg(feature = "souper-harvest")]`) and `timing`'s `std::time::Instant` (behind
      `#[cfg(feature = "timing")]`). Building with `default-features = false` + `["core", "host-arch",
      "pulley"]` (no `std`/`timing`/`souper-harvest`) is no_std-clean. Its `build.rs` runs ISLE codegen at
      build time on the host — fine. **Do not vendor it.** Same expectation for the small cranelift
      sub-crates (frontend, entity, bforest, bitset, control, assembler-x64): no_std-capable via features.
    - **wasmtime-environ 45 — vendor + edit (bounded).** Already `#![no_std]`, but its `compile` feature
      requires `std` and pulls alloc-friendly deps deliberately (`object/write_core`, `gimli/write`,
      `wasm-encoder`, `wasmprinter`). The `compile` module's residual `std::` (~79 lines / 21 files) is
      mostly mechanical core/alloc swaps (`std::ops::Range`→`core::ops::Range`, `std::mem`→`core::mem`,
      `std::borrow::Cow`→`alloc::borrow::Cow`, `std::collections::HashMap`→the crate's hashbrown alias,
      `std::sync::Arc`→`alloc::sync::Arc`, `std::any::Any`→`core::any::Any`). The genuine std touchpoints to
      resolve are few — notably `std::path::PathBuf` in `compile/module_environ.rs` (module-name/debug
      paths during translation; replace with an `alloc` string or feature-gate). Work: drop `std` from the
      `compile` feature and fix those residuals.
    - **wasmtime-internal-cranelift 45 — vendor + edit (the main work crate).** Not yet `#![no_std]`; ~43
      `std::` lines / 16 files. Work: add `#![no_std]` + `extern crate alloc`, convert std→core/alloc, and
      drive `object`/`gimli` through their alloc-only write paths. This is where the bulk of the elbow grease
      is; nothing here looked fundamentally std-bound on inspection (it's the `Compiler` impl glue, not OS
      services).
    - **wasmtime (already vendored) — extend the existing patch.** Add the `cranelift`/`compile` features to
      the kernel build path and make `cranelift` not transitively force `std`. object/gimli/target-lexicon/
      wasmprinter/wasm-encoder are controlled via features in the dependents, not vendored.
    Net vendor set for the rung: **two new crates** (wasmtime-environ, wasmtime-internal-cranelift) plus the
    existing wasmtime patch — cranelift proper stays from the registry behind feature flags. cranelift emits
    **native aarch64** (not Pulley); Pulley is only a diagnostic fallback that would skip the code-publication
    work (it needs the same compile crates, so it does not avoid this fork). Recommended sequence:
    feature-degate → no_std-ify the two crates → compile a trivial module in-kernel under QEMU (checkpoint 2,
    + real code publication, Decision 27) → full component (checkpoint 3) → wire the shell's `compile`/`$`/`&`
    (checkpoint 4). Risk note: a 45→newer wasmtime bump would re-touch the CM-async ABI constants the binder
    and kernel mirror, so this fork should ride the same pin (45) until a deliberate bump.

27. **On-target codegen — code publication / cache maintenance (checkpoint 2 runtime side).** Cranelift in
    the kernel emits real aarch64 machine code into a heap allocation that must be made coherent before it is
    executed. The current `BareMetalCodeMemory::publish_executable` only issued `dsb ish; isb` (correct for
    QEMU TCG, which keeps I-fetch coherent with stores, and for the flat everything-executable identity map).
    This change makes it correct for real hardware: clean the D-cache to the point of unification by
    `CTR_EL0.DminLine` (`dc cvau`), `dsb ish`, invalidate the I-cache to PoU by `CTR_EL0.IminLine`
    (`ic ivau`), `dsb ish; isb`, over the published range. W^X (mapping code read-only/executable and data
    non-executable) remains a separate MMU hardening item (Decision 3); cache maintenance is the part that is
    required even under the current flat map and is independently correct, so it lands now (it also already
    runs for the deserialized AOT artifacts). The publisher's `required_alignment` stays 1 until W^X
    introduces page granularity.

28. **On-target codegen — checkpoint 2 in progress: the real blocker is feature unification, not the
    crates themselves.** Decision 26's per-crate survey was right that `cranelift-codegen` 0.132 and the
    cranelift sub-crates build no_std via features — confirmed: with the dependency graph fixed (below), the
    whole codegen backend (cranelift-codegen, cranelift-frontend, cranelift-entity/bitset/control, the ISLE
    output, gimli read/write, object write_core) compiles clean for `aarch64-unknown-none`. What Decision 26
    missed is that several *dependents* hardcode `std` in their dependency feature lists, so Cargo feature
    unification drags `std` (and std-only crates) back onto the no_std target no matter what the leaf crates
    support. The vendor set is therefore **five crates**, not two:
    - `wasmtime` (already vendored) — `cranelift` feature no longer pulls `std`.
    - `wasmtime-environ` — `compile` feature no longer pulls `std`; `wasm-encoder` taken `default-features =
      false`; `wasmprinter` dropped from `compile` (it pulled `termcolor`, which is std-only — the single
      `wasmprinter::print_bytes` use was a Trace-level adapter-module dump, replaced with a byte-count log);
      the `compile`/`fact` modules' mechanical `std::`→`core`/`alloc`/`hashbrown` swaps are done.
    - `wasmtime-internal-cranelift` — `#![no_std]` + `extern crate alloc` added; `cranelift-codegen` taken
      `core` (not `std`)+`unwind`+`host-arch`; `cranelift-frontend` `core`; `gimli` `read`-only; `object`
      `write_core`; `itertools` `use_alloc`; `thiserror` `default-features = false`; `cranelift-native` made
      optional + the host-flag-inference call gated (the kernel always specifies its target triple, so it is
      never needed — and cranelift-native is std-only).
    - `cranelift-frontend` and `wasmtime-internal-unwinder` — vendored *only* to change their
      `cranelift-codegen` dependency from `features = ["std", …]` to the `core` profile. This is the crux:
      either of those edges alone forces `cranelift-codegen/std`, which in turn pulls `gimli/std` and
      `cranelift-control/fuzz` → `arbitrary` (a std crate) onto the target and breaks the build. Likewise
      `thiserror`'s `std` default arrived via `wasmtime-internal-cranelift`.
    All of the above is committed and verified not to disturb the existing builds (`wasm-codegen` is
    off-by-default; `cargo xtask ci`, the featureless kernel, and `build-kernel` with the runtime features
    stay green). The codegen build (`cargo build -p eo9-kernel --target aarch64-unknown-none --features
    wasm-codegen`) now proceeds through the entire cranelift backend and into `wasmtime-environ`'s own source.
    **Remaining to reach checkpoint 2 (precise punch list):**
    a. The `clif_dir`/`emit_clif` CLIF-dump debugging feature is the last `std::path` touchpoint, and it is
       genuinely cross-crate: `CompilerBuilder::clif_dir(&path::Path)` (environ `compile/mod.rs`), the impl +
       `Option<PathBuf>` field + the `std::fs` write block (`wasmtime-cranelift` `builder.rs`/`compiler.rs`),
       and `Config::emit_clif`/the `clif_dir` field (`wasmtime` `config.rs`). Gate the whole feature behind a
       `std`/`compile-debug` cargo feature (off for the kernel), or switch the path types to `&str`/`String`
       and gate only the `std::fs` write. Also `ModuleTranslation.path: Option<PathBuf>` (environ
       `compile/module_environ.rs`) → `Option<String>` or gate.
    b. `wasmtime-internal-cranelift`'s own source no_std-ification: ~43 `std::` lines across ~16 modules,
       plus prelude threading — each module needs `use crate::*;` (the crate root now re-exports
       `wasmtime_environ::prelude::*`) so `Vec`/`String`/`Box`/`format!`/`vec!` resolve under `#![no_std]`,
       and `object`/`gimli` driven through their alloc-only `write_core`/`write` paths.
    c. The in-kernel demo + verification: a `wasm-codegen`-gated `kernel/src/wasm/codegen.rs` that builds a
       compiling engine (target `aarch64-unknown-none`, the same compile-relevant flags as xtask's
       `precompile_for_kernel`, plus the existing `BareMetalCodeMemory` publisher — Decision 27), compiles a
       trivial module from embedded wasm bytes via `Module::new` on-target, publishes it, and calls it. That
       run under QEMU is checkpoint 2; the full component path + wiring `compile`/`$`/`&` into the shell is
       checkpoint 3–4.
    Std-crate intruders to watch for as the build advances (all resolved so far by `default-features = false`
    at the offending edge): `arbitrary` (via `cranelift-control/fuzz`), `termcolor` (via `wasmprinter`),
    `wasm-encoder` `std` default, `thiserror` `std` default.

29. **On-target codegen works — checkpoints 2 and 3 are done: the kernel compiles a component with Cranelift
    on the machine and runs it.** Under `cargo xtask qemu aarch64` (in the `demo` sequence) the kernel now
    prints, after the existing deserialize/async demos:
    ```
    wasm codegen: compiling a 298 byte component on-target with Cranelift…
    wasm codegen: compiled on-target in ~83 ms
    wasm codegen: hello() -> "Hello from a WebAssembly component on bare-metal Eo9!"
    wasm codegen: add(17, 25) -> 42
    ```
    i.e. the seed component is handed to `Component::new` (not `Component::deserialize`), Cranelift emits
    native aarch64 into a heap allocation, the cache-maintenance publisher (Decision 27) makes it executable,
    and the resulting code runs and returns correct results. This retires the plan's single riskiest
    assumption (that Cranelift runs under the kernel's `no_std + alloc`). The seed is a real component, so
    this is checkpoint 3 as well.
    - **Implementation:** `kernel/src/wasm/codegen.rs` (behind `wasm-codegen`) plus `new_engine` setting
      `target("aarch64-unknown-none")` and the OS-less tunables. xtask ships the un-precompiled seed wasm
      (`EO9_SEED_WASM`) and enables `wasm-codegen` in `build-kernel`. The no_std source port of the five
      vendored crates is detailed in `kernel/vendor/README.md`; the headline subtleties were the
      hashbrown-vs-std `Equivalent`/`get` and `#[may_dangle]` dropck differences, a local no_std `Mutex` for
      the compiler-context pool, and switching the `clif_dir`/`Path` debug surfaces to `String`/`&str` with
      the actual filesystem writes gated behind `std`.
    - **Native-host check (the gotcha):** linking the compiler makes wasmtime run its
      `check_compatible_with_native_host` on *every* engine, including the deserialize ones. It passes
      because the kernel is built **for** `aarch64-unknown-none`, so `target_lexicon::Triple::host()` equals
      the explicitly-set target; the OS-less tunables (no signals, no VM reservations/guards, no CoW) are
      what the rest of that check verifies. `cranelift-native` stays disabled (host CPU inference needs
      `std`), which is why the target must be named rather than inferred.
    - **Numbers:** kernel image **7.8 MB → 16.4 MB** with `wasm-codegen` on (Cranelift + the compile layers
      add ~8.6 MB). On-target compile of the ~300-byte seed component ≈ 83 ms under QEMU TCG (vs ≈ 28 ms to
      *deserialize* the host-AOT artifact of the same component — codegen is the slower path, as expected,
      but it is real and removes the host-AOT dependency). Determinism not yet measured bit-for-bit; the
      seed compiles to a fixed result across runs but a cross-run artifact-hash comparison is future work.
    - **CI stays lean:** `wasm-codegen` is off by default, so `cargo xtask ci` (featureless kernel) does not
      compile Cranelift; it stays green. `build-kernel`/`qemu`/`demo`/`program=`/interactive-eosh all work.
    - **Next (checkpoint 4):** wire the shell's `compile` to actually compile (today it deserializes a baked
      AOT artifact) and enable `$`/`&` composition in the bare-metal eosh (currently a clean "not
      implemented on the bare-metal kernel yet" error). That makes on-target codegen reachable interactively
      rather than only in the boot demo. Then: optional fuel-on-metal and a determinism check.

30. **Checkpoint 4 — survey: interactive composition is a *second* multi-crate no_std fork (the algebra
    layer), comparable to the codegen rung, not a wire-up.** The codegen work made the kernel *compile*
    component bytes on-target; what the shell still cannot do is *produce the fused component bytes* that a
    composition expression (`$`/`&`/`only`/`configure`) denotes. In usermode that fusion is
    `crates/eo9-component` (`compose`/`extend`/`restrict`/`rename`/`configure`), which is byte-level
    component composition — not anything wasmtime does at instantiation. To run `entropy.seeded $ cruncher`
    on metal honestly (reusing the real algebra, per the brief — no kernel-only re-implementation, no
    host-prefused lookups, no runtime instance-linking shortcut), that algebra must build no_std and link
    into the kernel. There is **no honest partial that delivers the user-visible goal without it**: the
    shell's `compile` already has a working codegen path (Decision 29), but it has nothing fused to compile
    until the algebra produces a fused component, and a baked program already runs via the AOT fast path, so
    routing plain programs through codegen adds latency without function. Hence this checkpoint is gated on
    the algebra fork below; this Decision records the survey so the next session executes rather than
    re-discovers. No code landed this pass (the worktree stays building; nothing half-vendored was
    committed). **Magnitude: similar to or larger than Decisions 26–29.**

    - **What `eo9-component` needs (`compose`/`extend` → `wac_graph::CompositionGraph`):** the dependency
      closure is wasmparser, wasm-encoder, wit-parser, wasm-wave, wac-types, wac-graph, wit-component, plus
      anyhow, id-arena, indexmap, semver, bitflags, serde, thiserror, log, petgraph, wasm-metadata.
    - **Already no_std-capable (just enable the `std=false` feature + resolve feature-unification, the same
      gotcha as Decision 28):** wasmparser, wasm-encoder, wit-parser, wasm-wave (all four have an `std`
      feature and `#![no_std]`), semver, bitflags, anyhow, id-arena, indexmap, serde, log, and thiserror
      (2.x supports no_std via `core::error::Error`). `eo9-component`'s own only-std use is
      `std::collections::BTreeMap` — trivial.
    - **Vendor + de-std (mechanical, like Decision 28):** `wac-types` (~5.2k lines, 8 files; deps all
      no_std-capable — the cleanest), `wac-graph` (~3.0k lines, 4 files; almost no `std::`/io itself), and
      `wit-component` (~14k lines, 15 files — but its `wat`/`wast` deps are optional features we do NOT
      enable, so the text-format parser drops out; composition needs only the binary encoder path).
    - **Two genuine sub-blockers (need a decision, not just elbow grease):**
      1. **`wasm-metadata` cannot be ported — it must be excised.** It is a dep of both `wac-graph` and
         `wit-component`, and it pulls `clap`, `flate2`, `url`, `serde_json`, `spdx`, `auditable-serde` —
         none no_std. It is used only to merge the `producers`/`metadata` custom sections, which Eo9 does not
         need on metal. Plan: feature-gate or delete the metadata-merging code paths in the vendored
         `wac-graph`/`wit-component` so `wasm-metadata` leaves the closure. Invasive (removing a code path)
         but bounded; document in `kernel/vendor/README.md`.
      2. **`petgraph` (0.6) no_std is unverified.** `wac-graph` uses it for the composition graph
         (topological ordering of instantiation). Confirm `default-features=false` gives the algorithms
         `wac-graph` actually calls under no_std; if not, vendor/replace (the used surface is small — a
         DAG topo-sort — so a hand-rolled replacement in vendored `wac-graph` is a fallback).
    - **After the algebra builds no_std:** make `eo9-component` (or a thin kernel-side reuse of it) linkable
      from the kernel; add `compose`/`extend`/`restrict`/`rename`/`configure` to `shellexec.rs` calling the
      real algebra on the `/bin` store components; change `compile` so a *fused* component (no baked
      artifact) goes through `codegen.rs` (Decision 29) while a plain baked program keeps the AOT fast path;
      run the fused result against the kernel roots with the existing child/capability rules. Then test
      `entropy.seeded $ cruncher …` interactively and confirm capability containment still holds.
    - **`build-kernel` already enables `wasm-codegen`** (Decision 29), so the compile half is ready; this
      checkpoint adds an algebra feature/closure on top. Keep the featureless CI build lean and green.
    - **Alternatives if the full fork is not wanted now (recorded, not recommended):** (a) defer interactive
      composition and ship the rest of the metal MVP (the shell already runs baked programs, and on-target
      codegen is proven in the boot demo); (b) a minimal hand-rolled binary component composer in the kernel
      using only wasmparser+wasm-encoder for the `provider $ consumer` case — rejected here because it is a
      kernel-only re-implementation that would drift from the spec/usermode sealing+type-checking semantics,
      which the brief forbids.

31. **Checkpoint 4 — step 1 done: `eo9-component` is now `no_std`-capable in its own source, behind a
    default-on `std` feature; usermode is byte-identical.** The crown-jewel algebra crate is made
    `#![cfg_attr(not(feature = "std"), no_std)]` + `extern crate alloc` IN PLACE (single-source, no second
    copy): its entire std surface was just five lines — `std::error::Error`/`std::fmt` (→ `core`),
    `std::borrow::Cow` and two `std::collections::BTreeMap` (→ `alloc`) — plus prelude types
    (`Vec`/`String`/`ToString`) now imported from `alloc` per module and `format!`/`vec!` via
    `#[macro_use] extern crate alloc`. A new `[features] default = ["std"]` makes every host build identical
    to before; `std` forwards to the `std` feature of the four leaf deps that gate it (`wasmparser`,
    `wasm-encoder`, `wit-parser`, `wasm-wave`). Verified: `cargo build -p eo9-component --no-default-features`
    compiles the crate as `no_std` (against std-built deps on the host — proving the source is core/alloc
    clean), and `cargo xtask ci` stays green (usermode unaffected). The dependency closure is **not** yet
    `no_std`, so the kernel cannot link the algebra yet — that is the remaining work below.

    **Sharpening of Decision 30 (the dep work is larger than "3 mechanical vendors"):** the leaf crates are
    not all "just flip `std=false`". The exact features `eo9-component` uses hardcode `std`:
    - `wit-parser`'s `decoding` feature is `["std", "dep:wasmparser"]` — and `eo9-component` needs `decoding`
      (`wit_parser::decoding::decode` in describe.rs/configure.rs). So **wit-parser must be vendored** (or its
      `decoding` feature edited) to make `decode` available without `std`, not merely feature-flagged.
    - `wasm-wave`'s `wit` feature is `["dep:wit-parser", "std"]` — `eo9-component` uses `wasm_wave::wasm`/
      `value`; confirm whether the `wit` feature is actually required (it may not be, since only `value`/`wasm`
      are imported) — if not, plain `wasm-wave` (no `wit`) is `no_std` via its `std` feature; if so, vendor it.
    - `wasmparser`/`wasm-encoder` have clean `std` features but their *default* sets (component-model,
      validate, hash-collections, …) must be re-added as always-on when switching to `default-features =
      false`, and the feature-unification trap (Decision 28) applies: every consumer in the kernel graph
      (incl. the vendored `wasmtime-environ`, which also uses `wasmparser`) must agree on `default-features =
      false`, or std unifies back on.
    So the corrected vendor/patch set for the algebra is: **vendor + de-std** `wac-types`, `wac-graph`,
    `wit-component`, and `wit-parser` (for `decoding`), **probably** `wasm-wave` (TBD on the `wit` feature),
    plus `default-features = false` feature surgery on `wasmparser`/`wasm-encoder` at every kernel-graph edge;
    and **excise `wasm-metadata`** from vendored `wac-graph`/`wit-component` (it pulls clap/flate2/url/
    serde_json/spdx/auditable-serde — confirmed in the dep tree of the `--no-default-features` build above),
    and **verify/replace `petgraph` no_std** in `wac-graph`. The eo9-component manifest's per-dep
    `default-features = false` + always-on-feature lines are deferred to that session because they are only
    meaningful once the vendored no_std deps exist (doing them now, with std deps, would gain nothing and risk
    usermode); the `std` feature hook is in place for them to hang off.

    **Remaining sequence (unchanged from Decision 30, now with the corrected dep set):** vendor + de-std the
    crates above under `kernel/vendor` (via the kernel `[patch.crates-io]`); set the eo9-component deps
    `default-features = false` with std forwarded; have `eo9-kernel` depend on `eo9-component`
    (`default-features = false`) via a cross-workspace path dep (the kernel `[patch]` will redirect the
    vendored deps in eo9-component's own closure, exactly as it already does for `wasmtime-environ`); then
    wire `compose`/`extend`/`restrict`/`rename`/`configure` into `shellexec.rs` over the `/bin` store
    components, route a *fused* component through `codegen.rs` (Decision 29) while plain baked programs keep
    the AOT fast path, run against the kernel roots with the existing child/capability rules, and test
    `entropy.seeded $ cruncher …` interactively with containment intact.

32. **Checkpoint 4 — closure analysis (Decision 31 sharpened): it is a TWO-VERSION-FAMILY fork, and there is
    no intermediate building checkpoint.** Inspecting `cargo tree -p eo9-component --no-default-features`
    against the registry sources nails the real shape, which is materially larger than Decision 31 assumed:

    - **Two parallel version families in one closure.** `wac-graph` 0.10.0 and `wac-types` 0.10.0 (the newest
      published — only 0.9/0.10 exist) **hard-pin the 0.247 family**: `wasmparser = "0.247"`,
      `wasm-encoder = "0.247"`, `wasm-metadata = "0.247"`. `eo9-component` itself pulls the **0.250 family**:
      `wasmparser`/`wasm-encoder`/`wit-parser`/`wit-component`/`wasm-wave` 0.250 (and `wasm-metadata` 0.250 via
      wit-component). `^0.247` does not accept 0.250, so a naive vendor would have to de-std BOTH families'
      `wasmparser` + `wasm-encoder` (and excise BOTH `wasm-metadata` versions). **Recommended collapse:** since
      we vendor `wac-types`/`wac-graph` anyway, bump their `wasmparser`/`wasm-encoder` deps to the 0.250 family
      in the vendored manifests so the whole closure is single-family — at the cost of adapting `wac-types` to
      any wasmparser/wasm-encoder API drift across 0.247→0.250 (3 minors; the surface wac-types touches is the
      type/section reader, so expect a handful of call-site fixes, not a rewrite). This roughly halves the
      de-std surface and removes the duplicate `wasm-metadata`. Do this first.

    - **New sub-blockers not in Decision 30/31:**
      - **`thiserror` 1.x is std-only.** `wac-graph` (and wac-types) use `thiserror = "1.0.x"` (edition 2021,
        no `core::error::Error` path). no_std `thiserror` only exists in 2.x. Fix in the vendored wac crates:
        bump to `thiserror = "2"` (`default-features = false`) — the derive surface is compatible — or drop the
        few error enums to hand-written `core::fmt`/`core::error::Error` impls.
      - **`wasm-wave` is nearly free, with one variable.** Its `std` feature is empty (`std = []`), `thiserror`
        is already 2.x, and `logos` is already `default-features = false`. eo9-component uses `wasm_wave::wasm`/
        `value`, NOT the `wit` module, so **build wasm-wave without the `wit` feature** (drops its `wit-parser`
        edge and the `std` it forces). The one thing to verify is that `logos` 0.14 (`default-features=false`,
        `export_derive`,`forbid_unsafe`) is genuinely no_std on our toolchain; if not, wasm-wave must be
        vendored to gate/replace the WAVE lexer (the parser is only needed for `configure`-constant + arg
        parsing, which the kernel does want).
      - **`serde_json` / `serde` likely drop out.** `wit-parser`'s `decoding` path (what eo9-component needs —
        `wit_parser::decoding::decode`) is binary, not serde; if the vendored `wit-parser`/`wit-component`/`wac`
        crates are built with their `serde` features OFF, `serde_json` (and its `zmij`/`itoa`/`memchr`) leave
        the closure. Confirm nothing in the used path requires `serde`.
      - **`petgraph` 0.6.5** no_std still to verify (`default-features=false`); the used surface in wac-graph is
        a DAG topo-sort — hand-roll in the vendored wac-graph if needed.

    - **`wasm-metadata` excision still required** (now only the 0.250 copy after the family collapse): it pulls
      `flate2`/`url`/`idna`/`icu_*`/`serde_json`/`spdx`/`auditable-serde` — none no_std. Feature-gate/delete the
      producers/metadata-merging code paths in vendored `wit-component` (and wac-graph if it still references
      it) so it leaves the closure.

    - **No partial building checkpoint exists.** None of the vendored crates (`wac-types` → `wac-graph` →
      `wit-component`, all sharing `wasmparser`/`wasm-encoder`) builds no_std until the *whole* closure is
      no_std and the kernel `[patch.crates-io]` redirects every member at once; and the kernel cannot link
      `eo9-component` until then. So this is one all-at-once session (committing only when the closure compiles
      for `aarch64-unknown-none`), not a sequence of independently-green commits. Estimated size ≈ the codegen
      rung (Decisions 26–29) given the family collapse + the wac-types API adaptation.

    **Concrete ordered recipe for the next session:**
    (a) Vendor `wac-types`, `wac-graph`, `wit-component`, `wit-parser` under `kernel/vendor`; add their
    `[patch.crates-io]` entries to `kernel/Cargo.toml`.
    (b) In vendored `wac-types`/`wac-graph` manifests, bump `wasmparser`/`wasm-encoder` to 0.250 and
    `thiserror` to 2.x; fix any wac-types call-site drift; excise `wasm-metadata`.
    (c) De-std all four vendored crates (`#![no_std]` + `extern crate alloc`, core/alloc swaps, prelude
    `use crate::*;`), turn off `serde`/`wat`/`wast`/`wit` features, verify/replace `petgraph`, verify `logos`.
    (d) Set eo9-component's per-dep `default-features = false` with `std` forwarding (the hook is already in
    its manifest from Decision 31); add `eo9-kernel → eo9-component` (`default-features = false`) cross-
    workspace path dep so the kernel `[patch]` redirects eo9-component's closure (as it does for
    wasmtime-environ). **Checkpoint A** = featureless `cargo xtask ci` green (usermode unchanged) AND
    `cargo xtask build-kernel aarch64` links eo9-component under `wasm-codegen`.
    (e) Then wire `$`/`&`/`only`/`configure` into `shellexec.rs` over the `/bin` store and route fused
    components through `codegen.rs` (Decision 29) — **Checkpoint B**, the interactive on-target composition.

    This session committed only this analysis (docs-only; the tree stays green) rather than a half-vendored
    non-building closure — same discipline as the codegen rung's survey checkpoints.

33. **Checkpoint 4 — execution begun: wit-parser green, wac-types de-std'd, and the family-collapse plan
    needs revisiting.** Two crates vendored under `kernel/vendor`; both are inert (not yet in any `[patch]`
    table), so `cargo xtask ci` is unaffected and the branch HEAD still builds.

    - **`wit-parser` 0.250 — done and standalone-green for `aarch64-unknown-none`** (commit
      `kernel/vendor: vendor + de-std wit-parser`). It was already `#![no_std]`; the only real work was that
      the `decoding` feature hard-required `std`. Fix: drop `std` from the `decoding` feature, gate the
      streaming `Read`-based `from_reader`/`decode_reader` behind `std`, and add a no_std `from_bytes` that
      drives the parser with `eof = true` over a complete slice (`decode(&[u8])` — what eo9-component calls —
      now routes through it). Also de-hardcoded `wasmparser/std` from the dep feature list and forward it via
      wit-parser's own `std` feature (the D28 unification fix). Verified with a temporary `[workspace]` table
      for the standalone target build, then removed.

    - **`wac-types` 0.10 — de-std'd, but bumping it to the 0.250 family turns out to require a *type-decoder
      port*, not call-site drift (this revises D32's "handful of fixes").** The de-std itself was
      straightforward and is committed (WIP): `#![no_std]`, `hashbrown` for `HashMap`/`HashSet`, crate-level
      `IndexMap`/`IndexSet` aliases with a no_std default hasher (the wit-parser pattern), core/alloc swaps,
      `Package::from_file` std-gated, `anyhow` bumped to 1.0.100 (older anyhow lacks the no_std
      `core::error::Error` `From` impl, which is why `?` on `BinaryReaderError` failed), and deps set to
      `default-features = false` with a forwarding `std` feature. **The blocker:** wasmparser 0.247→0.250
      reshaped the component type model — a component's imports/exports are now
      `IndexMap<String, ComponentItem>` (was `IndexMap<String, ComponentEntityType>`), and the
      `Types::component_entity_type_of_import/export(name)` accessors were removed. wac-types'
      `Package::from_bytes` `TypeConverter` (`entity()` / `component_entity_type()` / the import/export decode
      loop) is written against `ComponentEntityType`, so the family bump forces porting that decoder to
      `ComponentItem` — careful semantic work that must not be rushed (a wrong port silently corrupts
      composition's type checking).

    - **Strategic consequence — the family-collapse recommendation in D32 should be reconsidered.** D32
      advised bumping the wac crates to the 0.250 family to "halve the de-std surface". But the cost of that
      bump is now known to include a wac-types type-decoder rewrite. The alternative D32 rejected — keep the
      wac crates on the **0.247 family** and de-std a *second* `wasmparser`/`wasm-encoder` set (both 0.247 and
      0.250) — avoids the decoder rewrite entirely, and that second de-std is mechanical (0.247 wasmparser is
      also `#![no_std]`-capable, same feature-surgery as 0.250). Net trade for the next session: **"de-std a
      second wasmparser/wasm-encoder family (mechanical)" vs "port wac-types' decoder to the 0.250
      ComponentItem model (semantic, risky)"** — the former is very likely the cheaper, safer path. Decide
      this before continuing; if the 0.247-family route is taken, revert wac-types' Cargo.toml version bumps
      (keep all the de-std source edits, which are family-independent) and instead make the closure depend on
      a de-std'd 0.247 wasmparser/wasm-encoder.

    - **Remaining closure after the wac decision:** `wac-graph` (de-std + `thiserror`→2 + excise
      `wasm-metadata` + verify/replace `petgraph` no_std) and `wit-component` (de-std + excise
      `wasm-metadata` + drop `wat`/`wast`), then Checkpoint A (flip eo9-component deps to
      `default-features = false`, add the kernel `[patch]` entries + `eo9-kernel → eo9-component` dep, link
      under `wasm-codegen`) and Checkpoint B (wire `$`/`&` into `shellexec.rs`). `wasm-wave` looks free
      without its `wit` feature (per D32) and `wit-parser` is already done.

34. **Checkpoint 4 — 0.247 route executed: wac-types and wac-graph are no_std-green; the petgraph blocker is
    resolved (no hand-roll needed).** Two more crates of the algebra closure now build standalone for
    `aarch64-unknown-none` and are committed; they remain inert (not yet in any `[patch]` table), so the
    branch HEAD build state and featureless `cargo xtask ci` are unchanged.

    - **wac-types — green on the 0.247 family.** Per the approved Decision-33 route, reverted its
      `wasmparser`/`wasm-encoder` deps from 0.250 back to **0.247** and reverted the prior session's two
      0.250-isms in `package.rs` (`import?.name.name`/`export?.name.name` → `.name.0`, the 0.247
      `ComponentImportName`/`ComponentExportName` API). No decoder port needed — the original wac-types
      decoder is written against 0.247. All the family-independent de-std edits from the prior session stay.
      Verified: `cargo build -p wac-types --target aarch64-unknown-none --no-default-features` builds.

    - **wac-graph — vendored + de-std'd, green.** no_std + alloc; `std::` → `core`/`alloc`/`hashbrown`
      (HashMap/HashSet); crate-level `IndexMap` alias with a no_std default hasher (the wac-types pattern,
      needed because indexmap's default `S = RandomState` is std-only); `thiserror` 1.x → **2** (no_std);
      `wasm-metadata` made an optional dep behind an **off-by-default `metadata` feature** and its one
      producers-section call site gated out (the kernel doesn't need a producers custom section); deps set
      `default-features = false` with a forwarding `std` feature. Verified standalone-green.

    - **petgraph resolved — bump to 0.8.3, NOT a hand-roll.** petgraph 0.6.4/0.6.5 are std-only (no
      `#![no_std]`, no std feature), which was the feared wall. But **petgraph 0.8.3 is available in the
      registry and is no_std-capable** (`default-features = false` + `stable_graph`); wac-graph's full API
      surface (StableDiGraph, Dfs, toposort, visit maps, Direction, Reversed, Dot) compiled against 0.8.3
      with zero source changes. So the hand-rolled-DAG contingency from D30/D32 is unnecessary. (The earlier
      "registry unreachable" impression was a `cargo search` artifact; direct version resolution fetches
      fine.)

    - **Standalone-test note:** an `indexmap/std` unification error in the standalone build was a test
      artifact — wac-graph's dev-dependencies pull the full std stack under a v1 resolver. Adding
      `resolver = "2"` to the temporary `[workspace]` table (and dev-deps aren't built when wac-graph is a
      normal lib dependency of eo9-component/the kernel) made it clean. Vendored `target/` dirs are now
      gitignored (`kernel/vendor/**/target/`).

    **Remaining to Checkpoint A (next session):**
    - **wit-component (0.250) — the last and most delicate crate.** ~12k LOC, NOT no_std; std usage is
      mechanical (33 sites: fmt/collections/mem/borrow/hash/str/ops/iter/error → core/alloc). The real work
      is **excising `wasm-metadata`** (can't be ported — flate2/url/icu/spdx), which is woven through the
      **encoder path eo9-component actually uses**: `base_producers()` (lib.rs:86) and `AddMetadata`
      (encoding.rs:442) are in the `ComponentEncoder` path, plus `Producers::from_wasm/from_bytes`
      (metadata.rs/gc.rs). eo9-component calls only `wit_component::embed_component_metadata` +
      `ComponentEncoder::default()` (synth.rs), so the excision must keep the encoder producing a valid
      component while dropping the (informational) producers section — careful, not mechanical. Drop the
      `wat`/`wast` features. Give it its own `std` feature.
    - **Then Checkpoint A:** in `crates/eo9-component`, set the per-dep `default-features = false` and extend
      its `std` feature to also forward `wit-component/std` and `wac-graph/std` (currently it only forwards
      wasmparser/wasm-encoder/wit-parser/wasm-wave). Add `eo9-kernel → eo9-component` (`default-features =
      false`) as a cross-workspace path dep and extend the kernel `[patch.crates-io]` to redirect the whole
      closure (wac-types, wac-graph, wit-component, wit-parser — wit-parser is already vendored/green) at
      once. Target: featureless `cargo xtask ci` still green (usermode unchanged) AND `cargo xtask
      build-kernel aarch64` links eo9-component under `wasm-codegen`.
    - **Then Checkpoint B:** wire `$`/`&`/`only`/`configure` into `shellexec.rs` and route fused components
      through `codegen.rs` (Decision 29).

35. **Checkpoint 4 COMPLETE — interactive composition + compilation on bare metal.** The algebra
    dependency closure is fully no_std and linked into the kernel under `wasm-codegen`, and the shell's
    `eo9:exec/component-algebra` now runs the real `eo9-component` algebra. A user at the bare-metal eosh
    prompt can compose and run programs that are compiled on-target; verified transcript:
    ```
    eosh> hello --name metal --excited true
    [..] Hello, metal!
    ok: greeted
    eosh> entropy.seeded $ cruncher --seed 9 --rounds 200000
    ok: digest(14341732361190694547)
    eosh> exit
    eosh: session ended, outcome = ok(exited)
    ```
    `entropy.seeded $ cruncher` is fused by `eo9_component::compose`, compiled by Cranelift on-target
    (`Component::new`), instantiated against the kernel root providers, and run — no host-prefused artifact.

    - **The closure (Checkpoint A, commit "link the no_std component algebra…").** Five crates are vendored
      under kernel/vendor and de-std'd: `wit-parser`, `wac-types`, `wac-graph` (0.247 family, petgraph 0.8.3,
      thiserror 2, wasm-metadata behind an off-by-default `metadata` feature), plus now **`wit-component`**
      (0.250; `#![no_std]`+alloc prelude, hashbrown/indexmap no_std hashers, the unused serde/serde_json deps
      dropped, the `wasm-metadata` producers sections gated behind `metadata` and the serde-only
      package-metadata section behind a `wit-package-metadata` feature, `libdl.so` data file carried over,
      and explicit ordered `drop`s where hashbrown's lack of the std `#[may_dangle]` dropck eyepatch kept
      `self`-borrowing maps alive past `encode`'s `self`-move) and **`wasm-wave`** (already no_std; its `wit`
      feature no longer force-enables `std`, needed for `value::resolve_wit_type`). `eo9-component` itself was
      already no_std-capable (Decision 31); its deps are declared directly with `default-features = false`
      (functional features kept on, only `std` toggled) so the host build is byte-identical — usermode
      `cargo test -p eo9-component` stays green (56 tests). The kernel depends on eo9-component
      (`default-features = false`) under `wasm-codegen`, and the kernel `[patch.crates-io]` redirects the
      whole closure.

    - **The shell wiring (Checkpoint B).** `KComponent` now carries the component bytes plus an
      `Option<store-entry>` (the originating baked entry, when pristine). `compose`/`extend`/`restrict`/
      `rename`/`configure` call the matching `eo9_component` function on the operand bytes and store the fused
      result as an entry-less component; `compile` deserializes the baked host-AOT artifact for store entries
      (fast path) and runs `Component::new` on-target for fused ones; `describe` replays baked metadata for
      store entries and decodes fused bytes via `eo9_component::Component::describe`; `load` also accepts
      arbitrary valid component bytes now. All of this is `#[cfg(feature = "wasm-codegen")]`-gated, falling
      back to the previous "needs on-target codegen" refusals when the feature is off, so every feature
      combination still builds. Capability containment is unchanged: children still instantiate against the
      fixed text/time/entropy provider linker and never receive fs or exec.

    - **Determinism / limits.** Not bit-compared across runs (noted, not blocking); the digest is reproducible
      by seed. `entropy.seeded $ cruncher` produces cruncher's deterministic digest (cruncher does not draw
      entropy — the composition simply supplies an unused provider, which is correct). Remaining metal work is
      unchanged: GIC/interrupts (executor still polls), child fuel + eo9-sched, friendlier missing-fs errors
      for children, riscv64/x86_64.

36. **Idle CPU fix — GICv2 + timer IRQ + `wfi` (no more 100% spin).** The kernel previously
    busy-spun whenever a guest awaited a host operation: the interactive shell's drive loop
    (`shell.rs::run_eosh`) and the headless executor (`mod.rs::block_on`) both `core::hint::spin_loop()`
    on `Poll::Pending`, and the `read-line`/`time.sleep` futures (`providers.rs`) self-woke
    (`cx.waker().wake_by_ref()`), so even at the idle eosh prompt a host CPU sat pegged at ~100%
    (owner-reported). Fix:
    - **GICv2 bring-up (`gic.rs`).** Minimal distributor + CPU-interface enable (`GICD_CTLR`,
      `GICC_PMR=0xff`, `GICC_CTLR`), `configure_intid` (priority) + `enable_intid` for the generic-timer
      PPIs (26/27/29/30), and `acknowledge`/`end_of_interrupt` (IAR/EOIR). QEMU `-cpu max` defaults to
      GICv3, whose CPU interface is system-register-based, so xtask now pins `-M virt,gic-version=2`.
    - **IRQ handler (`boot.rs __irq_entry` → `exceptions.rs kirq`).** The two current-EL IRQ vectors branch
      to a stub that saves the caller-saved integer registers (x0–x18, x30) — the handler is built without
      FP so it never touches the v registers, leaving interrupted Cranelift/wasm SIMD state intact — calls
      `kirq` (ack at the GIC, disable the timer so its level line drops, EOI), and `eret`s. Every other
      exception stays fatal. `kmain` unmasks IRQ (`msr daifclr, #2`) after the GIC is up.
    - **`wfi` idle (`mod.rs::idle_wait`, used by both drive loops).** On `Poll::Pending`: arm the EL1
      physical timer ~`IDLE_WAKE_INTERVAL_NS` (10 ms) ahead unmasked (`timer::arm_wake`), `wfi` (the GIC
      forwards the timer interrupt, which the handler EOIs), then wake the parked host future. The
      `read-line`/`time.sleep` futures no longer self-wake; they register their waker
      (`mod.rs::register_idle_waker`, a single-slot spinlock cell) which `idle_wait` wakes after each `wfi`,
      so wasmtime re-polls them on the next iteration instead of busy-re-polling inside one poll.
    - **Result.** Idle host CPU at the eosh prompt drops from ~100% to **~1%** (measured via `top -pid`
      against a kernel sitting at the prompt on an empty stdin). Heavy guest compute (cruncher rounds,
      on-target codegen) runs inside a single poll, so it is unthrottled; only await-point latency is bounded
      by the 10 ms wake interval (serial input feels immediate; the demo's 50 ms sleepy measures ~52 ms).
      Verified: interactive `hello` + `entropy.seeded $ cruncher` (on-target), `demo` (sleepy, entropy,
      on-target codegen, clean power-off), and `program=` headless all unchanged; `cargo xtask ci` green.
    - **Limits / next.** Wake is timer-periodic (10 ms), not yet UART-RX-interrupt-driven, so an idle prompt
      still wakes ~100×/s (cheap — ~1% — but not a true event-driven 0%); a PL011 RX interrupt would make it
      fully event-driven. The IRQ path handles only the timer; a real scheduler/preemption tick is future
      work. The masked-`wfi`-wake shortcut (no handler) did not work under QEMU here, hence the real
      handler.

37. **Shell presentation fixes (2026-05-27).** The session manifest's note now tells the truth about
    composition: with `wasm-codegen` it says compositions are fused and compiled on-target, without it it
    says the kernel was built without the feature (it previously always claimed composition was unavailable).
    Spawn instantiation errors that mention an unsatisfied `eo9:*` import are rendered as a friendly
    missing-capability message instead of the raw linker error (the fs/io case from the user studies).

38. **Child fuel / preemption and metal shell recursion (2026-05-27).** The three pieces that turn "one
    looping child takes the machine" (the embedded-study blocker) into a scheduled system:
    - **Fuel on metal.** The kernel engine and xtask's `precompile_for_kernel` both set `consume_fuel(true)`
      (compile-relevant, so the baked artifacts and on-target codegen agree; the store image's artifacts grew
      ~14% — 3.5 → 3.9 MiB — and are re-precompiled by every `build-kernel` run anyway). Every store sets
      fuel before guest code runs: the demos/headless runner/eosh use an effectively-unlimited pool; spawned
      children get usermode parity — `SPAWN_FUEL` (40k) for instantiation, then `u64::MAX` sliced by
      `fuel_async_yield_interval(FUEL_QUANTUM = 10_000)`, so every poll of a child runs at most one quantum
      and yields back to the drive loop. The headless runner accepts `max-fuel=<units>` (the metal counterpart
      of `eo9 run --max-fuel`): an exhausted budget ends the run as `abnormal(killed)` (verified:
      `program=cruncher … max-fuel=50000`). An `OutOfFuel` trap in a child maps to the `killed` outcome.
    - **The drive loop checks children out to poll them.** `ChildSlot` gained a `Polling` state:
      `drive_children` takes the drive future out of the registry, polls it with the lock released, and checks
      it back in (kill/handle-drop during a poll are honoured at check-in). This removes the D36 deadlock —
      a child being polled can itself spawn/wait/kill through the same registry — and children spawned
      mid-pass get their first poll in the same pass. The boot `demo` now opens with a scheduling
      demonstration using exactly this machinery: three cruncher children (200k rounds, 2M rounds, and a
      `u64::MAX`-rounds spinner) interleave on one drive loop — the short one finishes (~508 turns) while the
      long one and the spinner are still running, the long one finishes (~5k turns) while the spinner still
      spins, and the spinner is then killed cleanly (`abnormal(killed)`).
    - **Children inherit the full session environment.** `spawn_child` now links the read-only store fs,
      io buffers, and the whole `eo9:exec` surface (plus text/time/entropy as before) and gives each child its
      own `ShellState` over the shared store image — the same inherit-everything-restrict-with-`only` default
      as usermode (plan/11 D14–15). `eosh> eosh` therefore works on metal: the nested shell resolves `/bin`,
      runs programs, fuses + compiles compositions on-target, reads the truthful session manifest, and `exit`
      returns to the outer shell (verified interactively; the grandchild spawn from a child mid-poll is the
      D36 case the lock rework enables). The friendly missing-capability message now covers only what the
      kernel genuinely lacks (net/disk/pci).
    - **eo9-sched: not adopted yet (recorded).** The registry's round-robin plus wasmtime's fuel-yield
      slicing already gives every child a bounded slice per turn on the single core; eo9-sched's conserved
      fuel ledger and policies become valuable with guest-directed `resume`/donation (E5) or priorities, and
      adopting it now would add bookkeeping with no observable change. Revisit alongside E5.
    - **Limitations / follow-ups.** Killing a parent does not cascade to its children (the registry is flat;
      orphaned grandchildren run to completion unobserved); shell-spawned children have no per-child hard cap
      (same as usermode — only the headless runner takes `max-fuel`); nested shells share the one serial
      console (input goes to the innermost active reader); eosh itself still has no interrupt key, so a
      foreground spinner still occupies the *prompt* even though the machine, other children, and the kill
      path all keep working; the idle-wake interval (10 ms) bounds await-point latency as before.

## Follow-up — panic diagnostics on metal (2026-05-27)

The usermode runtime now renders guest traps via a cleaned reason builder
(`crates/eo9-runtime/src/trap.rs`: trap kind + a demangled, address/hash-free symbol backtrace; the panic
*message* itself awaits the per-world post-trap export proposed in plan/07 Decision 11). The kernel's own
trap/outcome path (`kernel/src/wasm`) still renders the raw wasmtime error. Follow-up: share or mirror the
same cleaned-reason logic on metal so a bare-metal guest panic reads the same way, and adopt the panic-export
once it lands. Deliberately not done in the panic-channel pass to avoid colliding with the concurrent kernel
preemption/hardening work.
39. **Metal depth hardening — UART RX interrupt + event-driven `wfi` idle (2026-05-27).** Item 1 of the
    D38-follow-up list. Previously the idle path armed a fixed 10 ms timer and re-polled `read-line` on every
    wake (~1% host CPU at the prompt) and a compute-bound child in the *interactive* shell advanced only one
    fuel quantum per wake. Now:
    - **Receive is interrupt-driven.** `enable_rx_interrupt` unmasks the PL011 RX + RX-timeout interrupts;
      the GIC forwards UART SPI 33 (added to the enable list in main.rs); `kirq` calls `uart::drain_rx`, which
      empties the RX FIFO into a small SPSC ring (`RX_RING`, IRQ = sole producer, the boot core's `read-line`
      = sole consumer) and clears the UART interrupt. `ReadLine` now consumes from the ring (`ring_get_byte`)
      rather than polling the data register. A keystroke therefore wakes `wfi` directly.
    - **Idle is event-driven and deadline-precise.** `idle_wait(child_running)` arms the timer to the earliest
      deadline a parked future requested (`SleepUntil` → `request_timer_wake`, taking the min), capped by a
      backstop: short (10 ms) when a child is still running so it keeps getting turns, long (1 s) at the bare
      prompt where input arrives as a UART interrupt. So a `sleep` wakes at its actual deadline and an idle
      prompt sleeps ~1 s at a time (measured host CPU at the prompt: 0.7% on the first sample then 0.0%,
      vs the previous steady ~0.8%). The `wfi` runs with IRQ masked (`daifset`/`daifclr` around it) so an
      interrupt that becomes pending in the poll→`wfi` window still wakes it — no lost-wakeup race.
    - **Runnable children no longer wait on `wfi`.** `drive_children` returns a `DriveStatus`; the
      flag-`ChildWaker` records whether a child's poll rang its waker (a fuel yield does; a host-future park
      does not). The interactive loop re-polls immediately while any child is runnable (full-speed compute,
      an improvement over the old one-quantum-per-tick) and only `wfi`s when nothing is runnable.
    - Verified: `cargo xtask ci` green; `qemu demo` reproduces the preemption demo + hello + sleepy (57 ms ≥
      50) + on-target codegen unchanged; interactive `qemu` runs `hello`/`cruncher` and `exit` over the now
      interrupt-driven console; idle CPU ~0% (above). Stale "PSTATE.I stays masked / timer is the only source"
      comments in exceptions.rs updated; gic.rs/timer.rs module docs still describe the original masked design
      and are a doc-only follow-up.
    - **Not done this pass (the rest of D38, precise next steps):** (2) a Ctrl-C interrupt key — now cheap
      given the RX ring: have `drive_children` (or the exec `wait` path) scan the ring for `0x03` while a
      foreground child runs and route it to the existing kill path, tracking which child is foreground;
      (3) parent-kill cascade — give `ChildSlot`/the registry a parent rep and have `kill` recurse;
      (4) per-child hard cap — a per-exec-holder counter + a bounded child-fuel pool in `spawn_child`
      (mirroring usermode `--max-fuel`); (5) W^X for JIT pages — split the publisher's identity mapping so
      code pages are mapped executable-not-writable after the cache maintenance (needs mmu.rs page-permission
      support).

40. **Metal depth hardening — Ctrl-C, kill-cascade, per-child cap (2026-05-27).** D38 items 2–4 of D39.
    - **Ctrl-C interrupt key (item 2).** `uart::take_ctrl_c()` non-destructively scans the RX ring for ETX
      (`0x03`) and, if present, consumes the ring up to and including it (flushing pending input, the usual
      terminal behaviour). The exec `task.wait` op calls it on each pending iteration — the shell is parked in
      `wait` (not `read-line`) while a foreground job runs, so a Ctrl-C there means "kill what I'm waiting on":
      it kills the awaited task (and its descendants, below) and returns its outcome, dropping back to the
      prompt. Verified (scripted serial): `cruncher --rounds 100000000000` interrupted → `abnormal: killed`,
      prompt returns, a following `hello` runs.
    - **Kill cascade (item 3).** A parallel `PARENTS` vector (index-aligned with `CHILDREN`) records each
      child's parent rep; `CURRENT_PARENT` is set around each `drive_children` poll so a nested spawn during
      that poll records its parent (top-level shell spawns record `None`). `kill_task_tree(rep)` kills `rep`
      and all transitive descendants (fixed-point over `PARENTS`, cycle-bounded); both `task.kill` and the
      Ctrl-C path use it, so killing a foreground nested eosh takes its children/grandchildren down rather
      than orphaning them on the drive loop. Verified: nested eosh running a foreground cruncher, Ctrl-C →
      both die, machine stays responsive (a following `hello` runs immediately).
    - **Per-child hard cap (item 4).** `MAX_LIVE_CHILDREN = 64`: `spawn_child` refuses (clear error) once that
      many children are live (running/checked-out), so a fork-bomb-style shell can't exhaust memory/drive-loop
      time; finished children free slots. Inert for normal nesting (the demo + interactive sessions are
      unaffected). Not independently QEMU-exercised (needs a 64-spawn loop eosh lacks); the bound is a simple
      pre-spawn count check.
    - Doc-only fixes done: the stale "interrupt is never taken as an exception / PSTATE.I stays masked" note in
      timer.rs (the timer IRQ is now taken and EOI'd by `kirq`), and the kernel async-demo "async-lifted
      configure" strings (configure is now sync). Verified: `cargo xtask ci` green; `qemu demo` reproduces the
      sched/preemption demo + hello + sleepy + on-target codegen unchanged.
    - **Remaining (item 5): W^X for JIT code pages** — give mmu.rs page-permission support and map on-target
      code pages executable-not-writable (writable-not-executable while being written), after the existing
      I/D-cache maintenance. Not started this pass; the gic.rs module-header doc still describes the original
      masked-WFI design (doc-only follow-up).

41. **Metal depth hardening — W^X for JIT code pages (2026-05-27).** Item 5 of D39, done. The 1 GiB DRAM
    block in `mmu.rs` is now mapped at **4 KiB page granularity** (level-1 → one level-2 table → 256 level-3
    tables, ~1 MiB of static page tables covering the 512 MiB DRAM window): the DTB area and the heap default
    to Normal RW **non-executable** (PXN|UXN), the kernel image `[__kernel_start, __heap_start)` stays RWX
    (the trusted kernel runs from it), and the device window is unchanged. A new
    `mmu::set_range_permissions(start, len, PagePerm)` rewrites the L3 descriptors for a range (AP[2]/PXN/UXN),
    then `dsb ishst` → per-page `tlbi vaae1` → `dsb ish; isb`. The code publisher
    (`wasm::BareMetalCodeMemory`) now uses it: `required_alignment` = 4096 (whole-page code regions),
    `publish_executable` cache-maintains then flips the range to **executable + read-only** (`ReadExecOnly`),
    `unpublish_executable` flips it back to RW-NX. So Cranelift-emitted guest code is written into NX heap and
    is never simultaneously writable and executable. The kernel image itself is left RWX (internal `.text`/
    `.data` W^X is a further hardening, not the JIT threat addressed here). Verified: `cargo xtask ci` green
    (featureless); `qemu demo` — `mmu:` line reports "heap W^X", on-target Cranelift codegen runs from the
    flipped pages (`add(17,25) -> 42`), and every deserialize path (seed/hello/sleepy/entropy.seeded) +
    preemption demo run + clean PSCI power-off; interactive plain `hello` runs under W^X. (The interactive
    on-target composition exercises the identical publish→flip path the demo proves; the scripted serial
    harness couldn't pace input over the interrupt-driven console to capture it separately.) Also corrected
    the now-stale `gic.rs` module header (IRQs are taken via `kirq` now, not masked-WFI). Remaining: finer
    W^X for the kernel image's own `.text`/`.rodata`; guard regions.

42. **Kernel argument codec: WAVE lists + the variadic-tail default (2026-05-28).** The kernel-side WAVE
    codec (`wasm/wave.rs`) now parses `list<T>` values (`[…]`, top-level-comma split that respects quoted
    strings and nesting), and `shellexec::bind_args` defaults a *final* `list<…>` parameter that was never
    supplied to the empty list — the same variadic-tail rule as the usermode binder — so the re-signatured
    coreutils (`cat a.txt b.txt`, bare `ls`) bind correctly when spawned from eosh on metal. Verified:
    `build-kernel aarch64` + an interactive QEMU session (hello, an on-target `entropy.seeded $ cruncher`
    composition, clean exit) — no regressions. Remaining gap, deliberately not done here: the coreutils are
    not in `KERNEL_STORE_COMPONENTS` (xtask), so there is no list-taking program in the baked metal store to
    exercise the new path end-to-end; adding `ls`/`cat` to that list (and re-baking) is the one-line
    follow-up that makes bare `ls` actually runnable at the metal prompt. The headless `runner.rs` `program=`
    path keeps its scalar-only parser for now.

43. **`eo9:pci` root provider — opt-in PCI on bare metal (2026-05-28, branch `area/12-pci-provider`).** The
    kernel now implements the `eo9:pci` capability (wit/pci, plan/02 D14) directly against the machine, as the
    foundation for wasm device drivers. Split: `src/pci.rs` is the hardware half (raw ECAM at `0x3f00_0000`
    with `highmem=off`, bus walk, width-explicit config read/write, BAR sizing via the all-ones probe, BAR
    assignment from a bump allocator over the 32-bit PCIe MMIO window `0x1000_0000..0x3eff_0000` + memory-
    decode enable, bus-master toggle); `src/wasm/pci_provider.rs` is the provider half (WIT-shaped types,
    per-store handle tables for device/bar/dma-buffer on `KernelState.pci`, registration mirroring the other
    root providers). xtask's QEMU invocation adds `highmem=off` (the default highmem layout puts the ECAM
    above 4 GiB, outside the kernel's identity map; reading the ECAM base from the DTB instead is a follow-up)
    and `-device virtio-rng-pci` so enumeration finds real functions. Decisions and bounds:
    (a) **Never linked by default.** PCI without an IOMMU is DMA, i.e. full-memory authority, so the provider
        is only added to linkers when the boot's command line carries the bare `pci` token
        (`cargo xtask qemu aarch64 pci …`); the loader rule then still applies (only programs importing
        `eo9:pci/pci` link it). Without the token, spawn refuses with the capability story plus the exact
        token to add (`shellexec::missing_capability`). Finer-grained grants (`pci.filtered` composed in
        front of a driver) ride on top, unchanged.
    (b) **Honest `unsupported`, never a wrong answer:** interrupt delivery (`enable-interrupts` / `wait` —
        INTx/MSI-X routing through the GIC is the next kernel step a real driver needs, though virtio can
        poll), function-level `reset`, I/O-space BARs (the arm64 PIO window is unmapped; QEMU's default
        legacy/transitional virtio functions expose I/O BARs, so a driver demo should add a modern,
        `disable-legacy=on` function), and qword config-space access (per the WIT).
    (c) **DMA buffers** are page-aligned kernel-heap allocations (≤ 4 MiB each, ≤ 64 live per task); with the
        identity map the CPU address *is* the bus address, and QEMU keeps DMA coherent — real hardware will
        need non-cacheable mappings or explicit cache maintenance here. Handle tables are per-store, so
        exclusive device claiming (`busy`) is per-task only for now; machine-global single-driver-per-device
        arrives with the interrupt work.
    (d) **`lspci` demo** (`guest/examples/lspci`, SDK gains the `pci` API arm): enumerates and prints one
        line per function; baked into the kernel store (now 8 entries). Verified on QEMU: headless
        `pci program=lspci` and interactive `lspci` both list the host bridge (1b36:0008), the default
        virtio-net NIC (1af4:1000), and virtio-rng (1af4:1005) and exit `success(devices(3))`; without the
        token the friendly refusal prints; existing flows (cruncher headless, boot-to-eosh, exit, power-off)
        unchanged. CI green at each commit.
    (e) **Next (recorded, not built): a virtio-blk driver as a wasm component** — imports `eo9:pci` (+ io
        buffers), exports `eo9:disk`; QEMU side `-device virtio-blk-pci,disable-legacy=on,drive=…`; driver
        side: find the virtio vendor capabilities in config space, map common/notify/device-cfg through
        `open-bar`/`bar-read`/`bar-write`, build the virtqueue in `alloc-dma` buffers, kick via the notify
        register and poll the used ring (no interrupts needed for v0). Its natural consumer is store-on-disk /
        eofs-on-metal (area 14), which is what makes the metal artifact cache writable.
44. **The architecture layer is split out of the shared core (2026-05-28).** Preparation for the riscv64
    port (the breadth step of the roadmap). Everything aarch64-specific — the boot stub and exception
    vectors, GICv2, the identity-map MMU + W^X flip, PL011, PL031, generic timer, PSCI — moved from flat
    `src/*.rs` modules to `src/arch/aarch64/`, behind a per-architecture surface that every port provides
    identically: the `mmu` / `power` / `rtc` / `timer` / `uart` modules (re-exported at the crate root by
    main.rs, so the shared core keeps saying `crate::timer::…` with no `target_arch` cfgs anywhere outside
    `src/arch/`), plus `banner()`, `interrupts_init()`, and `NAME`. Three formerly inline aarch64-isms moved
    behind that surface: the wasmtime engine's target string (now the per-arch `NATIVE_TARGET`), the code
    publisher's cache maintenance (`mmu::flush_code_range`), and the executor's masked-`wfi` idle step
    (`timer::wait_for_interrupt`); build.rs selects the linker script per target arch (`linker-aarch64.ld` /
    `linker-riscv64.ld`). One real bug surfaced by the split: the idle halt sequence used `options(nomem)` on
    the `wfi`/unmask asm, so nothing told the compiler that the interrupt taken at the unmask writes memory
    (the UART input ring) that the caller re-reads immediately — the old inline placement happened to provide
    the barrier via call boundaries, and extracting the sequence produced a kernel whose console input stalled
    after a few dozen bytes. `wait_for_interrupt` now omits `nomem` on the halt/unmask instructions (it *is*
    the memory barrier) and the comment says why. Verified: featureless ci green; the aarch64 `qemu demo`
    transcript is unchanged (modulo the heap-start address, the image got slightly smaller) and a scripted
    interactive eosh session (ls / unknown command / parse error / exit) completes exactly as before.

45. **riscv64 port, milestones 1–2: boot + serial + heap + timer + interrupt delivery on QEMU `virt`
    (2026-05-28).** `cargo xtask build-kernel riscv64` builds a feature-less image
    (`riscv64gc-unknown-none-elf`, linked at 0x8020_0000, `linker-riscv64.ld`) and `cargo xtask qemu riscv64`
    boots it under `qemu-system-riscv64 -M virt,aia=none -smp 1 -m 512M -nographic` with QEMU's bundled
    OpenSBI as `-bios`: banner (S-mode, 10 MHz timebase, Goldfish-RTC wall clock), bare-mode `mmu` notice,
    507 MiB heap + self-test, SBI-timer 10 ms polled check, an end-to-end *delivered-through-the-trap-path*
    timer-interrupt check, `cmdline:` from the DTB QEMU passes in `a1`, and a clean SBI SRST shutdown so QEMU
    exits by itself. The arch layer mirrors aarch64 module for module: 16550A UART at 0x1000_0000 with the
    same interrupt-filled RX ring (PLIC source 10 → S-mode context 1, `aia=none` pinned for the same reason
    GICv2 is pinned on aarch64), `time` CSR + SBI TIME for the timer (`disable` = set-timer(MAX), which is how
    the level drops before `sret`), Goldfish RTC for seconds-since-epoch, SBI SRST + the sifive_test finisher
    as the power-off path, and a direct-mode trap entry that saves the caller-saved integer *and* FP registers
    (the riscv64gc kernel is hard-float, unlike aarch64-unknown-none, and traps may land in Cranelift code) —
    wrapped in `.option arch, +f, +d` because module-level asm is otherwise assembled against a baseline ISA
    at higher opt levels. Known deltas from aarch64, all deliberate: translation stays off (`satp` Bare), so
    `set_range_permissions` is a documented no-op and W^X for published JIT pages waits for the Sv39 step;
    the heap stops 2 MiB short of the top of RAM because QEMU places the DTB there; the timebase frequency is
    a constant (10 MHz on `virt`) rather than read back from the FDT; the UART RX interrupt path is wired and
    enabled but has no consumer until the wasm executor lands, so it is design-verified only. `cargo xtask ci`
    now builds and clippy-checks the feature-less kernel workspace for **both** bare-metal targets
    (`KERNEL_CI_TARGETS`), so the port cannot rot silently; aarch64's `build-kernel`/`qemu` paths are
    untouched (same QEMU invocation, same demo transcript). Next (milestone 3): host-AOT components against
    the kernel providers — needs xtask's precompile step parameterized by target plus the `all-arch` cranelift
    feature on the host wasmtime so it can emit riscv64, and the kernel's wasm features built for
    riscv64gc (the vendored no_std closure is expected to be arch-clean but is unproven there); then the
    baked store + eosh (milestone 4), Sv39 + W^X + on-target codegen (milestone 5), and fuel/preemption/Ctrl-C
    parity (milestone 6).
46. **Basic coreutils baked into the kernel store (2026-05-28, branch `area/11-host-odds`).**
    `KERNEL_STORE_COMPONENTS` gains ls, cat, echo, wc, head, and stat, so the metal shell can
    inspect its own read-only filesystem (`ls`, `ls /bin`, `cat /session`, multi-path `wc`) —
    exercising the D42 variadic-tail default interactively. Cost: the store image grows from
    8 to 14 entries (917 KiB of components + 7.6 MiB of host-AOT artifacts), kernel image
    25.1 MB → 29.0 MB (+3.9 MB; the precompiled artifacts dominate — dropping them for the
    coreutils and relying on on-target codegen is the lever if the image ever needs trimming).
    Verified on QEMU aarch64: bare `ls` → listed(2), `ls /bin` → listed(14), `cat /session` →
    printed(903), `wc /session /session` → totals line, clean exit/PSCI off.
47. **riscv64 milestone 3: host-AOT components run on the riscv64 kernel — and the baked store boots
    eosh (2026-05-29, branch `area/12-riscv64-m3`).** No kernel source changes were needed: the D44 arch
    split plus the per-arch `NATIVE_TARGET` already prepared the wasm side, and the vendored no_std
    wasmtime closure compiles for `riscv64gc-unknown-none-elf` as-is. The work is xtask-side:
    `precompile_for_kernel` and `build_store_image` take the bare-metal target (aarch64 keeps the original
    flat `kernel/target/precompiled/` layout and stays byte-for-byte identical — verified by hashing every
    artifact and the ELF before/after; other targets get `precompiled/<target>/`), and
    `build-kernel riscv64` now runs the same precompile pipeline as aarch64 (seed, hello, the async pair,
    the 14-component store image) and builds the kernel with `wasm-seed,wasm-hello,wasm-async,wasm-store`
    (no `wasm-codegen`: on-target codegen is milestone 5, so the metal shell refuses `$`/`&` with the
    documented message). Emitting riscv64 from the host needs the non-host Cranelift backends, which only
    the new off-by-default `kernel-cross-aot` xtask feature links (`wasmtime/all-arch`); `build-kernel
    riscv64` re-runs itself with that feature automatically, so every other xtask invocation stays lean —
    the one-time cost of the cross-AOT build measured ~25 s on the dev machine, and plain builds pay
    nothing. Verified on QEMU riscv64 `virt`: `program=hello name=riscv excited=true` →
    `[wall-clock] Hello, riscv!` / `success(greeted)` (45 ms instantiate+main); `program=cruncher seed=9
    rounds=200000` → `success(digest(14341732361190694547))` — the same digest as aarch64 and native;
    `demo` → the fuel-sliced sched/preemption demo (short/long finish, spinner killed), the seed component
    (`add(17,25) -> 42`), the hello program, the sleepy canary awaiting a 50 ms kernel-timer sleep
    (62.2 ms observed), and entropy.seeded sync-configure with the exact SplitMix64 values; and an
    interactive scripted session — the baked store boots **eosh on riscv64**: `ls /bin` lists 14 programs,
    `hello --name riscv --excited true` → `ok: greeted`, a `$` composition refuses with the no-codegen
    message, `exit` → clean SBI shutdown. aarch64 re-verified unchanged on the same xtask (demo incl.
    on-target codegen, interactive with Ctrl-C kill, `pci program=lspci` → 3 devices). Remaining for the
    port: milestone 5 (Sv39 translation + W^X + the riscv64 backend in the vendored compile fork for
    on-target codegen) and milestone 6 parity checks (Ctrl-C / RX-ring consumption on riscv64, idle-power
    measurement); milestone 4's boot-to-eosh goal is already covered by the store image above.
48. **riscv64 milestone 5: Sv39 + W^X + on-target codegen (2026-05-29, branch `area/12-riscv64-m5`).**
    The riscv64 port now matches aarch64's depth. (a) `arch/riscv64/mmu.rs` builds an Sv39 identity map
    mirroring the aarch64 layout: one read/write non-executable gigapage over the MMIO window
    (UART/PLIC/RTC/test finisher), DRAM at 4 KiB page granularity (root → one mid-level table → 256 leaf
    tables, ~1 MiB of static tables) with OpenSBI's reservation and the heap+DTB region read/write
    non-executable and the kernel image+stack RWX, `satp` switched to Sv39 with `sfence.vma` on either
    side; everything else is unmapped and faults. All leaf PTEs carry A/D/G so implementations without
    hardware A/D updating never fault on first touch. (b) W^X: `set_range_permissions` rewrites the leaf
    PTEs (R/W vs R/X) and issues per-page `sfence.vma`; the shared code publisher already orders writes and
    runs `fence.i` (`flush_code_range`), so published code — deserialized or Cranelift-emitted — is never
    writable and executable at once, exactly like aarch64 (D41). (c) On-target codegen: `build-kernel
    riscv64` now enables `wasm-codegen` and passes the raw seed wasm; cranelift's riscv64 backend is
    selected automatically by `host-arch` when compiling for the target. One vendored addition was needed:
    registry `cranelift-codegen` 0.132.0 uses the std-only `f64::powi` in four constant comparisons in the
    riscv64 backend (`src/isa/riscv64/inst/args.rs`), so the crate is now vendored with those four constants
    spelled as exact power-of-two divisions — nothing else changed (kernel/vendor/README.md); the aarch64
    build does not even compile that file. Verified on QEMU riscv64 `virt`: the demo runs entirely under
    Sv39 (sched/preemption, seed/hello/async/entropy deserialize paths) and the on-target codegen step
    compiles the seed component in ~99 ms and runs `hello()` / `add(17,25) -> 42` from W^X pages;
    interactively, `time.frozen --now-seconds 5 --monotonic-ns 0 $ hello` composes, compiles on-target, and
    prints the frozen instant (`[5.000000000] Hello, frozen!` → `ok: greeted`), and plain programs and
    `exit` behave as before. aarch64 re-verified unchanged on the same branch (demo incl. on-target codegen,
    interactive with a Ctrl-C kill, `pci program=lspci` → 3 devices). riscv64 image with the full feature
    set: ~34.7 MB. Remaining for the port: milestone 6 parity checks — Ctrl-C / RX-ring consumption while a
    foreground child runs on riscv64, and an idle host-CPU measurement to confirm the WFI path is as quiet
    as aarch64's.
49. **riscv64 milestone 6: interactive parity verified — Ctrl-C, idle power, and the first-byte root cause
    (2026-05-29, branch `area/12-riscv64-m6`).** The riscv64 port is now at interactive parity with
    aarch64; no kernel code changes were needed (the branch's delta is one stale module-header comment in
    `arch/riscv64/boot.rs` plus this record).
    - *Ctrl-C while a foreground child runs:* verified on QEMU riscv64 — `cruncher --seed 9 --rounds
      100000000000` at the eosh prompt, `0x03` over the serial pipe → `abnormal: killed`, the prompt
      returns, `hello --name afterctrlc` runs (`ok: greeted`), `exit` → clean SBI shutdown. The shared
      `task.wait` interrupt-key check (D39) consumes the riscv64 16550 RX ring exactly as it does the
      PL011's; nothing arch-specific was missing.
    - *Idle power:* with the kernel parked at an idle eosh prompt, host `qemu-system-riscv64` measures
      **0.0 % CPU across 5 samples** (4 s apart, `ps -o %cpu`), identical to `qemu-system-aarch64`
      re-measured the same way on the same machine — the masked-`wfi` + SBI-timer/RX-interrupt wake design
      is as quiet as the aarch64 original.
    - *The "first byte of the first line is dropped" report (m5 review):* root-caused, and it is not a
      kernel RX bug. When a scripted session pipes its input from the moment QEMU starts, the host chardev
      parks the first byte in the 16550 receive register before any guest code has run; OpenSBI's own UART
      bring-up (FIFO enable + clear) then discards it before the Eo9 kernel exists, so the kernel never
      sees it. Once the kernel is running the path is lossless: the same script synchronized to the prompt
      (sleep until `eosh>` appears, as the QEMU smoke scripts already do for every other case) delivers the
      full first line — `hello --name rv64 --excited true` → `ok: greeted` — and the Ctrl-C transcript
      above types three further full lines without loss. aarch64 is only "immune" because it boots with no
      firmware stage: the parked byte survives in the PL011 until the kernel drains it (verified: the same
      immediate pipe on aarch64 keeps its first byte). Convention recorded: scripted serial sessions should
      wait for the prompt before sending input (which is also what a human at the console does); there is
      nothing for the kernel to fix and no detection possible (the byte is destroyed before handoff, with
      no overrun flag left behind).
    - *Deliberate differences from aarch64, unchanged by this milestone:* S-mode under OpenSBI rather than
      bare EL1 (so SBI calls for timer/power and a firmware stage that owns early console bring-up), PLIC +
      16550A instead of GICv2 + PL011, the hard-float trap frame, the fixed 10 MHz timebase constant, and
      the kernel image remaining RWX under Sv39 exactly as it is under the aarch64 map (D41/D48). Everything
      observable at the shell — outcomes, digests, entropy streams, preemption, Ctrl-C, on-target codegen,
      idle behaviour — matches aarch64.

50. **The metal storage stack demo: `disk.virtio $ fs.eofs $ <program>` on QEMU aarch64 (2026-05-29, branch
    `area/08-virtio-blk`).** The kernel itself needed no changes — the eo9:pci provider (D43) was enough to
    write a working virtio-blk driver as a guest component (plan/09 D16). What this branch adds around it:
    `KERNEL_STORE_COMPONENTS` gains `disk.virtio` and `fs.eofs` (store now 16 entries; aarch64 image
    29.0 MB → 31.5 MB, the precompiled artifacts dominating as before), and `cargo xtask qemu <arch> …`
    accepts a bare `disk` argument — consumed by xtask, never forwarded to the kernel command line — that
    creates a blank 64 MiB raw scratch image under `kernel/target/eo9-scratch-disk.raw` on first use and
    attaches it as `-device virtio-blk-pci,drive=…,disable-legacy=on` (modern only: the provider rejects
    I/O-space BARs, so the legacy/transitional flavour is deliberately not offered). The existing flows are
    untouched: without `disk` no drive is attached, so the lspci smoke still sees exactly 3 functions, and
    the demo/interactive/program= transcripts on both architectures are unchanged (re-verified). Verified
    end to end with `cargo xtask qemu aarch64 pci disk`, interactive: `disk.virtio $ fs.eofs $ ls` →
    formats the blank disk, `listed(0)`, with the driver's probe line (131072 sectors, queue size 16);
    `… $ readwrite /hello.txt eo9-on-real-disk` → `round-tripped(16)`; `… $ cat /hello.txt` → prints the
    contents; and after a full QEMU power cycle a fresh boot shows `ls` → `hello.txt`, `cat` → the same
    contents — real persistence through a wasm driver, an on-target-compiled composition, and eofs's
    root-flip commits. Follow-ups recorded: interrupt delivery (MSI/INTx → GIC/PLIC) for a non-polled
    driver, a FLUSH-on-commit story for durability against host crashes, virtio-net as the sibling driver
    feeding the `eo9:net` l2 layer, and store-on-disk so the artifact cache itself can live on the virtio
    device.

51. **The metal network demo: `net.virtio $ l2check` on QEMU aarch64 (2026-05-29, branch
    `area/09-virtio-net`).** Like the storage stack (D50), the kernel needed no changes — the eo9:pci
    provider was enough for a working virtio-net driver as a guest component (plan/09 D17). What this
    branch adds around it: `KERNEL_STORE_COMPONENTS` gains `net.virtio` and `l2check` (store now 18
    entries), and xtask's `qemu` accepts a bare `net` argument — consumed by xtask, never reaching the
    kernel command line — that attaches a modern virtio-net PCI function backed by QEMU user-mode
    networking (`-netdev user,id=eo9net -device virtio-net-pci,netdev=eo9net,disable-legacy=on`), so
    `cargo xtask qemu aarch64 pci net` is the whole recipe. With the explicit netdev QEMU suppresses its
    default transitional NIC, so the no-flag PCI view (and the `lspci` count of 3) is unchanged. Verified
    on QEMU aarch64 metal: the composed `net.virtio $ l2check` compiles on-target, probes the NIC, and
    ARP-resolves the slirp gateway (`10.0.2.2 is at 52:55:0a:00:02:02`); the aarch64 and riscv64 demo
    transcripts and the aarch64 interactive/lspci paths are unchanged (the riscv64 store also carries the
    two new components — the riscv64 PCI story itself is still the recorded follow-up, since the ECAM
    bring-up in `src/pci.rs` is aarch64-virt-specific). Follow-ups unchanged from D50 plus: a riscv64 PCI
    provider if drivers should run there too, and the l3/l4-over-l2 middleware as the first real consumer
    of this link layer.

52. **x86_64 port, milestones 1–2: PVH boot + serial + heap, timer + interrupt delivery + event-driven idle
    on QEMU `q35` (2026-05-29, branch `area/12-x86-64`).** The third architecture follows the same ladder as
    riscv64 (D45), reusing the D44 arch split unchanged — the shared core, heap, and wasm layers needed no
    edits. `cargo xtask build-kernel x86_64` builds the feature-less image for `x86_64-unknown-none` (linked
    at 2 MiB, `linker-x86_64.ld`) and `cargo xtask qemu x86_64` boots it under
    `qemu-system-x86_64 -M q35 -no-reboot -nographic`.
    - **Boot path: PVH direct boot, not Multiboot.** QEMU's `-kernel` only loads 32-bit Multiboot ELFs, so
      the image instead carries the `XEN_ELFNOTE_PHYS32_ENTRY` note (in a PT_NOTE segment the linker script
      declares explicitly): QEMU jumps to the 32-bit `pvh_start` stub with paging off and `%ebx` pointing at
      the `hvm_start_info`. The stub loads its own GDT, points CR3 at a statically assembled 0..4 GiB
      identity map (2 MiB pages), enables PAE + EFER.LME + paging, far-returns into 64-bit code, zeroes
      `.bss`, installs the IDT, and hands the PVH command line to `kmain`. The command line is a plain
      NUL-terminated string rather than a DTB, so `fdt::bootargs` gained a final, magic-checked C-string
      fallback (unreachable on the device-tree architectures).
    - **Per-arch surface** (`src/arch/x86_64/`): COM1 16550 over port I/O with the same interrupt-filled RX
      ring as the other ports; the TSC (PIT-calibrated once at boot, ~1.0 GHz under TCG) as the monotonic
      counter; PIT channel-0 one-shots as the wake timer (a single shot caps at ~54.9 ms — longer waits wake
      early and re-arm, which the executor treats as a spurious wake; the LAPIC one-shot timer is the
      recorded upgrade); the legacy 8259 PIC remapped to vectors 0x20..0x2F with only IRQ 0 (PIT) and IRQ 4
      (COM1) unmasked — materially simpler than LAPIC+IOAPIC for two lines, with that as the recorded
      upgrade path; the CMOS RTC for the wall clock; ACPI S5 via the q35 PM register (port 0x604) for a
      scriptable power-off; `sti; hlt` as the lost-wakeup-free idle step. CPU exceptions dump
      vector/error/RIP (+CR2 for page faults) and park; the IDT is installed by the boot path so even
      pre-`interrupts_init` faults are loud rather than silent triple-fault exits.
    - **Two x86-only build quirks**, both documented in the new `kernel/.cargo/config.toml` and the linker
      script: `x86_64-unknown-none` defaults to position-independent code, so the kernel builds with
      `-C relocation-model=static` (matching what the other bare-metal targets already default to); and the
      PIC-compiled prebuilt core/alloc reach `memcpy` and friends through a GOT, which must be placed
      explicitly in the image — left as an orphan section it lands after `__heap_start` and the heap
      allocator clobbers it (the silent-power-off failure mode that cost the bring-up an evening).
    - **Verified** (`cargo xtask qemu x86_64`): banner with calibrated TSC frequency and correct CMOS wall
      clock, 509 MiB heap + self-test, 10 ms PIT condition polled, "timer interrupt delivered through the
      trap path after ~15 ms" (IDT + PIC + PIT end to end), `cmdline:` echoed from `-append` via the PVH
      string, clean ACPI S5 power-off (QEMU exits 0). aarch64 and riscv64 demo runs are byte-for-byte
      unaffected; `cargo xtask ci` now builds and clippy-checks the feature-less kernel for all three
      bare-metal targets. W^X stays deferred exactly as riscv64's was pre-Sv39: the boot map is 2 MiB RWX
      pages without NXE, and the codegen milestone replaces it with 4 KiB tables + NX. Milestone 3 (host-AOT
      components) needs the precompile pipeline pointed at `x86_64-unknown-none` (cranelift's x86_64 backend
      is the host backend, so no extra feature), the kernel's wasm features built for the target — the
      target disables SSE while Cranelift-generated code uses it, so the wasm milestones likely need a
      custom target spec or `-C target-feature=+sse,+sse2` plus FP state handling in the trap entry — and
      the UART RX ring consumed by the executor (wired but consumer-less today, as on riscv64 at this
      stage).

53. **The metal sockets demo: `net.virtio $ net.l4.over-l2 $ l4check` on QEMU aarch64 (2026-05-29, branch
    `area/09-net-l4-over-l2`).** The kernel store gains the TCP/IP middleware and its check program
    (`KERNEL_STORE_COMPONENTS` += `net.l4.over-l2`, `l4check`; 20 entries, 1709 KiB of components +
    13578 KiB of precompiled artifacts; aarch64 image ≈ 34.1 MiB). With the same `pci` boot grant and
    xtask `net` flag the virtio-net demo already uses, the metal shell can now compose the full transport
    stack and compile it on-target: the session prints the driver's probe line and then
    `ok: resolved("example.com is 172.66.147.243; tcp 10.0.2.2:9 -> ConnectionRefused")` — a UDP DNS query
    answered through the wasm driver + wasm TCP/IP stack + slirp, and a refused TCP SYN surfaced as a
    typed error instead of a trap. No kernel source changes; the riscv64 image carries the same store
    additions (its PCI provider remains the recorded follow-up). Scripted sessions keep the D49
    convention: wait for the prompt, pace the input.

54. **x86_64 milestones 3–4: host-AOT components run and the baked store boots eosh on QEMU q35
    (2026-05-29, branch `area/12-x86-64-m3`).** The same step riscv64 took in D47, with one x86-specific
    wrinkle. `cargo xtask build-kernel x86_64` now runs the full host-AOT precompile pipeline (seed, hello,
    the async pair, the 18-component store image) targeted at `x86_64-unknown-none` and builds the kernel
    with `wasm-seed,wasm-hello,wasm-async,wasm-store` (no `wasm-codegen`: on-target codegen and the 4 KiB
    W^X tables are milestone 5, so `$`/`&` refuses with the documented message). Emitting x86_64 from this
    project's aarch64 development hosts needs the non-host Cranelift backend, so `build-kernel x86_64`
    re-runs itself with the existing off-by-default `kernel-cross-aot` feature exactly as riscv64 does (an
    x86_64 host would not need it).
    - **The SSE/float-ABI question (D52) resolved without a custom target spec.** The kernel stays on plain
      soft-float `x86_64-unknown-none` — so its own Rust code (including every interrupt handler) can never
      touch XMM state, which is why the trap entry still saves no FP registers — and the boot stub now
      enables SSE in hardware (clear CR0.EM/TS, set CR0.MP, set CR4.OSFXSR|OSXMMEXCPT) before any
      Cranelift-generated code can run. wasmtime refuses to load native code on this target by default
      because of the soft-float ABI; the one boundary where a float would cross in a register is a float
      libcall (f32/f64 ceil/floor/trunc/nearest emitted when the compilation target lacks SSE4.1), so
      xtask's `precompile_for_kernel` enables `has_sse3/ssse3/sse41/sse42` for this target only — no
      artifact can contain such a libcall — and the kernel engine opts in via `x86_float_abi_ok` (its
      documented safe condition) plus a CPUID-backed `detect_host_feature` probe so the enabled ISA flags
      are verified against the real CPU at load time. The QEMU invocation gains `-cpu max` (as on aarch64)
      so SSE4.2 is present under TCG. aarch64/riscv64 artifacts and engines are untouched (the flags are
      keyed on the x86_64 target string).
    - **Verified** (`cargo xtask qemu x86_64 …`): `program=hello name=pvh excited=true` → `Hello, pvh!` /
      `success(greeted)` (42 ms instantiate+main); `program=cruncher seed=9 rounds=200000` →
      `success(digest(14341732361190694547))` — the same digest as the other targets; `demo` → the
      fuel-sliced sched/preemption demo (short and long finish, spinner `abnormal(killed)`), the seed
      component (`add(17,25) -> 42`), the hello program, the sleepy canary (50 ms await observed at
      62.2 ms), and entropy.seeded sync-configure with the exact SplitMix64 values; and an interactive
      session — the baked store boots **eosh on x86_64**: `ls /bin` lists 18 programs, `hello` greets, a
      `$` composition refuses with the no-codegen message, a runaway cruncher dies to **Ctrl-C**
      (`abnormal: killed`, the prompt returns and the next program runs — the COM1 RX ring and the shared
      `take_ctrl_c` path work as-is), `exit` → clean ACPI S5 power-off. aarch64 and riscv64 demos re-run
      unchanged on the same branch (exact digests/entropy values, on-target codegen, clean power-off);
      `cargo xtask ci` green. Milestone 4's boot-to-eosh goal is covered by the store image above.
    - **Remaining for the port:** milestone 5 — 4 KiB tables with NX (replacing the 2 MiB RWX boot map),
      W^X for published code, and `wasm-codegen` with Cranelift's x86_64 backend on-target (the engine-side
      ISA flags must then mirror the precompile set so on-target output matches what the CPU probe
      accepts); milestone 6 — parity checks (idle-power measurement; Ctrl-C already verified above). The
      full-feature x86_64 cargo build emits the same pre-existing dead-code warning class the riscv64 one
      does (`arch::NAME` unused there) — cargo-build only, outside the clippy gate.

55. **x86_64 milestone 5: 4 KiB NX page tables, W^X for published code, and on-target codegen
    (2026-05-29, branch `area/12-x86-64-m5`).** The boot stub's static 2 MiB RWX identity map is now used
    only to reach `kmain`; `arch/x86_64/mmu.rs` builds runtime tables and switches `CR3` over early in
    boot. Layout: the 512 MiB of RAM at **4 KiB granularity** (one PD → 256 leaf tables) — low RAM
    (firmware/PVH structures) RW-NX, the kernel image + boot stack + the tables themselves RWX (trusted,
    like the other ports), the heap RW-**NX**; the rest of the first 4 GiB (LAPIC/IOAPIC windows, q35
    ECAM) as 2 MiB RW-NX device pages; above 4 GiB unmapped. `EFER.NXE` is enabled (the boot map carries
    no NX bits, so turning it on first is harmless) and `CR0.WP` is set so ring 0 honours read-only
    pages — that is what makes the published-code flip real on x86. `set_range_permissions` rewrites the
    4 KiB leaf PTEs and `invlpg`s each page; `flush_code_range` stays a documented no-op (x86 keeps
    instruction fetch coherent with same-CPU stores; aarch64/riscv64 need real maintenance there). The
    publisher path is unchanged — heap allocations are now genuinely non-executable until
    `publish_executable` flips them to read-only-executable, and `unpublish_executable` returns them to
    RW-NX, so deserialized and on-target-compiled code never sits writable+executable. On-target codegen:
    `build-kernel x86_64` now builds the full feature set (`…,wasm-codegen`) and ships the raw seed wasm
    for the codegen demo, exactly like riscv64; the kernel engine, when the compiler is linked on x86_64,
    enables the same `has_sse3/ssse3/sse41/sse42` ISA flags as xtask's precompile (so on-target-generated
    code also never emits float libcalls — the `x86_float_abi_ok` condition from D54 — and engine and
    artifact flags agree), with the CPUID probe still verifying them at load time. **No vendored changes:**
    the existing cranelift fork's x86_64 backend builds no_std as-is. Verified on QEMU q35: the demo runs
    every prior step unchanged plus `wasm codegen: compiled on-target in ~95 ms`, `add(17,25) -> 42` from
    W^X pages; interactively, `time.frozen --now-seconds 5 --monotonic-ns 0 $ hello --name frozen
    --excited true` composes, compiles on-target and prints `[5.000000000] Hello, frozen!` / `ok: greeted`,
    and `entropy.seeded $ cruncher --seed 9 --rounds 200000` gives the cross-target digest
    `14341732361190694547`; clean ACPI S5 power-off. aarch64 and riscv64 demos re-run unchanged. Idle
    (milestone 6's measurement): `qemu-system-x86_64` sits at ~0.2 % host CPU at an idle eosh prompt
    (`sti; hlt` wake path; same effectively-zero class as the other ports), so no further idle work is
    needed — the x86_64 port is at functional parity with aarch64 and riscv64.

56. **The eo9:pci provider works on riscv64: per-architecture PCI map (2026-05-29, branch
    `area/12-pci-interrupts`).** The hardware half (`src/pci.rs`) had the QEMU aarch64-`virt` addresses baked
    in; they now come from a `pci_map` module on the per-architecture surface (`crate::arch::pci_map`,
    wasm-store builds only): aarch64 keeps ECAM `0x3f00_0000` / BARs `0x1000_0000..0x3eff_0000` (with the
    existing `highmem=off` pin), riscv64 `virt` gets ECAM `0x3000_0000` / BARs `0x4000_0000..0x8000_0000`
    (fixed addresses, no highmem dependence), and x86_64 `q35` carries documented-but-unwired MMCONFIG
    values so the shared module compiles there (the x86_64 QEMU invocation does not pass the `pci` grant).
    The riscv64 Sv39 map gains a second RW-NX gigapage over `0x4000_0000..0x8000_0000` so BARs the provider
    assigns are reachable; the ECAM was already inside the MMIO gigapage. xtask's riscv64 QEMU invocation
    adds `-device virtio-rng-pci`, mirroring aarch64's enumeration baseline. Containment is unchanged: the
    provider still only links when the boot command line carries the bare `pci` token. Verified on QEMU
    riscv64: `pci program=lspci` → host bridge `1b36:0008` + virtio-rng `1af4:1005`, `success(devices(2))`
    (riscv64 `virt` adds no default NIC, unlike aarch64); and the full storage stack interactively —
    `disk.virtio $ fs.eofs $ readwrite /rv64.txt …` → `round-tripped(17)`, `… $ ls` → `rv64.txt`,
    `… $ cat /rv64.txt` → the contents — composed and compiled on-target against a riscv64 virtio disk.
    aarch64 `pci program=lspci` still reports its 3 devices; the aarch64, riscv64, and x86_64 `demo` runs
    are unchanged (same digests, exact entropy values, clean power-offs). Reading the ECAM base from the
    DTB/MCFG instead of per-arch constants remains the recorded follow-up.

57. **PCI interrupt delivery: no WIT gap, kernel design recorded, deferred with its driver proof
    (2026-05-29).** The eo9:pci API already carries `enable-interrupts(device, kind, count)` and
    `wait(interrupt)` (wit/pci), so exposing interrupts needs no WIT change; the kernel still answers
    `unsupported` (D43b). The implementation the next pass needs: read the Interrupt Pin register, apply the
    standard `(slot + pin − 1) mod 4` swizzle onto the `virt` gpex lines (GIC SPIs 35–38 on aarch64, PLIC
    sources 0x20–0x23 on riscv64); on delivery mask the level-triggered line at the controller, EOI, and
    count it; `wait` returns the count and re-arms (unmasks) on the next call, after the driver has cleared
    the device-side cause; `wait` itself parks on the same drive-loop/wfi machinery the timer path uses.
    MSI (GICv2m on aarch64) is cleaner long-term but riscv64 has no MSI path without AIA, so INTx covers
    both verified arches first. Deferred from this branch because the honest end-to-end proof — converting
    `disk.virtio`'s used-ring poll to `enable-interrupts`/`wait` and measuring the difference — is a guest
    driver change that belongs in the same verified pass as the kernel machinery. The `pci.filtered`
    attenuator landed separately (plan/09 D19); its allow-list `configure` is currently blocked by the
    compose-time configuration binder supporting only scalars/strings/enums (see that decision).

58. **The persistent store disk: a disk-backed cache of on-target compile results (2026-05-29, branch
    `area/12-store-on-eofs`) — store-on-eofs, first rung.** Booting with the bare `storedisk` command-line
    token (attached by the new xtask QEMU argument of the same name, which also keeps the token on the
    cmdline) makes the kernel claim a virtio-blk function for itself, mount the eofs filesystem on it
    (formatting **blank** disks in place; anything unrecognized is left untouched and the cache stays off),
    and from then on the shell's `compile` operation consults `/cache/<blake3-of-the-executable-bytes>.cwasm`
    before invoking Cranelift: a hit deserializes the artifact written by an earlier boot, a miss compiles
    and writes the artifact back (commit per store). Measured on the acceptance demo
    (`time.frozen … $ hello` at the interactive prompt): first boot `compile cache miss (compiled on-target
    in 1497 ms)` + `cached 677 KiB`, second boot — a full QEMU power cycle later — `eofs mounted (txg 2),
    1 cached compile artifact(s)` + `compile cache hit (677 KiB loaded in 2122 us)`, same
    `[5.000000000] Hello, cached!` output. Pieces and choices:
    - **In-kernel polled virtio-blk driver** (`src/virtio_blk.rs`): mirrors the `disk.virtio` wasm driver's
      modern bring-up, three-descriptor requests, and polled used ring, but runs against `src/pci.rs`
      directly with heap-allocated, identity-mapped DMA regions (volatile ring access, fences around
      publish/notify/poll). The wasm driver remains how *programs* get a disk; this one exists because the
      compile cache is kernel infrastructure that must work while the shell's own store is mid-call. This is
      the "in-kernel Rust drivers only for boot-critical devices" carve-out the PCI provider review
      anticipated.
    - **eofs as the on-disk format** (`wasm/diskcache.rs`): the kernel now links `eofs-core` (and `blake3`)
      under the new `wasm-storedisk` feature — the same no_std engine the fs.eofs guest provider and usermode
      mkfs use, so a store disk formatted by any of them is mutually readable. Cached artifacts live under
      `/cache`. Artifacts over 8 MiB are not cached.
    - **Trust/containment — disk bytes are never deserialized unverified.** `Component::deserialize` trusts
      its input (a tampered artifact is arbitrary native code), so every cache entry is written with a header
      carrying a keyed-blake3 tag over `cache-key || length || artifact` (binding the entry to its own name, so a valid entry cannot be copied over a different entry's path to make one composition run another's artifact); the 32-byte key is generated per checkout by
      xtask (`kernel/target/eo9-storedisk-mac.key`, /dev/urandom, never committed, never on the disk) and
      baked into the kernel image, and `lookup` recomputes and compares the tag (constant-time via
      `blake3::Hash` equality) **before** the artifact is returned for deserialization. A mismatch is a
      printed, typed refusal that falls back to recompiling, which overwrites the entry; deleting the key
      file and rebuilding rotates the key and simply invalidates the whole cache. The key file is created
      owner-only (mode 0600 at create), but note the key is also embedded in the kernel image itself, so its
      secrecy is inherently limited — the keyed tag is **tamper-evidence for the disk**, not a secret-key
      security boundary (anyone who can read the kernel image can forge tags; anyone who can replace the
      kernel image never needed to). The defense layering is:
      eofs's unkeyed block checksums catch plain corruption, the keyed tag catches an adversary who can
      rewrite blocks *and* recompute those checksums but lacks the in-image key, and deserialize's own
      compatibility checks catch artifacts from a different wasmtime build. The baked-in store image and the
      kernel's own in-memory compile results stay untagged — same trust domain as the kernel image. The disk
      is claimed only via the explicit boot token and is never exposed to guests; guests still need
      `pci`+`disk.virtio` for their own disks. Sharing one virtio-blk function between the kernel store and a
      guest driver in the same boot is unsupported until machine-global device claiming lands (the xtask doc
      says so), so the demo uses a separate image file (`kernel/target/eo9-store-disk.raw`) from the guest
      scratch disk.
    - **Feature scoping**: `wasm-storedisk` is enabled by xtask for the **aarch64** full build only (the ECAM
      bring-up in `src/pci.rs` is aarch64-virt specific); riscv64/x86_64 builds, the featureless CI gate, and
      any boot without the token are bit-for-bit unaffected (verified: all three `demo` transcripts unchanged,
      full `cargo xtask ci` green).
    - **Remaining for later rungs**: shell-visible store additions (`store add`-style bindings and programs on
      the disk, surfaced under /bin), VIRTIO_BLK_F_FLUSH for commit durability, eviction/space management for
      the cache directory, riscv64/x86_64 enablement once their PCI bring-up exists, and the
      kernel-claims-vs-guest-claims arbitration (machine-global device claiming).

59. **PCI INTx delivery is live, and disk.virtio completes requests on interrupts (2026-05-30, branch
    `area/12-driver-interrupts`) — D57 implemented, with one deliberate adaptation.** The `eo9:pci`
    provider's `enable-interrupts`/`wait` are no longer `unsupported` on aarch64 and riscv64:
    - **Routing.** `enable-interrupts(device, intx, 1)` reads the function's Interrupt Pin register, clears
      the command register's INTx-disable bit, and applies the standard `(slot + pin − 1) mod 4` swizzle to
      pick the gpex line — GIC SPIs 35–38 on aarch64 `virt`, PLIC sources 0x20–0x23 on riscv64 `virt`
      (`arch::pci_intx`, the per-arch mask/unmask surface). x86_64 keeps `unsupported` (`WIRED = false`):
      its q35 PCI grant is not wired and legacy PIRQ routing through the 8259/IOAPIC is its own project.
    - **Delivery protocol.** The arch IRQ/trap handler masks a fired level-sensitive line at the controller
      (the device keeps asserting until the driver clears its cause) and counts it in the arch-independent
      `pci::intx_record`; `wait` unmasks the line, blocks until a delivery / a console Ctrl-C / a 2 s bound,
      consumes the count, and returns with the line masked again; the next `wait` — after the driver reads
      virtio's ISR register, which clears the cause — re-arms it. The kernel prints one line per boot the
      first time a delivery actually serves a wait, so every transcript shows whether completions are
      interrupt-driven.
    - **The adaptation: `wait` blocks host-side rather than suspending the task.** D57 sketched `wait`
      parking on the drive loop the way `time.sleep` does. That is incompatible with how every existing
      driver chain executes: `fs.eofs` polls its disk import eagerly (single poll, plan/14 D19) and
      `disk.virtio`/`net.virtio`/`net.l4.over-l2` poll their pci/l2 imports eagerly (plan/09 D16–D18), so a
      `wait` that returned `Pending` would make the exported disk/socket operation itself suspend — and the
      consumer above it would fail with "the provider suspended". Instead `wait` blocks inside the host call
      on the same masked-`wfi` + timer primitives the executor's idle path uses (`timer::wait_for_interrupt`):
      the CPU halts (instead of the guest spinning host calls) and the PCI interrupt, a keystroke, or the
      timer slice wakes it. Cost: while one task is blocked in `wait`, siblings and the shell do not run —
      acceptable because the storage/net stacks are single-request run-to-completion by design (and a console
      Ctrl-C aborts the block within an interrupt's notice, after which the driver's polled fallback is
      preemptible as before). A properly task-suspending `wait` becomes possible when the async disk/net
      bridge (plan/14 follow-up) lands; the controller-side machinery is identical either way.
    - **disk.virtio conversion.** The driver discovers the virtio ISR window, asks for one INTx vector at
      bring-up (feature-detected: `unsupported` or any failure → the polled used-ring loop, so the same
      driver binary works on x86_64 or any provider without routing), and per request: check the used ring,
      `wait` for a delivery (up to 4 retries), read the ISR to deassert, re-check; any wait failure falls
      back to the polled loop. The probe line now reports the completion mode
      (`completion: INTx interrupt` / `polled`). net.virtio still polls (its conversion is the natural
      follow-up; same recipe).
    - **Verified** (QEMU, fresh blank scratch disks): aarch64 `pci disk` interactive —
      `disk.virtio $ fs.eofs $ readwrite/ls/cat` round-trips with
      `pci: INTx delivery on line 3 served an interrupt wait` in the transcript; riscv64 `pci disk` — the
      same storage session interrupt-driven through the PLIC (line 2); aarch64 `pci net` ARP demo, aarch64
      `pci program=lspci`, and all three arch `demo` runs unchanged (same digests, exact entropy values,
      on-target codegen, clean power-offs); full `cargo xtask ci` green.
    - **Measurement (honest).** End-to-end wall-clock and QEMU CPU time for the three-command storage
      session are within run-to-run noise of the polled baseline (~66 s wall / ~58 s CPU either way):
      on-target Cranelift compilation of the three-component composition dominates both by ~95%, so the
      completion strategy is invisible at session granularity under TCG. The structural win — the core
      halts in `wfi` during device waits instead of burning host calls — is what matters on real hardware
      and for I/O-heavy workloads, and is what the per-boot diagnostic line proves is happening.
    - **Remaining**: convert net.virtio the same way; MSI/MSI-X (needs GICv2m on aarch64, AIA on riscv64);
      x86_64 PIRQ routing; per-task interrupt-vector exclusivity alongside machine-global device claiming.
60. **A writable /bin on metal: shell-saved programs persist on the store disk (2026-05-30, branch
    `area/12-writable-bin`) — store-on-eofs, second rung.** On a `storedisk` boot, `/bin` is now a
    **two-layer** view: the baked-in store image (immutable, always shadowing) plus disk-resident programs
    saved at the shell. `save <name> = <expr>` in eosh (plan/10 D16) evaluates the expression and writes the
    component's algebra-serialized bytes to `/bin/<name>.wasm` through the ordinary `eo9:fs` capability — no
    new WIT. Pieces and choices:
    - **shellfs** (`wasm/shellfs.rs`): `open` with `create`/`write`/`truncate` flags is accepted only for
      `/bin/<name>.wasm`, only when the store disk is mounted, and never for a baked name (those answer
      `read-only` — the kernel image stays the trust anchor for the standard names). Writes are
      **write-through**: every `write` call re-tags and commits the accumulated content, so the result the
      guest sees reflects whether the program is actually on disk (no destructor-swallowed failures). The
      write path always replaces entries wholesale (truncate-on-open semantics). `remove` deletes
      disk-resident entries (so `rm /bin/<name>.wasm` works); listings, stat, open, and open-exec serve both
      layers; resolution order is baked-then-disk. Without the `wasm-storedisk` feature (riscv64/x86_64,
      featureless CI) a module-local inert shim keeps the code identical and every mutation answers
      `read-only` exactly as before.
    - **diskcache** (`wasm/diskcache.rs`): saved programs live under `/bin` on the eofs disk with the **same
      keyed-tag entry format** as the compile cache (magic ‖ length ‖ keyed-blake3 tag ‖ payload), tag key
      `bin/<name>` so a `/bin` entry can never be replayed as a `/cache` entry or under another name.
      Component bytes are wasm (the compiler validates them), but the tag still matters: without it, someone
      who can rewrite disk blocks could swap what a saved name resolves to — e.g. replace an
      `only`-attenuated composition with an unattenuated one. Saved programs are capped at 4 MiB; names are
      validated (dotted shell-name shape) on every operation.
    - **Spawn path**: a saved program resolves through the existing fs open-exec → algebra load → on-target
      compile path, so the compile cache (D58) gives saved compositions ~2 ms reboot-warm starts for free —
      `save` + the cache together are what make the metal shell feel installed rather than ephemeral.
    - **rm joins the baked store** (xtask `KERNEL_STORE_COMPONENTS`, store now 22 entries): it completes the
      lifecycle (`save` → `ls /bin` → run → `rm /bin/<name>.wasm`); on baked names it is inert (read-only
      refusal). All three architecture demos re-verified with the grown store.
    - **Acceptance transcript (QEMU aarch64, `storedisk` boot)**: `save frozen-hello = time.frozen
      --now-seconds 5 --monotonic-ns 0 $ hello` → `saved: /bin/frozen-hello.wasm`; `ls /bin` → 23 entries;
      `frozen-hello --name saved --excited true` → compile cache miss (1.7 s) → `[5.000000000] Hello,
      saved!`; **full QEMU power cycle** → `eofs mounted (txg 3), 1 cached compile artifact(s), 1 saved
      program(s)`; `frozen-hello --name reboot2 --excited true` → cache hit (677 KiB in 2.1 ms) → correct
      output; `rm /bin/frozen-hello.wasm` → removed, `ls /bin` back to 22, resolution now NotFound;
      `rm /bin/hello.wasm` → `read-only` (baked names protected).
    - **Containment note**: the disk `/bin` is writable session-wide — any program granted `eo9:fs` can
      create or remove disk entries, mirroring usermode's writable `--fs-root` layer. Writing a file never
      executes it (running a saved name still goes through resolve → load → compile → spawn under the
      session's grants), and the baked layer cannot be shadowed or removed. The store disk remains
      kernel-claimed and is never exposed to guests as a raw disk.
    - **Remaining for later rungs**: eviction/space management for both `/cache` and `/bin`,
      VIRTIO_BLK_F_FLUSH durability, riscv64/x86_64 enablement (needs their PCI bring-up in the kernel's
      claiming path), and surfacing disk-vs-baked provenance in listings (`ls /bin` shows both layers
      undifferentiated today; the session manifest carries the layering note instead).

61. **Kernel executors call the configured bind entrypoint (2026-05-30, plan/03 D23).** `shellexec`'s
    `spawn_child` and the headless `runner` call `eo9:rt/configured.bind` (via the shared
    `wasm::bind_entrypoint` lookup) after instantiation, before `main` — under the spawn fuel budget in
    `spawn_child` (mirroring usermode), via `block_on` in the runner. This is what applies compose-time
    configuration to compositions compiled on-target: verified interactively on QEMU aarch64 —
    `time.frozen --now-seconds 5 --monotonic-ns 0 $ hello --name frozen --excited true` compiles on-target
    and prints `[5.000000000] Hello, frozen!`, and `pci.filtered --allow "[{segment: 0, bus: 0, device: 1,
    function: 0}]" $ lspci` (a baked list-of-records allow-list, the previously blocked consumer) shows
    exactly `0000:00:01.0`, `ok: devices(1)` against a 3-device baseline. The demo path (raw components,
    manual configure calls) is unaffected. Note: configured artifacts have a new byte shape, so the
    storedisk compile cache misses once per composition and refills; nothing else changes.

62. **PCI teardown quiesce, attributable disk errors, and pci.none on metal (2026-05-31, branch
    `area/12-pci-teardown` — the study 09 fix-now batch).** Three fixes from the driver-developer
    user study (docs/user-studies/09-driver-developer.md):

    * **Quiesce before free (finding 6, the memory-safety hole).** `PciTables` now tracks, per open
      device, whether bus mastering was enabled through that handle, and guarantees the ordering
      "every armed device is disarmed before any of the task's DMA buffers are freed" on every path:
      `Drop for PciTables` (task completion, trap, or kill — the store drops, the Drop body quiesces,
      then the buffer storage drops), explicit `device` handle drop (revokes that device's licence),
      and explicit `dma-buffer` drop (quiesces all armed devices first, since destructor order is not
      ours to choose). Interrupt lines are masked at the same points. A once-per-boot diagnostic
      (`pci: quiesced N device(s) at task teardown …`) makes the ordering visible in transcripts.
      Verified on QEMU aarch64: `pci disk` normal completion (diagnostic prints between the task
      ending and its outcome rendering), and the study's nightmare case — Ctrl-C killing
      `net.virtio $ net.l4.over-l2 $ l4check` while the NIC has rx buffers posted and bus mastering
      on — quiesces the device before `abnormal: killed` is even printed, and the same device is
      then re-claimed and used by a fresh `net.virtio $ l2check` run (ARP resolves) in the same boot.
      Out of scope, recorded: a *driver-side* mirror (bus-master off at clean exit) has nowhere to
      live — providers have no exit hook; the kernel-side guarantee is the load-bearing one.
    * **Attributable device errors (findings 2 + 3).** `eofs-core::DeviceError` gains `IoNamed(String)`
      (Copy dropped, Clone kept; zero fallout — nothing relied on Copy), `fs.eofs` maps the disk API's
      `Io(message)` errors onto it instead of flattening to the unit `Io`, and a disk reporting size 0
      gets one probe read so its real error surfaces instead of `invalid format options: device too
      small`. `disk.virtio`'s no-device message now names the remediation (the `disk` boot flag /
      QEMU device flag / a too-narrow attenuator). Verified: a `pci` boot without the `disk` flag now
      reports the driver's full message at the shell; the `pci disk` happy path is unchanged.
    * **pci.none baked into the store (finding 4, partial).** The absence stub joins
      `KERNEL_STORE_COMPONENTS` (23 entries). **Gap, needs a planner dispatch:** `pci.deny` — the
      typed-refusal stub SPEC promises and wit/pci's `deny` world fully specifies — has no stub crate
      anywhere (usermode or metal); the study could not distinguish "not in the store" from "does not
      exist" because it does not exist. Creating it is one mechanical crate following the
      net-l2-deny pattern.
    * The DMA contract (finding 9) is recorded as a proposed `wit/pci` doc-comment addition in
      plan/02 D22 and stated in disk.virtio's module docs for driver authors.

63. **IOMMU spike: SMMUv3 is incremental, ~1k lines, and the only real-hardware path
    (2026-06-01, branch `spike/12-iommu`, `docs/spikes/iommu.md`).** Owner-approved
    investigation of study 09's no-IOMMU finding, run against QEMU 11's `-M virt,iommu=smmuv3`.
    Findings: (1) the kernel boots and runs the full demo, lspci, and the storedisk DMA path
    completely unchanged with the SMMU present — QEMU resets `GBPA.ABORT = 0`, so an
    unprogrammed SMMU is global bypass, which means IOMMU support has no flag day and can land
    in independently-verifiable steps; (2) the SMMU's MMIO (0x0905_0000, 128 KiB) is already
    inside the kernel's identity-mapped device gigabyte, and the DTB's `iommu-map` is the
    identity RID→StreamID mapping, so a linear stream table suffices; (3) a minimal stage-2-only
    polled driver is ~800–1,200 lines (register bring-up, linear stream table, command/event
    queues, a stage-2 page-table builder adapted from `mmu.rs`, and pci-provider lifecycle
    integration) — no new concepts beyond what `pci.rs`/`virtio_blk.rs` already established;
    (4) with stage-2 translate + `GBPA.ABORT = 1`, out-of-bounds device DMA becomes an
    `F_TRANSLATION` event-queue fault attributable to the owning task instead of silent memory
    corruption, closing the study's "disqualifying" finding structurally; (5) virtio-iommu would
    be ~half the work but is a QEMU-only dead end — real aarch64 silicon has SMMUv3 (or nothing),
    so that effort transfers nowhere. Recommendation: build the SMMUv3 driver as part of
    real-board prep (not before; nothing on the roadmap is blocked by it), per the four-step
    incremental plan in the spike doc. `wit/pci` needs no changes. The xtask `iommu` argument
    added by the spike is experimental and aarch64-only; promote it to a documented flag when
    step 1 of the incremental plan lands.

64. **Spectre bounds-check mitigation audit (2026-06-01, `docs/spikes/spectre-audit.md`).** Cranelift's
    speculative bounds-check masking is ON in usermode (defaults + guard-region sizing), but **forced
    OFF on every bare-metal engine and cannot be enabled**: wasmtime 45 hard-errors if the Spectre
    settings are enabled together with `signals_based_traps(false)`, because the mitigation's
    load-from-null formulation needs fault-on-null, which explicit-bounds-check mode (and our
    identity-mapped address 0) does not have. On metal the load-bearing Spectre defense is therefore
    the time capability alone; the compiler-level fix is an upstream/vendored Cranelift enhancement
    (cmov-mask formulation), a candidate for the same fork+PR path as the no_std work. The kernel's
    wasmtime configuration is also confirmed to be exactly the no-MMU configuration (no signals, no
    reservations, no guards) — the MMU is used only for W^X, never for memory safety.

65. **Executor v2 — services on metal: the kernel svc registry, boot-runs-init, and `poweroff`
    (2026-06-02, branch `area/12-svc-v2`).** The `eo9:svc` surface moves from absent-stub to real on
    store builds (`src/wasm/svc.rs`): a machine-lifetime registry (16 services, 256 KiB log rings,
    64-record failure history — usermode's constants) pumped by the session drive loop beside
    foreground children. The boot pipeline becomes kernel → init → console: `runner::boot`'s default
    branch runs `init` from the baked store with the svc grant (a per-store generation count on
    `KernelState`: init 2 → its console 1 → the console's children 0, never a default; `task.spawn`
    hands children parent−1), with the baked default config `console = eosh` — out of the box, leaving
    the shell with no services running still ends the boot, so the pre-init transcripts are preserved.
    `program=eosh` keeps the supervisor-less direct console. The `svcdemo` token swaps in the baked
    demo config (a cruncher worker under `restart.always`, a one-shot echo banner under
    `restart.never`); init, restart.never/always/backoff joined `KERNEL_STORE_COMPONENTS` (38 → 42).
    Deviations from the usermode registry, all deliberate: the policy is **compiled once at detach**
    (on-target Cranelift is ~100 ms; usermode recompiles per decision) and only instantiated per
    decision; service state reports running/waiting-restart/finished (no blocked split — the kernel
    has no per-service park introspection); the service compile path skips the storedisk cache
    (services restart from the in-memory artifact; cross-reboot caching is a follow-up); optional
    imports outside text/rt/io fail at the spawn inside detach (the service linker registers no
    optional flavors) rather than resolving to absent as usermode does. Capability soundness is
    structural: the service linker carries log-capture text, the rt riders, and io buffers — no fs,
    exec, time, entropy, pci, or svc. Three correctness fixes the work surfaced: (a) **Ctrl-C
    ownership** — `task.wait` consumes the interrupt key only when the waiter is itself a child or the
    root opted in (`set_root_consumes_ctrl_c`); init's wait on the console is exempt, so Ctrl-C still
    kills the console's foreground job (and stays a no-op at the bare prompt) instead of killing the
    console, and detached services are untouchable by it (they live outside the child table the
    kill-cascade walks); (b) **busy-pass input wakes** — with an always-runnable service the drive loop
    never idles, so `wake_idle` is now also called on busy passes; without it the console's parked
    `read-line` waker was never rung and a spinning service deafened the prompt; (c) **option-default
    parity** — `bind_args` binds an unsupplied `option<…>` parameter to `none` (the usermode binder and
    the headless runner already did; init spawns its console with no arguments). `poweroff` rides the
    supervision tree as a typed outcome (eosh's `program-success` gains `poweroff-requested`; init
    exits on it regardless of running services; the kernel then stops leftovers and powers off via the
    existing PSCI/SBI/ACPI path) — no new capability, no WIT-package change. Verified on all three
    architectures (demos unchanged; aarch64 additionally: the svcdemo battery — list/log/stop, Ctrl-C
    kills foreground not services, exit-restarts-console under ruling D, crash-restart under configured
    backoff with the trapped reason in the record, storedisk + pci flows, poweroff with and without
    services). Remaining for v3+: the L2 switch root provider; storedisk-persisted service configs +
    logs (a `storedisk` boot could read `/services.cfg`); per-service fuel budgets; the `blocked`
    state split; cross-reboot service compile caching.
66. **The display stack on metal: `gpu.virtio $ draw`, the `gpu` QEMU flag, and `check-gpu`
    (2026-06-02).** The kernel store gains the gfx family (gpu.virtio, gfx.mem, gfx.none,
    gfx.deny, draw — 5 entries), all reachable at the metal prompt with zero kernel-source
    changes: the driver is an ordinary wasm component over the existing eo9:pci provider. The
    xtask `gpu` argument attaches `-device virtio-gpu-pci,xres=640,yres=480` (geometry pinned so
    the deterministic pattern is reproducible) plus a QMP socket; `cargo xtask check-gpu` is the
    headless verification: boots `pci gpu`, types `gpu.virtio $ draw` then `… --frames 2` at the
    serial prompt (byte-at-a-time at 25 ms with echo verification — chunked pastes lose bytes
    under host CPU contention, the D49 console convention taken to its conclusion), QMP
    `screendump`s after each, and compares both PPMs pixel-for-pixel against the pattern
    computed independently in xtask. Verified end to end, plus: Ctrl-C mid-draw → the generic
    teardown quiesce covers the GPU (diagnostic line) and a second `gpu.virtio $ draw`
    re-claims and re-presents; `gfx.mem $ draw` on metal = the usermode checksum = gpu.virtio's
    readback checksum (12974149382569602461 at 640x480) — one deterministic pattern across
    every backend and target; all three arch demos and the lspci baseline unchanged (the GPU
    only exists behind its explicit flag). One driver-shaping lesson recorded: dropping ANY DMA
    buffer mid-conversation kills the device — the kernel's conservative quiesce-on-buffer-free
    clears bus mastering, and QEMU's virtio-pci clears DRIVER_OK when bus mastering drops — so
    drivers must allocate exactly what they keep (gpu.virtio's framebuffer is an Option filled
    once, never replaced; surfaced by the enriched poll-limit diagnostics, which stay).
67. **The "gfx freeze" was the silent per-spawn recompile: a session compile cache and a codegen
    indicator (2026-06-02, branch area/12-gpu-freeze).** Owner report: the first `gpu.virtio $ draw`
    in a `make gfx` window took ~20 s before drawing, and the console stopped responding around the
    third command. Reproduction across ~50 scripted draw rounds (headless, VNC- and cocoa-display,
    with host CPU hogs, type-ahead, and per-keystroke echo verification) produced **no input freeze**
    — the at-prompt input path is healthy under host load up to ~29 — but phase-timing the rounds
    found the real lesion: **without a store disk the kernel had no compile cache at all**, so every
    spawn of a composed command line re-ran Cranelift on-target (~4 s quiet, 15–60 s observed under
    a loaded host — the owner's session ran concurrently with a stress harness pinning four cores),
    and the synchronous compile blocks the entire drive loop: no echo, no Ctrl-C, no quiesce, total
    silence. Each repeat draw paid it again. That is indistinguishable from a frozen shell. (The
    transcript's "missing" INTx-wait and quiesce lines on draw #2 were red herrings: both
    diagnostics are once-per-boot by design.) Fixes: (a) the exec `compile` path keeps a small
    per-session FIFO (8 entries, hash + full-bytes equality on the fused executable bytes) of
    compiled `Component`s — a repeat composition now spawns in ~0.3 s instead of ~4 s, codegen runs
    once per session per pipeline; the storedisk cache is unchanged and a disk hit also seeds the
    session cache; (b) **codegen announces itself** — `codegen: compiling the composed component
    on-target (N KiB) …` before the stall and `codegen: compiled in N ms` after, on both the exec
    and the svc-detach compile paths, so the one remaining stall is never silent (the seed of the
    interactive-compile-feedback design recorded here rather than in plan/10: the kernel knows when
    codegen starts; richer progress would need wasmtime-side hooks). Two adjacent defects fixed
    while disproving freeze hypotheses: (c) `wake_idle`'s single last-write-wins waker slot would
    silently drop the console's parked `read-line` waker the moment any other host future
    (`time.sleep`) parked after it in the same pass — unreachable with today's baked programs (no
    service sleeps; verified: svcdemo console responsive across 20 prompt probes before and after)
    but a standing console-deafness hazard, now a drain-all list (`will_wake`-deduplicated, emptied
    on every wake, so it cannot grow); cross-checked against the usermode discarded-Ready lost-wakeup
    (crates/eo9, fixed on master): the kernel's `task.wait` is self-ringing and does **not** repeat
    that shape; (d) gpu.virtio now reads the ISR on completions consumed *without* a wait — the
    interrupt-mode path could return with the ISR unread, leaving level-sensitive INTx asserted, so
    the controller recorded a delivery the instant the next `wait` unmasked and that stale delivery
    made the wait return spuriously (instrumented runs showed the interrupt path never actually
    degrading to the polled fallback, so this is hardening, not the lesion); the polled fallback also
    announces itself once per device conversation now (a silent 50M-host-call spin reads as a hang).
    The same already-completed-no-ack shape exists in disk.virtio/net.virtio — left for the storage/
    net lanes (an active branch owns those files). Verified: draw rounds with per-keystroke echo
    probes (6 headless + 5 cocoa, repeat draws 4.5 s → ~1.1 s end-to-end), Ctrl-C during compile
    (kills the job at the first wait after codegen, `abnormal: killed`), Ctrl-C mid-draw + re-claim,
    `check-gpu` pixel-exact, `disk.virtio $ fs.eofs $ readwrite/ls` and `net.virtio $
    net.l4.over-l2 $ l4check` on metal (the chains share the INTx and exec machinery), svcdemo
    battery prompt-responsiveness, all three arch demos, full `cargo xtask ci`.

68. **The svc-detach compile path consults the session compile cache (2026-06-02, branch
    `area/09-async-gpu`; closes D67's carried follow-up).** `shellexec::compile_component`
    (the detach path's compiler) announced codegen but never consulted the in-RAM session
    cache, so detaching a fused composition the session had already compiled — at the
    prompt, or under another service name — re-paid Cranelift. It now takes the session's
    `&mut ShellExec` and runs the same discipline as the exec surface's `compile`: hash
    narrows, full-bytes equality confirms, hit skips Cranelift, miss compiles + announces +
    caches. The persistent disk cache stays out of this path (unchanged recorded follow-up:
    it would only help across reboots). Both `host_detach` call sites (child + policy) pass
    the exec state; baked store entries (every `restart.*` policy) still take the
    deserialize fast path and never touch the cache. Verified on metal: `detach a = gfx.mem
    $ draw restart restart.never` compiles once (`codegen: compiling …` announced), `detach
    b =` the identical composition detaches with **no second announce** — one codegen
    across both; full `cargo xtask ci` green.

69. **`pci.wait` parks the calling task (2026-06-02, branch area/12-pciwait-park; plan/09
    D39's unblocking work — the async-first doctrine applied to the kernel's own host
    APIs).** The INTx wait host fn no longer blocks host-side (the eager-era masked-`wfi`
    loop inside the host call, which halted *every* task — the console, detached services,
    sibling drivers — between a driver's request publish and its interrupt, and made the
    cancel window structurally unconstructible). It is now `IntxWait`, a future with
    exactly the `time.sleep` parking discipline: the arming poll unmasks the line, each
    poll consumes the IRQ handler's delivery count (handler masks-on-fire, unchanged) or
    resolves the typed bound expiry, and a pending poll registers with the executor's
    idle wakers plus `request_timer_wake(deadline)` — the interrupt wakes the `wfi`, the
    wake pass re-polls, services keep running meanwhile. There is no Ctrl-C arm anymore:
    a console interrupt kills the waiting task through the ordinary kill cascade, and
    `IntxWait::Drop` masks an armed line and drains any delivery that raced the teardown
    (no leaked unmask, no stale count for the next wait; the old typed
    "interrupted from the console" error is gone — Ctrl-C now lands as the same
    `abnormal(killed)` every other parked op produces). The once-per-boot diagnostic keeps
    its grep-stable prefix with a truthful parenthetical ("the task parked instead of
    polling"); `INTX_WAIT_SLICE_NS` is gone (the executor's own backstops bound staleness);
    the bound stays `INTX_WAIT_BOUND_NS` = 2 s with the same typed expiry text. Module docs
    rewritten (the "blocks host-side is deliberate" rationale was stale since D33/D35).

    The companion cancelcheck fix closes the same-wake-round flake the tail-2 review
    predicted: `(Some(Ok), Err(busy))` — A's completion delivered in the same round as
    B's busy — now classifies as an honest `Eager` instead of a loud typed failure.

    **Verified on QEMU aarch64** (`pci disk` boots): the probe now hits — `cancelcheck
    --attempts 25/100/50/3` → hits 1/1/1/1 with `eager` the rest, `data-miss=0`
    throughout, and **zero corruption across every verification sweep** — i.e. the D34
    drain-before-reuse invariant now holds under *real* mid-flight cancels (a hit is a
    genuine `subtask.cancel` landing while the driver is parked mid-INTx-wait), upgrading
    the invariant from analysis-pinned to executable-and-passing (master same probe:
    hits=0, structurally). A hit is also the direct scheduling proof of the liveness
    payoff: a peer task ran during an in-flight interrupt wait. Throughput control: a
    `restart.always` echo tick service across an identical disk-heavy foreground
    workload — 46,430 ticks (parked) vs 46,657 (master, host-blocking): QEMU's sub-ms
    virtio waits make the throughput delta noise on this device; the payoff here is
    latency/structure (and ms-scale real hardware). Ctrl-C mid-attempts → `abnormal:
    killed`, teardown quiesce fired, and a same-boot re-claim ran 3 more attempts over
    live INTx (hit 1) — no line-registration leak. Regression sweep all green: storage
    round-trip + admit-address-filtered chain + power-cycle persistence (INTx-served,
    zero polled fallbacks), net ARP + DNS + typed ConnectionRefused, check-gpu
    pixel-exact both frames, svcdemo battery (list/log/stop), storedisk two-boot
    (format → save → 664 KiB cached → power-cycle → 2.4 ms hit), three arch demos, full
    `cargo xtask ci`.

70. **Paste robustness: the console survives arbitrary-rate input on all three arches
    (2026-06-02, branch `area/12-paste-freeze`; entry 69 is taken by the concurrent
    `area/12-pciwait-park` branch).** Owner report: pasting a ~53-char line at the aarch64
    `make qemu` prompt froze the console mid-echo (~34 chars), permanently deaf to Enter and
    Ctrl-C — the bug every QEMU-driving harness in this project has been masking with the
    "pace input ~20 ms/char" convention. Reproduced with unpaced single-write bursts under
    host load (8 CPU hogs): master froze within 1–2 bursts; QEMU sat near-idle (~20% CPU,
    no IRQ storm, no executor wedge — instrumented), the FIFO and input ring both empty: the
    *host-side* character feed had stopped delivering. Mechanism (QEMU pl011.c, read
    directly): QEMU paces its feed by `pl011_can_receive = fifo_depth − read_count`, and the
    kernel never enabled the FIFOs (depth 1), so *every byte* paused the feed until the
    guest completed an interrupt + drain + DR-read round-trip; under load those per-byte
    pause/resume cycles wedge permanently. Two further latent deafness modes found while
    root-causing: (a) QEMU latches the receive interrupt only on the FIFO's
    empty→occupied transition and `UARTICR` clears the latch — the old drain-then-clear
    order could wipe the latch with a byte already in the FIFO (no receive-timeout timer in
    the model: dead forever); (b) the 256-byte input ring silently dropped long pastes
    (the project's own demo compositions exceed 300 chars). Fix, three layers plus the ring:
    **acknowledge-then-drain** in the PL011 handler (a byte landing after the final empty
    check re-latches and re-delivers); **FIFOs enabled** (PL011 `LCR_H.FEN` + TRM-correct
    `CR` enables + earliest `IFLS`; 16550 `FCR` on riscv64, x86_64 already had it) so the
    feed pauses at most once per 16 bytes; an **idle-path scavenger** (`scavenge_rx`, called
    from `idle_wait`'s existing wakes) that rescues any FIFO leftovers into the ring and,
    after one second of total input silence, issues one harmless data-register read — both
    QEMU UART models call `qemu_chr_fe_accept_input` unconditionally on every data-register
    read, even with an empty FIFO, so the read revives a wedged feed within ~1 s (documented
    trade: a byte landing in the few-instruction check-to-read window during an
    already-silent second is consumed and lost — one keystroke versus permanent deafness);
    and the **ring sized to `MAX_READ_LINE_BYTES`** (4096) so any line the shell accepts
    survives a paste. Drop policy: bytes beyond ring capacity are dropped, the console never
    goes deaf. Verified: A/B with a corrected harness (the original harness over-read past
    its needle and reported phantom "prompt lost" freezes — timestamped logs proved those
    rounds healthy; the harness now carries over surplus bytes) — master froze at cycles
    1–2 under load, the fixed kernel survived 600/600 + 200/200 loaded bursts; the owner's
    exact paste runs to `ok: greeted` loaded and quiet; burst matrix 16–512 chars lossless
    on all three arches; paste during an on-target compile is buffered and runs after;
    riscv64/x86_64 loaded hammers 100/100 each (riscv64 master baseline did not reproduce
    the wedge in 100 bursts — the PL011 path is where it bites; the 16550 changes are
    hardening of the same class); full battery (three arch demos, storage round-trip, net
    ARP, check-gpu pixel-exact, svcdemo) and `cargo xtask ci` green. The serial-driving
    convention "pace input ~20 ms/char" is now obsolete for correctness (harnesses may keep
    it for echo-interleaving readability only).

71. **Interrupt-edge audit + the PLIC claim becomes a linear token (2026-06-03, branch
    `spike/12-irq-ownership`).** Owner-commissioned after entry 70's sequencing bug: audit
    every interrupt-edge site for order-dependent code the compiler cannot see, and
    investigate whether ownership/typestate can enforce such sequencing structurally.
    **Audit** (`docs/spikes/irq-audit.md`, site-by-site with invariants, current order,
    rescue, verdict): 0 live bugs; 4 SOUND-BUT-FRAGILE clusters — PL011 ack-then-drain
    (severe history, scavenger-mitigated), GIC service-before-EOI, GIC IAR→EOIR pairing
    (no rescue), PLIC claim→complete pairing (no rescue); everything else SOUND via three
    named structural families (masked-halt brackets, sticky counters + guaranteed re-polls,
    bounded fallbacks) that make ordering non-load-bearing. **Prototype**
    (`src/arch/riscv64/plic.rs` + `traps.rs`): `claim()` returns a `#[must_use]`
    `Claim(NonZeroU32)` token consumed by value in `complete()` — complete-without-claim
    impossible, double-complete a compile error, an abandoned claim (the
    permanently-stranded-source bug) a lint plus a debug-build panic naming the source.
    Zero-cost verified: `Option<Claim>` const-asserted to the size of the raw register read
    (the `NonZeroU32` niche reuses the hardware's 0-sentinel), no `Drop` impl in release,
    clean A/B builds show `ktrap` byte-size identical (686 = 686; the +56-byte total `.text`
    delta is CGU repartitioning noise that also moves unrelated symbols). **Verdict**
    (`docs/spikes/irq-ownership.md`, honest): adopt the token shape for the two no-rescue
    must-consume protocols only — PLIC (done) and the GIC IAR/EOIR pair (same shape, queued
    follow-up); do NOT retrofit typestate generally — Rust's affine (not linear) types make
    the dangerous direction (forgetting to complete) a lint + debug panic rather than a
    compile error, the type system enforces the order you encode rather than discovering
    the correct order, and the audit's SOUND majority is sound for structural reasons
    tokens cannot express. The audit's invariant inventory is judged worth more than the
    types.

72. **GICv2 ack token, GICv3 support, and the Orange Pi 5 Plus prep pack (2026-06-03,
    branch `area/12-board-prep`).** The three preparable halves of real-board bring-up,
    landed before the board arrives:
    * **The IAR→EOIR linear token** (entry 71's one queued follow-up): `gic::acknowledge()`
      now returns `Option<Ack>` (`None` = spurious 1020–1023, no EOI obligation) and
      `end_of_interrupt(Ack)` consumes it by value — EOI-without-ack unconstructible,
      double-EOI a compile error, abandoned ack `#[must_use]`-linted + debug-drop panic.
      Representation is the raw IAR **plus one** in a `NonZeroU32` (IAR 0 is a legal INTID
      — SGI 0 — so the raw value has no spare zero; +1 cannot wrap at ≤ 24 significant
      bits). Zero-cost verified: `Option<Ack>` const-asserted at 4 bytes and the release
      `kirq` symbol byte-identical to master (0x118 = 0x118).
    * **GICv3** behind boot-time detection: `GICD_PIDR2.ArchRev` selects v2 (today's
      MMIO GICC path, byte-for-byte unchanged, no new boot output) or v3 (distributor
      ARE+group-1, blanket group-1 IGROUPR — group 0 would signal FIQ, which this kernel
      treats as fatal — redistributor wake + SGI-frame group/enable/priority for PPIs,
      `GICD_IROUTER<n> = 0` affinity routing for SPIs, ICC_* sysreg CPU interface with
      EOImode = 0 so the handler's ack→service→EOI flow is identical on both versions).
      Detection is PIDR2 rather than DTB because the kernel has no DTB parser yet
      (docs/board/gfx-simplefb.md scopes the first one); QEMU keeps GICD at the same base
      for both versions, and a real board moves the bases into a board profile
      (docs/board/orange-pi-5-plus.md item 5). The same `Ack` token spans both.
      `cargo xtask qemu aarch64 gicv3 …` boots `-M virt,gic-version=3`.
    * **The board docs**: docs/board/rk3588-pcie.md (the DesignWare config-access shim —
      `ConfigAccess` trait with ECAM + DW implementations, the iATU CFG window mechanics,
      the ECAM-refactor half that is implementable blind), docs/board/orange-pi-5-plus.md
      (U-Boot recipe, SD layout, the eight known kernel changes with sizes, the day-one
      smoke ladder), docs/board/gfx-simplefb.md (the dumb-framebuffer provider + the
      minimal FDT reader as its blind-implementable core).

## Entry 73 — the fusion-graph spawn cache, the compiler fingerprint, and the spawn-path numbers (2026-06-04)

**Decision (owner design, granularity correction):** spawn-path caches key on the
**fusion graph hash** — blake3 per node (leaf = component bytes; interior = op tag ‖
ordered child hashes ‖ canonicalized args), domain-separated — not on prompt spelling
and not on fused bytes. `KComponent` carries the hash; the session fused-bytes cache,
the session compiled-artifact cache (now also covering pristine entries'
deserialization), and svc's `compile_component` all key on it. Invalidation is
structural (a changed binding changes exactly the subtrees containing it). The
`graph-verify` feature re-encodes on every fusion hit and asserts byte equality (the
encoder-determinism check; verified live). Subtree-level partial-encode sharing:
recorded follow-up, not built.

**Decision (owner): the compiler fingerprint lives in the lookup key.** build.rs hashes
kernel/vendor/** + the engine-config sources into `EO9_COMPILER_FINGERPRINT`; the
storedisk compile-cache key is blake3(domain ‖ fingerprint ‖ executable-bytes). A
compiler change makes old entries clean misses — never a verification failure; the
keyed MAC stays reserved for integrity. Hidden-dep audit: codegen target features are
build-time fixed (explicit triple + explicit cranelift flags; x86's CPUID probe is
load-time verification only) — no runtime-detected set joins the key. Follow-up: fold
the same fingerprint into xtask's host-side precompile stamps (they still rely on the
manual PRECOMPILE_CONFIG_REV).

**Spawn linker:** built once per boot (host-fn set is boot-constant), grant-shape bit
stored and re-checked on every reuse (the security review's guard); per-spawn authority
lives in the per-spawn Store. InstancePre measured-and-skipped: instantiation is
0.9 ms / 0.4 ms — the composite-key machinery isn't paid for by a sub-ms saving.

**Numbers (TCG, medians):** warm `gpu.virtio $ draw` 377 ms → 121 ms external (kernel
195 ms → 2.1 ms); `time.frozen … $ hello` 151 ms → 28 ms; bare `hello` 2 ms external /
0.6 ms kernel. The remaining warm cost is guest-side eosh resolve/byte-passing
(~119 ms for the gpu line) — the eosh lane's problem, out of this one. Cold is
unchanged (codegen dominates). Full table: docs/spikes/spawn-latency.md.

**Eviction audit:** session caches LRU-8 (confirmed); usermode ~/.eo9 cache bounded by
`Store::gc` (LFU-then-LRU under a size budget — confirmed, and its `compiler_version`
key field is adequate for registry wasmtime but would not catch vendored content
changes); the kernel storedisk compile cache is **unbounded** — recommendation (not
implemented here): per-entry last-used + a ~32 MiB budget sweep at mount, which also
reclaims dead-fingerprint entries after kernel upgrades (artifacts 0.5–1.5 MiB;
~50–100 compiles fill the 64 MiB scratch disk today).

**HVF (xtask `hvf` flag, aarch64-only, opt-in):** boots through MMU/heap, then the
timer self-test faults (ESR EC=0x00 at `arch::aarch64::timer::self_test`) — Apple's
Hypervisor.framework exposes only the *virtual* generic timer to guests and the kernel
drives the EL1 *physical* timer (`cntp_*`). Finding recorded; the CNTV switch
(cntpct→cntvct, cntp_*→cntv_*, PPI 30→27) is the follow-up that would unlock
native-speed aarch64 sessions; the flag stays as the experiment vehicle. TCG remains
the default and the verified configuration.

74. **CNTV switch + syndrome-valid MMIO: the kernel runs under Apple HVF (2026-06-04,
    `area/12-cntv-hvf`).** Two changes unlock `cargo xtask qemu aarch64 hvf` end to end
    (docs/spikes/hvf-cntv.md has the full analysis and numbers):
    * **The EL1 virtual timer (CNTV)** replaces the physical timer everywhere in
      `arch/aarch64/timer.rs` (`cntpct→cntvct`, `cntp_*→cntv_*`): Apple HVF exposes only
      the virtual timer to guests. Safe on TCG (`CNTVOFF = 0` on QEMU `virt`, so
      virtual == physical there — canonical demo values byte-identical before/after);
      no interrupt-path change needed because the PPI family 26/27/29/30 was already
      enabled and `kirq` already serviced all four (the live PPI moves 30→27). The boot
      banner's "counter advancing" line now spins until it observes a real advance (at
      HVF's host-passthrough 24 MHz, back-to-back reads are routinely equal).
    * **`pci::mmio` syndrome-valid accessors**: LLVM compiled the ECAM volatile reads
      into SIMD/FP loads (`ldr s0`/`ldr b0` — found by disassembly), whose ISV=0
      data-abort syndromes hardware virtualizers cannot decode (QEMU hvf asserts; KVM
      shares the restriction). Inline-asm GPR forms (`ldrb/ldrh/ldr`, no writeback) on
      aarch64 behind the four PCI primitives — every wasm driver and the in-kernel
      virtio-blk routes through them. Plain volatile preserved on other arches.
      *Residual:* UART/GIC/RTC/INTx-mask volatiles currently compile to GPR forms
      (battery-proven) but only by compiler choice — sweep them through `mmio` if HVF
      becomes a daily driver or for any real-hypervisor deployment.
    * **Verified**: full HVF battery (lspci, disk INTx round-trip with zero polled
      fallbacks, net ARP + DNS, gpu draw cold/warm, switch vnicheck) with guest
      values byte-identical to TCG; TCG regression (demo, disk, check-gpu pixel-exact,
      svcdemo backoff pacing, 10/10 unpaced paste bursts, riscv64/x86_64 demos)
      unchanged. Speedups: ~10× on-target codegen, 9.4× cold draw, 14× demo session.
75. **The device-memory volatile sweep (`area/12-mmio-sweep`, 2026-06-04).** Closes
    entry 74's residual: the syndrome-valid accessors moved from `pci::mmio` to the
    crate-level `src/mmio.rs` (gated `any(aarch64, wasm-store)` — aarch64 drivers use
    it unconditionally, other arches only via the `wasm-store`-gated pci module, so
    feature-less builds stay lean), and every aarch64 device-memory volatile swept onto
    them: the PL011 UART read/write helpers (the paste-fix FIFO/scavenger paths ride
    them), all seven GIC MMIO helpers (GICC/GICD/GICR/SGI-frame) plus the byte-wide
    `IPRIORITYR` write in `configure_intid`, and the PL031 RTC data-register read.
    Inventory discipline: device memory = swept; RAM volatiles (virtio `DmaRegion`,
    the DTB scan in fdt.rs, the PVH `start_info` reads) stay plain volatile with the
    classification documented at each site; non-aarch64 device volatiles stay plain
    with a one-line why-aarch64-differs comment (riscv64 16550/PLIC/Goldfish-RTC/test
    device). Disassembly check: `kirq` and the UART write path carry zero SIMD/FP
    memory operations (before the sweep they were GPR **by luck**; now by construction
    — and the executable proof is the HVF battery, where any SIMD/FP device access
    aborts QEMU outright). Verified: three arch demos canonical, disk INTx round-trip,
    svcdemo, gicv3 demo + disk smoke, 10/10 unpaced paste bursts, check-gpu
    pixel-exact (TCG); boot + lspci + disk round-trip + l4check DNS + draw cold/warm
    (session-cache hit) + 3 paste bursts + the PL031 wall-clock read (HVF); full ci.

## Entry 76 — console read-line gains arrow-key recall (2026-06-04)

See plan/10 D24: the ReadLine future in `wasm/providers.rs` now parses ESC/CSI
sequences (consumed, never leaked into the line) and serves ↑/↓ recall from a
32-entry per-boot KLock ring. Shared across all three arches; paste-rate input and
Ctrl-C semantics unchanged (12/12 aarch64 battery + riscv64/x86_64 smokes).

## Entry 77 — the backstop accountability sweep: every progress backstop is an armed detector (2026-06-04)

The SPEC doctrine ("liveness is event-driven, never timer-driven; a progress backstop is
a confession; a backstop firing that discovers stranded work is a high-priority bug")
implemented as instrumentation. docs/spikes/backstop-audit.md carries the full inventory:
three progress-backstops kernel-side (the 10 ms child-running idle cap, the 1 s bare-prompt
cap, the UART scavenger's two rescues), one usermode (the 10 ms park cap), everything else
deadline-class or event-driven.

- `idle_wait` now returns a `WakeKind` (Event / Deadline / Backstop): cap-rated late wakes
  probe for stranded input (the scavenger's moved-byte count), stranded INTx deliveries
  (`pci::intx_pending_total`, a non-consuming peek), and — via the drive loops and
  `block_on` — stranded runnables (`any_runnable`/`Ready` on the pass right after a
  backstop wake). Findings print a rate-limited grep-stable `liveness:` line (first, then
  every 16th) and count in per-kind atomics. Deadline-rated wakes (sleep, IntxWait bound,
  svc restart) are never findings.
- Usermode `park_until_progress` applies the same rule: a full cap-rated `park_timeout`
  followed by foreground/service readiness is a find on stderr. The svc_shell suite's
  session helper asserts no `liveness:` line — the zero-findings gate, enforced in ci.
  The kernel-side gate lives in the battery convention (ci runs no QEMU).
- The battery (three arch demos canonical, storage/net/gpu incl. check-gpu pixel-exact,
  svcdemo + timer-paced backoff churn, cancelcheck hits=1/data-miss=0, 10/10 unpaced paste
  bursts, HVF disk round-trip, chaos 3x60 `-c` + 20 seeded `--svc` sessions, suites + full
  ci): **zero findings everywhere** — today's backstops rescue nothing we could observe.
  The backstops stay in place per the owner's ruling; removal is the end-state follow-up,
  and any future `liveness:` line is a bug to root-cause, never an assertion to relax.

## Entry 78 — the Orange Pi serial-loader stub (area/12-serial-loader)

`boards/opi5-serial-loader/` (own workspace, never in the root build): a 1,060-byte
flat stub typed into board RAM once via the vendor U-Boot's `mm.l` at 0x0400_0000
(265 words, prompt-paced — seconds), then launched with `go 0x04000000` ONLY (x0 via
the protocol header). Vendor `booti` is banned for the stub: it data-aborts inside
U-Boot itself on this minimal image and its own recovery reset then failed — the
2026-06-04 live incident (board wedged, physical power cycle). The stub keeps its
arm64 Image header for a future sane bootloader, but the dev loop never uses it. It then
loops forever: receive `"EO9L" + load/len/x0 + payload + crc32` on UART2 at 1.5 Mbaud
(~85 s for the 12.4 MiB minimal image), `k` per 64 KiB, verify, `K`, `dc civac` sweep
(to PoC since 2026-06-07 — the original `dc cvau` reached only PoU, leaving DRAM
stale for a next stage that runs with caches off; the first-light cache lesson) +
`ic iallu`, jump. CRC mismatch or a 3 s stall re-arms it — a failed transfer never
needs hands. The board needs no byte-level flow control (byte service is ~1 µs
against the line's 6.7 µs); the host sender uses the 64 KiB `k` acks as a windowed
flow control so the wire, not host polling, sets the transfer pace.
Mac side: `tools/make_mm_script.py` (bootstrap + U-Boot `crc32` verify) and
`tools/send_image.py` (transfer + console tail). Host-verified only (protocol tests,
header bytes, disassembly): the stub's first live run is on the board.

Dev-loop follow-up recorded: the board kernel should power off via PSCI SYSTEM_RESET
instead of SYSTEM_OFF so every run returns to the U-Boot prompt by itself (fold into
the area/12-opi5-profile review).
## Entry 79 — the Orange Pi 5 Plus board profile (2026-06-04)

The board arrived alive (vendor U-Boot in SPI NOR, GICv3 PIDR2=0x3b and UART2 base
0xfeb5_0000 verified live at the `opi#` prompt at 1.5 Mbaud). `board-opi5plus` is a
cargo feature off by default — every QEMU path keeps its own constants:

* **Entry**: the 64-byte arm64 `Image` header (`.text.header`, text_offset 0,
  `__image_size` covers .bss + boot stack) + an EL2→EL1 drop (TF-A hands U-Boot EL2h —
  boot log `SPSR = 0x3c9`; the trampoline zeroes CNTVOFF_EL2 for the CNTV timer, sets
  HCR_EL2.RW, parks the FP/CP15 traps, seeds SCTLR_EL1 = 0x30D00800, `eret`s to EL1h).
  QEMU's ELF loader enters `_start` at EL1 and never sees either.
* **Layout**: linked at 0x0020_0000 (the DRAM bank base; booti dst = bank+text_offset
  lands there under both the 2017.09 and the round-down relocation rules, and a direct
  `fatload` to 0x0020_0000 runs in place). MMU window: DRAM 0x0..0x2100_0000 (528 MiB,
  RAM at level-1 entry 0) + a Device gigabyte at entry 3 (0xC000_0000.. — UART2, GIC,
  U-Boot's relocated remains and control FDT, all readable). TF-A's low regions are
  mapped RW-NX and never touched.
* **Console**: DW-APB UART2 register layer (stride 4 via mmio.rs; LSR/THR/RBR only) —
  the line stays exactly as U-Boot programmed it (24 MHz / 1.5 M = divisor 1; nothing
  writes LCR/DLL/FCR). uart.rs is now a shared core (ring/Ctrl-C/console) over a
  cfg-selected `hw` layer; the PL011 layer is byte-for-behavior the old code. Day-one
  input flows through the idle scavenger's poll (≤ ~10 ms latency) because UART2's GIC
  SPI is deliberately unwired; wiring it is the recorded follow-up.
* **GIC**: GICD 0xfe60_0000 / GICR 0xfe68_0000 (frame 0 = the boot core, cpu-hwid-0);
  the existing v2-in-frame-first version probe lands RAZ→0xFFE8→0x3b→v3 exactly as on
  QEMU gicv3. PSCI via SMC (TF-A); PL031 absent → wall clock reads 0 (PMIC RTC later);
  ECAM_BUSES = 0 (lspci finds nothing instead of dereferencing DRAM — the DW-PCIe shim
  is its own session, docs/board/rk3588-pcie.md).
* **Build**: `cargo xtask build-kernel aarch64 opi5plus [minimal]` → flattens PT_LOADs
  into `kernel/target/eo9-opi5plus[-min].img` (header magic asserted). `minimal` bakes a
  hello-only store (12.4 MiB image) for fatload-speed iteration; full bakes the standard
  52. Boot recipe + expected output + fallbacks: .claude/board-bringup/BOOT.md.
* Pre-existing (also on the standard QEMU build, untouched): the three
  `eo9-kernel` warnings (unused `Val`/`NAME`/`COMPILER_FINGERPRINT` under
  wasm-store-without-storedisk feature sets).

Board-only residue for bring-up day: first serial output through the DW layer, the GIC
on real silicon, SMC SYSTEM_OFF, booti's relocation behavior on the vendor 2017.09
U-Boot (fallback `go 0x00200000` documented). QEMU regression: demo canonical + gicv3
demo + paste burst re-run green on this branch (the board feature off).

## Entry 80 — board loop-safety: exit-is-reset, panic marker, the hardware watchdog (2026-06-04)

The autonomous UART dev loop runs unattended — every kernel outcome must land back at the
U-Boot prompt. Three `board-opi5plus`-gated changes (QEMU byte-for-behavior unchanged,
canonical v2 + gicv3 demos re-verified; the standard kernel binary carries none of the new
strings):

* **Exit = reset** (src/arch/aarch64/power.rs): the end-of-run PSCI call is
  `SYSTEM_RESET` (0x8400_0009) on the board, `SYSTEM_OFF` on QEMU — one
  `PSCI_END_OF_RUN` id, cfg-selected; `OFF_REQUEST` names the truth per profile. The
  board arm drains the UART transmit path (new `uart::tx_drain`, bounded spin on DW LSR
  TEMT) before the SMC so the outcome line survives the reset.
* **Panic = print + reset** (src/panic.rs): the report gains a grep-stable `EO9-PANIC`
  marker line on the board profile (loop drivers classify the boot), then flows into the
  same drained SYSTEM_RESET via `system_off()`. QEMU panic text is untouched.
* **The watchdog** (src/wdt.rs, new): RK3588 DW-WDT (`wdt@feaf0000`, rk3588s.dtsi;
  driven per Linux dw_wdt.c) armed at boot — TORR TOP=13 (≈22.4 s at the 24 MHz
  `tclk_wdt0`, gated from xin24m per clk-rk3588.c), response mode 0 (direct SoC reset),
  CRR `0x76` pats. Patted from `idle_wait` (every idle wake) and both busy-pass branches
  of the shell/init drive loops, so neither a hot nor a parked kernel starves it; a hang
  → reset → U-Boot in ≤ ~22 s. Doctrine note: a dead-man's switch, not a progress
  backstop — it never makes anything advance (SPEC "liveness is event-driven").
  Arming is best-effort with loud verification (enable readback + live counter): if
  firmware left the WDT clock gated we print `wdt: arm FAILED` and boot on rather than
  blind-poking CRU gates (write-mask-high; the ungate is a bench follow-up if the live
  board needs it). Mainline evidence that no CRU glue is required: rk3588 dts carries no
  reset-mode property and dw_wdt.c does nothing CRU-side — the WDT→global-reset routing
  is SoC/firmware default. Live validation (deliberate starve → observe reboot) is the
  planner's first on-board test.

Both images rebuilt: `eo9-opi5plus-min.img` 12,976,896 B / `eo9-opi5plus.img`
50,135,120 B, each carrying exactly one `EO9-PANIC` / `wdt: armed` /
`SYSTEM_RESET (back to U-Boot)` string; the QEMU kernel carries zero. Known flake while
gating: `svc_shell::restart_cycles_complete_while_the_foreground_is_quietly_blocked`
fails under heavy parallel host load on the UNMODIFIED base tree too (2/3 base-tree
failures at load ≈ 17 with a concurrent agent's QEMU battery; 8/8 green in a quiet
window) — the wall-time pacing class, not this branch.
