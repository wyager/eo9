# User study 01 — pragmatic CLI developer

## Session metadata

- **Date:** 2026-05-27
- **Branch / worktree:** `study/session-cli-dev` (worktree of master at `de7baef`)
- **Participant persona:** a pragmatic backend/CLI developer, ~8 years of experience
  (Python/Go/Rust, terminal-native, no WebAssembly background, "no patience for ceremony").
- **Methodology:** the participant was a role-played persona run as a separate session with
  no access to the repository, its documentation, or any tools — it saw only what the
  facilitator pasted into the conversation and replied conversationally. Every command shown
  to the participant was actually executed by the facilitator in the study environment;
  outputs are verbatim, trimmed only for length. Failures and breakage were shown as they
  happened, not cleaned up.
- **Environment:** release build of the `eo9` CLI from this checkout on an Apple Silicon
  macOS host; a throwaway store (seeded with the bundled programs and coreutils) and a
  throwaway `--fs-root` sandbox directory; for the bare-metal segment, the aarch64 kernel
  image built from the same checkout (`cargo xtask build-kernel aarch64`, on-target-codegen
  feature enabled by xtask) run under `qemu-system-aarch64 -M virt`.
- **Shape:** an opening pitch + first demos and the participant's first reactions happened in
  an earlier sitting (summarized below); this report covers the continuation — five
  facilitator rounds of demos/answers, the participant's verdict, and a structured wrap-up.

## Round 1 (prior sitting, summarized)

The participant was given the pitch — capability-secure OS on the WebAssembly component
model; a program's imports are its permissions; granting authority is a composition algebra;
deny-by-default — and shown: `eo9 hello --name you --excited true` (greeting plus
`success(greeted)`), the eosh shell, a couple of coreutils (ls/cat/echo) running under
`--fs-root`, and a composition example. Their four opening reactions, in their words:

1. "The clock thing bugs me" — hello had to be *granted* a clock but no grant appeared on the
   command line, and `--fs-root` looks like a flag, not the algebra that was pitched. Is
   there a default grant set when running things directly, and isn't that exactly the
   ambient authority the pitch says doesn't exist? Show what `eo9 hello` actually got granted
   and where that decision was made.
2. Show a denial — run something that tries to do something it wasn't granted — because
   "refused before it starts" vs "traps at runtime" are very different developer
   experiences; "the denial is the whole product."
3. What is `success(greeted)`, who consumes it, and does it wreck normal shell pipes
   (stdout vs the typed-result channel)?
4. How slow is it — wall-clock for `eo9 echo` vs `/bin/echo`, and is compile time paid on
   every run or is there caching?

## Round 2 — answers to the four opening questions

**Default grants.** Facilitator answered candidly that direct `eo9 <name>` does wire a
default provider set (terminal text, host clock, host RNG; fs only with `--fs-root`; network
never — none exists; exec never for children), and showed `eo9 -c "env"`, `eo9 -c "env
hello"`, `eo9 -c "describe hello"`, hello's timestamped output (why it wants a clock), and
the place the decision is made (`crates/eo9/src/providers.rs`, `root_providers()`), plus the
strict path:

```
$ eo9 -c "only eo9:text/text,eo9:text/types,eo9:time/time,eo9:time/types $ hello --name boxed --excited true"
[1779919943.171208000] Hello, boxed!
ok: greeted
```

with the honesty footnote that the README's shorter `only eo9:text,eo9:time` form does **not**
parse today (`RestrictError::InvalidAllowList("eo9:text is not an interface name …")`).

**Denial.** Three shapes shown: `eo9 cat --path notes.txt` with no `--fs-root` → refused
before instantiation with the friendly message and exit 3 (and exit 0 with the grant);
`only eo9:text/text,eo9:text/types $ hello …` → compose-time refusal naming
`eo9:time/time@0.1.0`, but rendered as a raw `RestrictError::RequiredOutsideAllowList(…)`
debug print and surfacing as exit 1 because it went through the shell; and a real runtime
trap for contrast (`outcomes --mode trap --detail boom` → `abnormal(trapped(…))`, exit 2).
Exit-code contract stated: 0 success, 1 typed failure, 2 trap/kill, 3 refused before start.

