# User study 03 — embedded/OS systems engineer

## Metadata

- **Date:** 2026-05-27
- **Participant:** an embedded/OS systems engineer persona (12+ years: RTOSes, Linux bring-up,
  bootloaders, device drivers, memory-constrained targets; pragmatic about toolchains and binary
  sizes; allergic to vague performance claims). The participant had no prior knowledge of Eo9 and
  saw only what the facilitator showed; they did not read the repository.
- **Facilitator:** ran every command the participant asked for live and relayed real, trimmed
  output; nothing was fabricated or beautified. Where something does not exist or was not
  demonstrated, that was said outright and recorded here.
- **Code under test:** branch `study/session-embedded` at de7baef (clean), built with
  `cargo xtask build-guest`, `cargo build -p eo9`, `cargo xtask build-kernel aarch64`.
- **Environment:** QEMU `virt` (`qemu-system-aarch64 -M virt -cpu max -smp 1 -m 512M -nographic`)
  under TCG on an Apple-silicon macOS host; userspace runs used a debug-build `eo9` binary against
  a throwaway store (`EO9_STORE` pointed at a temp dir). All timings below are therefore emulated
  (or debug-build) numbers and were presented to the participant as such.
- **Session shape:** ~6 rounds: pitch + bare-metal boot demo → footprint/timing/scheduling/JIT
  hygiene → codegen-quality ratio test, boot flow, deploy, program/build/bake story → capability
  model + fault isolation on metal → debugging story → verdict and recommendations.

## Condensed transcript (the interesting exchanges)

### Round 1 — pitch and boot

Facilitator gave the two-paragraph pitch (imports are permissions; same components in userspace
and on bare metal; the kernel carries its own Cranelift) and showed a real scripted serial session:
boot to `eosh>`, `hello --name metal --excited true` → `ok: greeted`,
`entropy.seeded $ cruncher --seed 9 --rounds 200000` → `ok: digest(14341732361190694547)`
(fused and compiled on-target), `exit` → PSCI power-off.

Participant's immediate asks: kernel ELF size with a section breakdown ("how much is compiler vs
baked store vs actual kernel"), what the kernel actually needs resident ("could this fit in 64M?
16M? That answer decides whether 'bare metal' means a Cortex-A SBC or something genuinely small"),
a timed re-run of the composition with compose/compile/run split, what cruncher does per round,
whether the GIC is up at all ("polled ISTATUS is doing a lot of quiet work"), the scheduling model,
whether `-smp 1` is a limitation or a decision, and whether JIT pages are W^X with proper
DC CVAU / IC IVAU maintenance "or everything RWX in the identity map right now. Be honest, I won't
faint."

### Round 2 — footprint, timings, scheduling, JIT hygiene

Facilitator showed measured numbers:

- Kernel ELF with the on-target compiler: file 23,641,592 B; `--strip-all` 16,258,064 B;
  `.text` 10.25 MB, `.rodata` 5.94 MB (≈4.9 MB of that is the baked store + AOT artifacts).
- Same kernel rebuilt without `wasm-codegen`: 8,201,784 B file; `.text` 1.55 MB, `.rodata` 5.14 MB.
  So Cranelift + compile layers + the algebra cost ≈ +8.7 MB of `.text`.
- Resident need: honestly uncharacterized. Heap line is "all RAM minus image"; 131 KB in use after
  the boot self-test; peak heap during an on-target compile never measured.
