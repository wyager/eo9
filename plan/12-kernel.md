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