**The outcome line.** Tested rather than asserted: both program stdout and the outcome line
go to stdout (stderr empty); `eo9 echo --text "hello pipe" | wc -c` counts the outcome line;
no `--quiet`/porcelain flag exists.

**Speed.** `/bin/echo` ≈ 7 ms; `eo9 echo` warm ≈ 14–17 ms; cold compile of a small program
≈ +10–50 ms (shown with `-v`: cache miss → compile → cached; cache hit → "launched from
cached image"); the eosh component itself ≈ 0.21 s to compile cold; the cache is
content-addressed and persists in the store.

**Participant reaction (round 2), condensed, their words where quoted:**

- Credit for answering the ambient-authority question with "yes, there's a default set."
  But: "that's a different pitch than 'your imports are your permissions.' It's
  'deny-by-default for the scary stuff, ambient for the boring stuff,' which is roughly
  where pledge/unveil and plain WASI already live."
- The default living in `root_providers()` means changing it is "recompile your launcher, or
  type interface-level allow-lists with versions… Nobody is typing that. The package-level
  shorthand not working isn't a footnote, it's the difference between 'I'd use the strict
  path' and 'I'd never use the strict path.'"
- `success(greeted)` vs `ok: greeted`: "the kind of inconsistency that tells me the surfaces
  grew independently."
- "Everything you've shown me is *subtraction*… Show me *substitution*. Compose hello with a
  fake clock provider and get a deterministic timestamp out. If it can't, 'the clock is a
  capability' is just a checkbox."
- The denial itself "is genuinely good," but the shell leaking refusals as exit 1 with a
  debug print "means the contract only holds at the top level," and lumping missing-arg with
  missing-capability under exit 3 needs distinguishing somewhere for CI.
- The outcome line "makes these demo programs, not coreutils… stdout-purity is non-negotiable
  for anything calling itself cat or echo." Asked whether anything consumes the typed result
  (composition? program-to-program?).
- Speed: "honestly, fine."
- Asks for next round: how a program is written/built (source, build command, store,
  languages, loop time); the fake-clock substitution; an optional-import program from the
  program's side; what a trap looks like when debugging ("source lines, or printf-debugging
  inside a component model sandwich?"). Parked: distribution/registry.

## Round 3 — authoring, substitution, optional imports, debugging

**Quick answers given:** package-level `only` not tracked in the gaps doc; the
`success(…)`/`ok:` inconsistency and the shell exit-1 leak acknowledged; "does anything
consume the typed result?" — honest no: no pipeline operator, no dataflow, exit code + humans
+ tests only.

**Authoring.** Showed hello's `wit/world.wit` and full `src/lib.rs` (no_std, `bindings!` +
`main!`), the build (`cargo xtask build-guest`, wasm32-unknown-unknown + `wasm-tools
component new`), and a timed loop: touch hello's source → rebuild 2.1 s → `store add` 0.015 s
(content hash deduped against the existing object) → run 0.05 s. Caveats stated: no
per-package build, no watch mode, Rust-only in practice (the SDK macros are Rust; nothing
non-Rust exists in the tree).

**Substitution.** The first attempt blew up and was shown as-is:

```
$ eo9 -c "time.frozen $ hello --name det --excited true"
abnormal: trapped: … core::option::expect_failed … eo9:time/time@0.1.0#now … wasm `unreachable`
```

The unconfigured frozen-clock provider panics at call time (same for `entropy.seeded $ rng`
with no seed). The configured form works and is deterministic:

```
$ eo9 -c "time.frozen --now-seconds 0 --monotonic-ns 0 $ hello --name det --excited true"
[0.000000000] Hello, det!
ok: greeted                                  (identical on the second run)

$ eo9 -c "entropy.seeded --seed 42 $ rng --count 3"     (run twice)
13679457532755275413 / 2949826092126892291 / 5139283748462763858 — byte-identical
```

Volunteered wart: the README's marquee example `entropy.seeded $ cruncher --seed 9 …` "works"
but `describe cruncher` shows cruncher imports nothing — the composition is a no-op.

**Optional imports.** Honest answer: the `-optional` interface flavor exists and is exercised
by synthetic test fixtures, but no shipped example program uses it; showed the fixture WIT
and `eo9 -c "env readwrite"` reporting the would-be-refused state.

**Trap debugging.** `outcomes --mode trap` panics with a message; the output shows
function-name frames only, no source lines, and the panic message is dropped entirely;
`--debug-info` produced byte-identical output (and `-v` showed it reused the cached image).

**Participant reaction (round 3), condensed:**

- "The seeded-RNG run is the first thing in this whole demo that made me sit forward… That's
  deterministic tests without faketime/LD_PRELOAD hacks. I'll give you that one without
  caveats."
- "Except the caveat you handed me yourself: the unconfigured `time.frozen` trapping at
  runtime is precisely the failure mode this system exists to prevent, on its own showcase
  provider… the algebra checks shape, not whether that something is actually in a runnable
  state… `time.frozen` without `--now-seconds` should be refused at compose time, full stop.
  Is that a design gap or just unimplemented?"