- Timed interactive run (driver script timestamps every serial line, measures send→next-prompt):
  boot banner→prompt 0.065–0.069 s; `cruncher` from the baked AOT artifact 10–32 ms
  prompt-to-prompt; `entropy.seeded $ cruncher` 1.07–1.17 s, recompiled every time (no cache on
  metal); the boot demo's compile of a 298-byte toy component printed `compiled on-target in
  138511 us` that day.
- The kernel exposes no per-phase (compose vs compile vs run) timing — could not provide the split
  the participant asked for.
- Scheduling/interrupts: no GIC; timer and UART RX polled; busy-poll executor; cooperative only;
  fuel not enabled on metal; the no_std scheduler crate exists but is not adopted; single-core.
- JIT hygiene: cache maintenance (DC CVAU / DSB / IC IVAU / DSB / ISB by CTR_EL0 line size) is
  implemented in the publisher; W^X is not — flat identity map, RAM effectively RWX; exceptions
  fatal-and-park.

Participant's reaction: "'nobody has characterized it' and 'polled, no GIC, RWX' are answers I can
work with." Pushed back that TCG cruncher timings "tell me almost nothing" and asked for two
better measurements: (a) crank rounds until the run takes seconds and compare baked-AOT vs
on-target-compiled code (the ratio measures code quality), (b) run the same composition natively
in userspace to get the realistic floor for the 1.1 s. Flagged that recompiling every composition
"is a real problem the moment anyone scripts anything" and asked whether a fused-artifact cache is
designed or not. Asked for the boot flow / real-board story ("which board is the intended first
target… and be straight about the class of machine"), the deploy story, the program source and
toolchain, and why AOT artifacts are 5–20x the wasm.

### Round 3 — ratio test, native floor, boot flow, deploy, build/bake

- Ratio test on metal, 200M rounds: baked artifact 0.780 s prompt-to-prompt; composed +
  on-target-compiled 2.135 s. Subtracting the separately measured ~1.05–1.15 s compose+compile
  overhead, the on-target-compiled run looked ~25–35% slower than the host-AOT artifact on this
  one microbenchmark — presented as "same league, not yet shown equal," with the caveat that
  nobody has verified the two engines use the same opt level. Same digest from both, matching the
  host run.
- Native floor: in userspace, `entropy.seeded $ cruncher` is ~0.51 s total vs 0.14 s for plain
  `cruncher`, i.e. ~0.35–0.4 s for compose+compile+link natively (debug-build CLI). Running the
  identical composition a second time was not faster — the compile cache visibly did not help this
  path (one known reason: cache keys are conservatively marked compiler-not-certified-deterministic).
- Boot flow: UART/RTC/RAM addresses are hard-coded for `virt`; the DTB is parsed only for
  `/chosen/bootargs`; ELF loaded by QEMU `-kernel`; PSCI assumed; no U-Boot/EDK2 path; no GIC; no
  real board named anywhere in the plan; riscv64/x86_64 QEMU ports queued before any silicon.
  Facilitator agreed on the record this is a Cortex-A-class proposition.
- Deploy: new program onto the metal machine = rebuild image + reboot. No virtio, no block device,
  no network on metal. The CoW/Merkle filesystem engine, the PCI WIT API, and `eo9 store add`
  exist as pieces, "not a deploy mechanism you can use."
- Build/bake story: full `hello` source (39 lines of no_std Rust) and its WIT world shown; the
  three toolchain commands (`cargo build --target wasm32-unknown-unknown`, `wasm-tools component
  new`, `wasm-tools validate`); xtask precompiles each store component for the kernel target and
  assembles a ~4 MB store image embedded via `include_bytes!` and served read-only as `/bin`.
- AOT artifact sizes: hello 35 KB → 302 KB, cruncher 16 KB → 182 KB, eosh 229 KB → 1.58 MB; nobody
  has tried to shrink them.

Participant's reaction: liked the honesty of the ratio claim; noted "the irony: determinism is the
pitch, and it's the compiler you can't yet certify" (re the cache). Gave an unprompted opinion:
**don't port to two more emulators before touching a real board** — "QEMU virt is the friendliest
fake hardware in existence… a real board teaches you whether the design survives." Then drilled
into the capability story ("`hello` ran with no composition typed, yet it got a clock — so eosh is
granting some interfaces ambiently by default. Which ones, decided where, and how do I see and
override it?") and asked for fault-isolation demos on metal: a trapping guest, stack exhaustion,
and an infinite loop ("if any of them takes the whole machine down, that's the next hardening item
ahead of W^X in my book"). Also asked what `time::now()` is backed by on metal vs `time.frozen`.

### Round 4 — capability model and fault isolation

- Defaults explained and shown: the session grants text/time/entropy to children by default, never
  fs or exec; `describe`, `env <program>` (including the "would be refused at spawn" marking for
  fs), the `only` refusal at compose time, the missing-fs refusal before spawn, and the `--fs-root`
  grant round-tripping a file inside the sandbox were all shown with real output.
- Fault isolation on metal, demoed live: `outcomes --mode fail` → typed failure, shell fine;
  `outcomes --mode trap` → `abnormal: trapped` with a symbolized wasm backtrace, and the very next
  `hello` ran normally (shell survives). Stack exhaustion: no fixture exists; the facilitator
  declined to claim a result and explained the intended mechanism (explicit stack-limit checks
  because signals are off; heap-allocated guest stacks with no guard pages behind them). Infinite
  loop: stated on the record — no fuel, no preemption on metal, so a spinning child takes the
  console and the machine needs a reset.
- A program needing the filesystem, run on metal, fails with the raw linker message
  (`SpawnError::Internal("component imports instance eo9:io/buffers@0.1.0 …")`) instead of the
  friendly userspace message.
- `time.frozen --now-seconds 42 --monotonic-ns 0 $ hello` was composed, configured, compiled
  on-target, and printed `[42.000000000] Hello, frozen!` (2.4 s).
- Live bug found by the participant's question: composing `time.frozen $ hello` *without*
  configuring it does not produce a "provider not configured" error — the provider's `now()`
  panics and the user gets a raw `abnormal: trapped` backtrace. Same behavior in userspace, so it
  is a stub/validation sharp edge, not a metal regression.

Participant's reaction: "the capability demo lands… refusal at compose/spawn time rather than a
runtime EPERM ten minutes in is the right call." Two notes: (1) **the silent default grant of
entropy bothered them more than text/time** — "in a system whose whole pitch is determinism and
explicit authority, that's the one I'd make opt-in or at least make eosh print a one-liner at
spawn ('granting: text, time, entropy')"; (2) the unconfigured-provider panic and the raw linker
error are "exactly the kind of thing a session like this should flush out." Restated that
one-spinning-guest-takes-the-machine plus no GIC means "the scheduling/interrupt work is the real
next milestone, not a hardening footnote." Then asked for the debugging story: can they do better
than mangled backtraces; does PC→wasm mapping work for on-target-JIT'd code; is the kernel
exception dump symbolized; does `qemu -s -S` + gdb actually work today ("have you done it"); any
structured logging?

### Round 5 — debugging story

- Userspace guest: ceiling today is the typed outcome plus the wasm trap backtrace (mangled names,
  hex offsets). A `--debug-info` compile flag exists but there is no documented attach-and-step
  workflow behind it; the "interpreting executor" (debugger-as-a-component) is design intent only.
- Metal guest: trap symbolization works, including for on-target-compiled fused components (frames
  attributed by name; composition glue shows as `<unknown>!<wasm function N>`); beyond traps,
  nothing — no attach, no breakpoints, no watchpoints.
- Metal kernel: exception dump is raw hex (vector, ESR/EC, ELR, FAR), then park; not symbolized.
  The facilitator tried `qemu -s -S` + lldb live during the session rather than guessing: lldb
  attaches to the gdbstub and kernel symbols resolve (a breakpoint on `kmain` lands at the right
  address), but in two quick scripted attempts the breakpoint never actually fired — the kernel
  booted straight past it. Honest summary given: stub and symbols are there; no proven or
  documented debugger workflow exists; Cranelift-emitted code would have no symbols at all.
- Logging: printf over PL011 all the way down; no structured logging or trace facility; the typed
  outcome vocabulary is the one structured signal.

### Round 6 — the participant's verdict (their words, condensed)

- **Real:** "A no_std kernel that boots to an interactive shell on virt, carries its own Cranelift,
  and will take a composition I typed at a prompt, fuse it, compile it to aarch64 on the machine,
  run it, and give me the bit-identical digest the host gives — that's a real, integrated thing,
  not a slideware loop." Also called out: capability enforcement at compose/spawn time, typed
  outcomes, trap backtraces that survive the JIT path, the cache-maintenance sequence being in
  before W^X, the clean guest authoring story, and "the typed outcome vocabulary is quietly one of
  the better ideas here."
- **Vapor-adjacent:** "'Runs on bare metal' currently means 'runs on the friendliest fake board
  ever made, with hard-coded addresses, no interrupt controller, no storage, no W^X, and no board
  on the roadmap.' The deploy story, the compose cache, the scheduler, the debugger — all 'pieces
  on the shelf' or 'designed in spirit.' I don't mind shelf pieces; I mind them being implied as
  features. You mostly didn't, to your credit."
- **Top blockers (their order):** (1) one spinning guest takes the machine — "until that's fixed
  this is a sandboxed launcher with great isolation properties, not an operating system";
  (2) no writable storage on metal (blocks deploy, blocks the compose cache, forces
  recompile-every-boot); (3) footprint uncharacterized ("you can't claim a hardware class until
  you can answer that"); (4) no real board, with two more emulators queued ahead of one;
  (5) debugging is trap-backtraces-or-nothing and the kernel-side debugger path didn't survive a
  ten-minute test.
- **Over-engineered:** "a full in-kernel compiler and a composition algebra before there's an
  interrupt controller — the fancy layer is two milestones ahead of the OS fundamentals."
  **Under-engineered:** instrumentation ("you couldn't tell me compose-vs-compile split, peak
  heap, or why the cache didn't hit, and all three answers were 'no instrumentation'").
- **Model confusion worth fixing:** "there are two authority mechanisms — session default grants
  and explicit composition — and the silent text/time/entropy grant fell exactly into the seam
  between them. Either make grants printed/explicit or fold them into the same algebra."
- **Impressed by:** the on-target compile path end to end; determinism demonstrated rather than
  asserted; refusal-before-run; the measurement honesty during the session.
- **First three builds they'd ask for:** (1) GIC + timer interrupt + fuel/preemption on metal
  ("that's the line between launcher and OS"); (2) virtio-blk on virt + a writable
  content-addressed store + certify compiler determinism so the compose cache actually hits;
  (3) name a real board and do bring-up before riscv64/x86_64 (DTB-driven discovery, U-Boot
  handoff, W^X, and a measured footprint "come along for the ride").
- **Cheap wins they listed:** print the default grant at spawn; fail unconfigured providers at
  compose time; add a stack-exhaustion fixture; per-phase timing on the compose path.
- **Net:** "I came in expecting a toy and I'm leaving thinking it's a promising research kernel
  about eighteen months of disciplined, boring work away from being something I'd flash onto a
  board. The interesting ideas are real; the OS underneath them isn't yet."

## Findings

### Confusions

1. **Two authority mechanisms, one of them invisible.** The participant initially read the default
   text/time/entropy grant as "ambient authority" — the exact thing the pitch says doesn't exist —
   because nothing at the prompt shows that a default grant is happening. The distinction
   (parent-chosen default grant vs ambient) had to be explained; their conclusion was that the
   seam between session grants and the composition algebra is a model-level confusion, not just a
   UX one.
2. **Entropy in the default grant** reads as contradictory with the determinism pitch ("every
   child gets a real RNG without anyone saying so").
3. **"Bare metal" vs "QEMU virt."** The participant repeatedly had to pin down what "runs on bare
   metal" means today (hard-coded virt addresses, QEMU's ELF loader, PSCI assumed, no board).
   The phrase over-promises relative to the current state.
4. **The composed-but-unconfigured provider.** `time.frozen $ hello` looks like a legitimate
   expression but produces a guest panic with a backtrace rather than an error explaining that the
   provider needs configuration. The participant read this as the system failing to enforce its
   own model edge.

### Pain points

1. **Every composition recompiles, every boot starts cold** on metal (no writable storage, no
   fused-artifact cache); even in userspace, repeating an identical composition was not observably
   faster (cache keyed conservatively because compiler determinism is not certified).
2. **No per-phase instrumentation.** Compose vs compile vs run time, peak heap during a compile,
   cache hit/miss reasons — none of it is observable; the facilitator had to derive splits by
   subtraction from external measurements.
3. **Fs-needing programs on metal die with a raw linker error** (`eo9:io/buffers … not found in
   the linker`) instead of the friendly userspace "needs fs, pass --fs-root" message.
4. **Debugging is thin everywhere:** mangled-symbol trap backtraces are the ceiling for guests;
   the kernel exception dump is unsymbolized hex; there is no documented (or session-verifiable)
   debugger workflow against the kernel; no structured logging or trace facility.
5. **Getting a new program onto the metal machine requires rebuilding and rebooting the kernel
   image** (read-only baked store).

### Missing capabilities (in the participant's priority order)

1. GIC / interrupt-driven execution, preemption or fuel on metal — an infinite-looping guest
   currently takes the machine (stated on the record during the session).
2. Writable storage on metal (virtio-blk + the content-addressed store) and a fused-artifact
   compile cache backed by certified compiler determinism.
3. Footprint characterization (peak heap during on-target compile; minimum viable RAM; what the
   AOT-only variant needs) and artifact-size reduction (AOT artifacts are 5–20x the wasm).
4. Real-board bring-up: DTB-driven device/memory discovery (addresses are compile-time constants
   today), U-Boot/EDK2 handoff, W^X for JIT pages, real entropy, exception recovery.
5. Debugger story (kernel-side and guest-side), stack-exhaustion fixture, multi-core (not started).

### Performance / footprint reactions

- Boot-to-prompt (~65 ms banner→prompt under TCG) and 10–30 ms prompt-to-prompt for baked programs
  drew no complaints.
- The ~1.1 s on-metal compose+compile (TCG) and the ~0.35–0.4 s native equivalent were treated as
  workable *if* caching worked; the lack of caching was treated as the real problem.
- The on-target vs host-AOT code-quality ratio (~25–35% slower on one microbenchmark, with
  unverified opt-level parity) was accepted as "same league, not yet shown equal" — the
  participant explicitly endorsed that level of claim and no stronger.
- 16.3 MB stripped image with the compiler (8.2 MB without), ~+8.7 MB of `.text` for
  Cranelift+algebra, and 5–20x AOT artifact expansion all registered as "Cortex-A class, and say
  so"; the unanswered "could it fit in 64M/16M" question was turned into blocker #3.
- TCG-vs-native distortion was called out by the participant up front; the facilitator's framing
  of all numbers as emulated shape rather than silicon truth was necessary to keep credibility.

### Criticisms

1. "A sandboxed launcher with great isolation properties, not an operating system" until a
   spinning guest can't take the machine.
2. The fancy layer (in-kernel compiler, composition algebra) landed two milestones ahead of OS
   fundamentals (interrupts, storage, scheduling) — a sequencing criticism, not a value one.
3. Two more emulator ports queued ahead of any real board is the wrong order; QEMU virt hides
   exactly the problems a real board would surface (loader handoff, cache/coherency reality for
   the JIT publish path, interrupt-controller quirks, entropy, storage, UART location).
4. Determinism is the pitch, but compiler determinism is not certified — which is also what keeps
   the compile cache conservative.
5. Footprint and instrumentation gaps make the hardware-class claim unsupportable today.
6. The README's capability examples drifted from the implementation (see rough edges) — caught by
   the facilitator, not shown to the participant, but the same class of "implied feature" the
   participant warned about.

### What landed well

- The end-to-end on-target story: type a composition at the bare-metal prompt, have it fused by
  the real algebra, compiled by the kernel's own Cranelift, and run — with the digest matching the
  host bit-for-bit. The participant called this "a real, integrated thing, not a slideware loop."
- Refusal before run (`only` at compose time, missing grants at spawn time) rather than a runtime
  permission error.
- Typed outcomes (success/failure/abnormal in the program's own vocabulary) — "quietly one of the
  better ideas here."
- Trap containment on metal: a panicking guest comes back as a typed `abnormal` with a symbolized
  backtrace and the shell keeps running.
- Cache maintenance on JIT publish being implemented before W^X ("tells me someone is thinking
  about real silicon").
- The guest authoring story: a 39-line no_std Rust file plus a small WIT world and three toolchain
  commands.
- Honest answers: "nobody has characterized it," "should trap, not demonstrated," and trying the
  debugger live instead of asserting it works all visibly increased the participant's trust.

### Rough edges the facilitator hit (independent of the participant)

1. **README drift (userspace):** on a truly fresh store, `eo9 hello --name world --excited true`
   (the README's first example) fails with `name hello does not resolve in profile "default"` —
   the store is only seeded by the shell/`-c` paths, not by a direct `eo9 <name>` run. After any
   `-c` invocation it works.
2. **README drift (capability algebra):** the README's `only eo9:text,eo9:time $ hello` form is
   rejected (`InvalidAllowList: 'eo9:text' is not an interface name`); only full interface refs
   (`eo9:text/text,eo9:time/time`) work. The README's prettier refusal text ("required imports
   outside the allow-list: …") also doesn't match the actual Debug-formatted
   `RestrictError::RequiredOutsideAllowList([…])` output.
3. **Stale `env` text on metal:** the bare-metal `env` output still says "composition and
   on-target codegen are not available yet" — in the same session where a composition had just
   been compiled on-target and run successfully two commands earlier.
4. **Unconfigured-provider panic** (found via the participant's question, reproduced in both
   modes): `time.frozen $ hello` traps with a raw backtrace instead of a "provider requires
   configuration" error.
5. **Configure discoverability:** the facilitator's first two guesses at the configure syntax
   failed (`configure(time.frozen, …)` is a parse error; the flag name had to be found via
   `describe time.frozen`); the error messages were clear enough to recover, but there is no
   worked example of configuring a provider from the shell in the README.
6. **`only` error quality differs by mode:** metal wraps the same refusal in
   `RestrictError::Internal("…")` rather than the specific variant.
7. **First-command latency:** the first program run after boot on metal is consistently ~20 ms
   slower than subsequent ones (lazy init somewhere); harmless but visible in measurements.

## Facilitator observations (where the facilitator had to apologize for or work around the system)

- Had to apologize for the absence of per-phase timing (compose/compile/run) and derive splits by
  subtraction across separate runs; likewise for "peak heap during a compile" and "why didn't the
  cache hit" — all answered with "no instrumentation exists."
- Had to script the serial console externally (a timestamping driver feeding stdin) to produce any
  timing data at all; the system itself offers nothing for this.
- Had to rebuild the kernel with a different feature set to answer the "image size without the
  compiler" question — there is no size report or build matrix that answers it directly.
- Had to work around the userspace store seeding quirk (first README command fails on a fresh
  store) and the README/implementation drift in the `only` examples while preparing demos.
- Could not demonstrate stack exhaustion (no fixture) and declined to claim the behavior; could
  not produce a working kernel breakpoint via the QEMU gdbstub in two quick attempts and reported
  that as the honest state.
- Used a separate `EO9_STORE` to avoid touching the host user's real store; this worked, but the
  default of silently writing to `~/.eo9` is worth remembering when demoing on someone else's
  machine.
- The unconfigured `time.frozen` trap surfaced live mid-demo in front of the participant; the
  recovery (showing the configured form working on both modes) was possible only because
  `describe` exposes the provider's config arguments.