- The README no-op example "is a credibility problem, not a docs nit."
- The program source is "honestly cleaner than I expected. The WIT world doubling as the arg
  parser and the result type is a nice trick." Follow-ups: what can a guest actually use
  (no_std — constraint or discipline? std/serde/crates.io? lists, optional args, defaults,
  positional args, stdin as a stream — "if every program is a few named scalar flags in, one
  typed variant out, that's a demo shape, not a tool shape")?
- "Where do crash diagnostics go when stderr is a capability?" The dropped panic message "is
  the one string the programmer wrote specifically for this moment."
- Optional imports having zero real examples "supports my suspicion that the interesting half
  of the model is the half nobody's exercised."
- The outcome line: "it has no business being in stdout. That's a one-line fix away from 'the
  coreutils are actually usable in a pipe.'"
- New asks: the parked distribution question; and "the operating system claim" — "everything
  you've shown me is run-to-completion CLI programs under a launcher… that's not an OS,
  that's a capability-secure program runner with a shell." What's the next milestone; show
  program-to-program composition "even half-broken."

## Round 4 — no_std, panic plumbing, distribution, the OS claim

- Compose-time configuration checking: facilitator answered "design gap, not a queued fix."
- Guest constraints: wasm32-unknown-unknown (not WASI); no_std + alloc is a platform
  constraint; pure-computation crates fine; `std::fs` never. Args: any WIT type is parsed
  generically (so lists/options are expressible in principle) but every shipped program takes
  scalar flags; `read-line` and an io/buffers resource exist but no coreutil reads stdin.
- Panic message: the guest SDK's panic handler discards `PanicInfo` and executes
  `unreachable` — the message dies in the guest; "just unwired," not fundamental.
- Distribution: no export/import, no registry, no signing, nothing resolves versions; today
  it's scp + `store add`. "Versus nix: content-addressing yes, substituters/channels/
  signatures no, not even sketched."
- The OS claim: "what you have been using on the host is exactly what you called it." The
  bare-metal kernel exists (offered as a demo next round). Missing for the OS story, from the
  project's own gaps list: no network provider linked, Message API unstarted, scheduler not
  adopted, nothing long-running.
- Program-to-program: binaries only export `main`; composition is providers onto programs.
  Shown for real: the attenuation chain
  `fs.memfs $ fs.readonly $ readwrite --path note.txt --contents hi` →
  `error: fs("FsError::ReadOnly")`, a typed failure, no host fs anywhere — next to the broken
  half, `fs.memfs $ readwrite` → trap inside memfs `open`.

**Participant reaction (round 4), condensed:**

- "The memfs-readonly-readwrite chain is the second thing today that actually looks like the
  pitch… Except — hang on. The 'working' one is a write being refused by the read-only
  filter, which means memfs probably never got asked to do anything… has `fs.memfs` ever
  successfully served a single operation in any demo? Run a read through the chain. If that
  traps too, the most 'this is actually an algebra' demo you have is a Potemkin chain."
- "Notice how the warts compound at exactly that spot: showcase providers written with
  `.expect()`, a panic handler that eats the message, and backtraces with no source lines.
  The first place a curious newcomer wanders off the happy path, they get the worst debugging
  experience the system can produce."
- no_std is "coherent… but it's also the adoption cliff… 'recompile your tool against a new
  target' is a pitch I can sell to a team; 'rewrite your tool in no_std + alloc against WIT
  worlds' is not." Asked whether a std-port path exists.
- "I don't buy in-principle. Next round, write the dumbest possible program with a
  `list<string>` arg and an `option<u64>`, build it, and run it."
- Distribution: "stop letting anyone say 'like nix' until there's a substituter, a signature,
  or even a paragraph of design."
- Asks: the memfs read test; the list/option program; the QEMU boot with specifics (kernel
  providers, where the store lives, more than one program, cold boot incl. on-metal compile,
  a trap on serial); and "the TCB question, straight" — what's between a guest escaping the
  JIT and owning the machine; "'nothing, that's the bet' is an acceptable answer, but I want
  to hear it said out loud."

## Round 5 — memfs reads, the list/option program, bare metal, the TCB

**memfs reads.** They trap too (`stat`, `list-directory` panic the same way). Facilitator's
answer: "through the shell, fs.memfs has never successfully served a single operation in
anything I can show you… the one place memfs demonstrably works is the integration test
suite, where the harness initializes it through its config interface via the library API — a
path the shell doesn't expose. Today, in the shell, it is the left half of a screenshot."

**The list/option program.** A new guest crate (world: `main: func(names: list<string>,
limit: option<u64>) -> …`) was written, built (~4 s with warm caches), componentized, added
to the store, and run:

```
$ eo9 argtest --names '["alice","bob"]' --limit 5
names=["alice", "bob"] limit=Some(5)
success(reported(2))                                          # exit 0
$ eo9 argtest --names '["alice","bob","carol"]' --limit none  # works, limit=None
$ eo9 argtest --names '["alice","bob"]'
eo9: error: … bad arguments: missing argument `limit`         # exit 3
$ eo9 argtest --names alice --limit 5
eo9: error: … argument `names` is not a valid `list<string>`: invalid value type at 0..5   # exit 3
```

Findings attached: compound types genuinely work end to end with good wrong-type errors; but
an option-typed argument must still be passed explicitly (no defaults of any kind), and there
is no project scaffold — setting up the crate (path dep on the SDK, symlinked WIT deps,
toolchain pin) took longer than writing the program.

**Bare metal.** Kernel image (23 MB ELF, on-target-codegen feature) rebuilt in ~41 s with
warm caches. Headless run: boot → run cruncher → power-off in **0.107 s host wall**, same
digest as usermode, `instantiate + main took 37593 us`. The interactive shell was booted with
commands piped over serial; the kernel-side `env` shows PL011 text, generic-timer/PL031 time,
a counter-seeded splitmix64 entropy stub ("not a CSPRNG"), a read-only store baked into the
kernel image (no disk driver), children never get fs or exec. The `env` text claims
"composition and on-target codegen are not available yet" — and the next typed line,
`time.frozen --now-seconds 0 --monotonic-ns 0 $ hello --name metal --excited true`, composed,
compiled on-target, and ran (`[0.000000000] Hello, metal!`, ~2 s of guest time; the boot
demo's cleaner number is 87 ms to compile a 298-byte component on-target). The demo run does
five programs back to back (incl. an async guest awaiting a real 50 ms kernel-timer sleep) in
218 ms of kernel uptime — but strictly run-to-completion, one task at a time; the scheduler
crate isn't adopted and the executor polls. A trap on the serial console leaves the kernel
alive (it reports the backtrace — with mangled symbols — and powers off cleanly); a
fs-needing child on metal is refused, but as a raw `SpawnError::Internal(... linker ...)`
message, not the friendly usermode text.

**The TCB, said out loud.** Quoting the design doc: "Eo9 does not rely on hardware security
features (like separate privilege domains)… There is no kernel-mode/user-mode boundary."
Everything runs at EL1 in one identity-mapped address space; the MMU is for cache attributes,
not isolation; W^X for published code is still on the hardening list. "If Cranelift
miscompiles a bounds check, the guest owns the machine — there is no second wall. 'Nothing,
that's the bet' is the answer."

**no_std trajectory.** No std-port or std-shim plan exists in the repo's docs.

## The participant's verdict (round 5 reply, lightly condensed, their words)

- "The idea is real and differentiated." Three things they'd "steal": substitution (seeded
  RNG / frozen clock injected without the program knowing or changing), attenuation (a
  read-only filter as just another component in the chain), and the same unmodified
  components running on a host launcher and on bare metal with identical results
  ("docker, bubblewrap, nix, venvs — none of them do those").
- "The product is mostly not built, and the demos are shaped to hide where." memfs has never
  served an operation through the shell; the marquee README composition is a no-op; the
  package-level `only` syntax doesn't parse; the working attenuation chain works because the
  filter blocks before the broken provider. "Every time I stepped one inch off the happy
  path, I got the worst the system has."
- "The deny-by-default story is true at the boundary that matters and fuzzy everywhere else."
  The contract they'd need: "*if it composed, it runs or fails typed — it never traps on
  first-party components.* That's currently false, and it's a design gap, not a backlog item."
- "The adoption math is brutal and the project hasn't faced it." no_std + WIT worlds means no
  ports of existing tools; "the SDK, scaffolding, debugging, and docs [are] more important
  than the kernel — and those are exactly the weakest parts." The TCB bet "belongs in the
  pitch, not just the design doc."
- "Would I use it? Today, no — not even for toys, mostly because of the debugging story.
  Would I keep watching it? Yes." Their five preconditions for re-evaluating: (1)
  compose-time refusal of unconfigured providers, (2) outcome line off stdout and exit codes
  honest through the shell, (3) panic messages and source-line backtraces, (4) `eo9 new` plus
  per-package builds, (5) every example in the README runs as written. "Network and the
  message API can wait; nobody will get far enough to miss them until the first five are
  done."
- The metal demo was "honestly the most impressive thing you've shown… that cross-platform
  determinism is underrated — put it in the pitch." The first time all day "the docs
  underclaimed" (the stale on-metal `env` note).

## Wrap-up (the participant's structured answers, verbatim by their invitation)

**Top 3 pain points**
1. First-party providers trap at runtime when unconfigured — "the single biggest trust hit of
   the day, and it's a design gap (the algebra checks shape, not runnability)."
2. The debugging dead end — panic messages thrown away, no source lines, `--debug-info` does
   nothing visible, raw enum/linker internals as errors.
3. Docs that overclaim — the `only` shorthand, the no-op marquee example, friendlier error
   text in the README than what prints. "Every gap I found today started from a doc claim
   that didn't hold."

**Top 3 missing things**
1. Stdout discipline + a consistent exit-code contract (outcome line in stdout, refusal
   becomes exit 1 through the shell, `success(…)` vs `ok: …`).
2. Developer scaffolding — `eo9 new`, per-package build, watch mode, defaults for
   option-typed args.
3. Any distribution story — export/import, registry, signing, version resolution. "Until
   then, drop the nix comparison."

**Confusing or mis-designed (vs. merely unfinished)**
- The default grant set vs. the pitch — "the honest claim is 'deny-by-default for fs/net/
  exec,' and the pitch should say that"; the strict path is "theoretical" while the only
  tightening syntax is interface-level with versions.
- "Composed successfully" not implying "won't trap."
- The TCB bet is buried in the design doc rather than stated in the pitch.

**Genuinely impressed**
- Substitution (seeded RNG / frozen clock) without changing the program.
- Attenuation as composition, refusing in the program's own typed vocabulary.
- The bare-metal run: same components, same digest as usermode, 0.1 s boot-run-poweroff,
  87 ms on-target codegen.
- The typed-args machinery handling `list<string>`/`option<u64>` with errors that name the
  argument; the usermode missing-fs refusal message ("the best error text in the project").
- "The candor: showing me the breakage instead of steering around it."

**The 10-minute getting-started they'd want**
1. Run `hello`, then immediately `env` and `env hello` — show what was granted and where,
   before anyone asks.
2. The denial: `cat` with no `--fs-root`, exit 3, then the same command granted.
3. The substitution demo (frozen clock or seeded RNG, run twice, identical output) — "the
   'oh, that's why' moment."
4. Write-your-own: `eo9 new` (once it exists), the WIT world, four-line body, build,
   `store add`, run, including the exit-code table.
5. One attenuation chain that actually exercises every component in it.
   It should absolutely not: show any command that doesn't run as written, demo a no-op
   composition, or let the reader hit an unconfigured provider — "any one of them ends the
   evaluation for someone like me."

## Findings

### Bugs / rough edges verified during the session

1. **Unconfigured stub providers trap at runtime.** `time.frozen $ hello`,
   `entropy.seeded $ rng` (no seed), and `fs.memfs $ <anything>` all panic
   (`option::expect_failed`) inside the provider and surface as wasm traps with long
   backtraces. Nothing at compose time checks that a provider is in a runnable state.
2. **fs.memfs is unusable from the shell.** Its `open`/`stat`/`list-directory` all trap;
   `configure` rejects resource-owning providers; the only context in which it works is the
   integration suite via the library API. The shell-level attenuation demo "works" only
   because fs.readonly rejects the write before memfs is reached.
3. **README vs implementation:** the package-level `only eo9:text,eo9:time` form is rejected
   (full `namespace:package/interface` names required, including the `types` interfaces); the
   actual `only` failure message is a Rust enum debug print, not the friendly text shown in
   the README; the marquee `entropy.seeded $ cruncher` example composes a provider onto a
   program that imports nothing (a no-op).
4. **The typed outcome line goes to stdout** (verified by redirect and by `| wc -c`), there is
   no `--quiet`/porcelain flag, and nothing machine-consumes it besides the exit code.
5. **The exit-code contract only holds at the top level.** Through `eo9 -c`/eosh, compose-time
   refusals and runtime traps alike surface as exit 1 with `failure(command-failed(…))`
   wrapping a debug-format string.
6. **Outcome rendering is inconsistent across surfaces:** `success(greeted)` from `eo9 run`,
   `ok: greeted` from the shell.
7. **Crash diagnostics:** the guest SDK panic handler discards the panic message; backtraces
   have no source lines; `--debug-info` produced no observable difference and reused the
   existing cached image; on metal the serial backtrace is additionally not demangled.
8. **Option-typed arguments are not optional:** omitting the flag is "missing argument";
   `--limit none` must be typed; there is no way to express argument defaults.
9. **Provider configure flags are all-mandatory** (e.g. `time.frozen` requires both
   `--now-seconds` and `--monotonic-ns`).
10. **On metal,** a child needing fs is refused with a raw `SpawnError::Internal(... linker
    ...)` message (known gap), and the kernel-side `env` text says composition/on-target
    codegen are "not available yet" while both work in the image as built (stale,
    underclaiming).
11. **No shipped program demonstrates optional imports** — the mechanism exists and is tested
    only by synthetic fixtures, so there is no graceful-degradation example to crib from.
12. The eosh `help` text does not mention the provider-flag configure form; the
    `configure(provider, k=v)` spelling used in some docs does not parse in the shell (the
    working spelling is `provider --flag value $ program`).

### Confusions observed

- The gap between the pitch ("no ambient authority") and the default text/time/entropy grant
  on the direct-run path; `--fs-root` reading as "a flag, not the algebra."
- `option<T>` arguments still being mandatory to spell.
- Which exit code / which rendering you get depends on which front door you used.
- A compose that succeeds can still be a guaranteed runtime trap (unconfigured providers).
- The on-metal `env` note contradicting observed behavior.

### What landed well

- `env`, `env <program>`, and `describe` as inspection tools; the per-import marking.
- The usermode missing-fs refusal: clear message, names the fix, exit 3, before instantiation.
- Substitution: seeded RNG / frozen clock with byte-identical reruns, no program changes.
- Attenuation by composing a read-only filter, failing in the program's own typed vocabulary.
- Typed arguments end to end, including `list<string>`/`option<u64>`, with errors that name
  the argument and expected type.
- Performance: warm runs ~2× `/bin/echo`; content-addressed compile cache with visible
  hit/miss via `-v`; ~2 s edit-to-component loop with warm caches; store add dedup by hash.
- Bare metal: 0.107 s boot-run-poweroff under QEMU, identical digest to usermode, on-target
  Cranelift codegen (87 ms for a tiny component, ~2 s for a real composed program), traps
  contained, clean PSCI power-off, and a usable `env` at the metal prompt.
- The candor of the design doc about the no-privilege-domains bet.

### Feature requests / asks from the participant

- Refuse unconfigured providers at compose time ("if it composed, it runs or fails typed").
- Move the outcome line off stdout (stderr/fd3/`--porcelain`); make exit codes consistent
  through the shell; unify `success(…)`/`ok:` rendering.
- Surface panic messages; source-line backtraces; make `--debug-info` do something.
- `eo9 new` scaffold, per-package guest builds, a watch mode, argument defaults.
- A package-level (or otherwise human-typeable) `only` shorthand.
- An optional-import example program.
- A distribution story (export/import, registry, signing, version resolution) before any nix
  comparison.
- Fix the README so every example runs as written; replace the no-op composition example with
  the seeded-RNG one.
- Put the real claims in the pitch: ambient defaults for text/time/entropy, the
  Cranelift-only TCB bet, and (positively) the cross-platform determinism result.
- Distinguish missing-argument from missing-capability somewhere machine-readable.
- A stated position on non-Rust guests and on whether a std-on-capabilities path will exist.

## Facilitator observations

- The facilitator tripped over the same landmines the participant predicted: the first
  substitution demo attempted (`time.frozen $ hello`) trapped; the configure spelling from the
  status docs did not parse and the working provider-flag form had to be found by trial; an
  early `outcomes --mode trap` run was refused for a missing `--detail` argument. None of
  these were planned as "teaching failures" — they were shown because they happened.
- Demo preparation repeatedly required choosing between "the example as documented" and "the
  form that actually runs"; the README cannot currently be followed verbatim for the algebra
  examples.
- There is no scaffolding for creating a program outside the repo's own workspace; the
  argtest crate needed a hand-written manifest, a path dependency on the SDK, symlinked WIT
  deps, and the pinned toolchain copied over.
- The compile cache, `-v` diagnostics, `store gc`, and the piped-serial QEMU session all
  behaved exactly as needed for live demonstration — the inspection/observability surface
  (env/describe/-v) is genuinely demo-friendly.
- Timing methodology: release build; `/bin/echo` and `eo9` timed with the shell's `time`; the
  compile cache evicted with `store gc --max-cache-bytes 0` to measure cold paths; the very
  first invocation of the freshly built binary took ~0.6 s wall (one-off, not reproducible).
