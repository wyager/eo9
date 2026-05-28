# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design),
`docs/user-studies/` (external-perspective findings and their triage).

_Last updated: 2026-05-27 (overnight), master at 4962464. Headline: **the eosh shell now boots and runs in
the browser, and the last open algebra-correctness bug (bug 1) is fixed.**_

## Overnight changes (since the 2026-05-27 daytime refresh)

- **Algebra bug 1 FIXED** (`af9cb34`): `configure` is now a synchronous, minimal WIT `func` (binds
  compile-time constants, must not block) — the configured-middleware-over-configured-provider trap is gone
  (the two interposition cases pass). With this, the **algebra-correctness work is essentially complete**:
  the drop-law fix, `executable_bytes` (renamed-residual artifacts run), `describe --wiring`, the soundness
  corpus, and the generative property suite (`26ddc28`) are all in. Only the spec's abstract
  `≡`/instance-identity clarification remains.
- **Browser `eosh>` shell** (`4962464`): the real `eo9-component` algebra + the unmodified eosh run in the
  wasm32+Pulley `/vm` blob; the shell resolves `/bin`, runs 16 programs (4 examples + 12 coreutils), and
  renders typed outcomes, with in-blob fs/io and JSPI read-line. (Caveats below.)
- **Metal idle is now 0.0% host CPU** (`388962f`): a real UART-RX interrupt + event-driven WFI (was ~1%
  timer-poll). Plus **Ctrl-C, kill-cascade, and a per-child spawn cap** on metal (`a127861`).
- **`describe --wiring`** (`00bfaf7`): a full composition tree showing each provider layer and what it
  satisfies/seals/attenuates (provenance is in-memory only — content hash and compile cache unchanged).
- **Readable guest traps** (`dc53e70`): a trapped program now reports a clean reason (trap kind + demangled
  symbol backtrace, addresses/hashes stripped).
- **Design-call fixes** (`a212595`): `only eo9:text` package shorthand, the `-c` outcome line on stderr, and
  `-v` child-grant visibility.

## Works today (usermode, on master, CI-gated)

- `eo9 run <name-or-path> [--flags]` — real components end to end: WAVE-typed flags checked against the
  program's signature, three-way outcomes (`success`/`failure`/`abnormal`) with exit codes 0/1/2/3, the
  outcome line on **stderr** by default (`--outcome` to override) so pipes carry only program output,
  store-resolved dotted names or host paths, immutable `open-exec` (APFS clonefile, refuse-by-default on
  non-COW), memory limits, `--max-fuel` (a runaway program is killed → `abnormal`, exit 2), and a compile
  cache whose hits launch from the cached image with zero codegen. A first run on an empty store seeds the
  ~36 bundled components automatically.
- Filesystem access is opt-in: `--fs-root <dir>` grants a rooted fs capability (jailed; opened descriptors
  re-verified to still resolve under the root); without it, fs-requiring programs are refused with a clear
  message and fs-optional programs observe absence.
- **Coreutils** (12 guest programs, each importing only what it needs): `cat ls find wc head stat mkdir rm
  cp touch echo rng` — fs tools run only under a granted root, `echo` needs only text, `rng` consumes real
  entropy (`entropy.seeded --seed 43 $ rng --count 3` is the canonical deterministic-RNG demo).
- `eo9 store add|ls|gc`, `eo9 describe` (+ `describe --wiring` for the full composition tree), `eo9 compile`.
- Deterministic execution proven on real components: seeded/frozen providers compose onto unmodified
  programs and runs are byte-identical and sealed against ambient providers (integration suites).
- Invoker-side provider configuration via the algebra: `configure` is now **synchronous** (binds
  compile-time constants, never blocks), covering freestanding sync and async-API providers
  (`configure(time.frozen, …) $ configure(entropy.seeded, seed=…) $ program`), including the previously-
  trapping case where a configured provider's `configure` reaches through another configured provider.
  Resource-owning providers still configure by composition only (see GAPS). **Unconfigured configurable
  providers never trap**: the standard stubs self-bind documented defaults (empty memfs, the 2000-01-01
  frozen instant, 1 ms fuzzy granularity, seed `0xE09`); flags/`configure` override.
- **Algebra correctness**: authority-free imports drop cleanly per the no-op-drop law (`fs.none`/`time.none`/
  `text.none`/`entropy.none` compose+validate); renamed-residual artifacts compile and run (the compiler is
  fed the implements-stripped `executable_bytes` while describe/hash/cache keep the full bytes); a configured
  middleware over a configured provider now runs (bug 1 fixed); a **generative property suite** + a seeded
  soundness corpus assert encoder-validates-when-defined, the action law, sealing, `only`, and rename across
  enumerated component triples.
- **`fs.overlay` + algebraic layering**: an ordinary `eo9:fs` middleware with two named slots (`upper`,
  `lower`) — reads upper-first, listings union with upper winning, writes to lower, upper never mutated. With
  the root-handle-in-the-interface convention (`fs-impl` lives in `eo9:fs/fs`), guest-leaf layering works
  purely in the algebra: `with memfs-A as upper, memfs-B as lower $ fs.overlay $ readwrite` composes,
  validates, and round-trips end-to-end.
- **`eo9 shell` / eosh**: tab completion, capability-aware `env` (+ `env <program>`), friendly error
  rendering (no raw enum/debug strings for `only`/spawn/configure failures), readable guest-trap reasons.
  The session filesystem is layered — programs read-only at `/bin`, the user's `--fs-root` data writable at
  `/` — and **children inherit the full session environment every generation** (text/time/entropy, the
  layered fs, and the whole `eo9:exec` surface), so **`eosh> eosh` works**: the nested shell resolves
  `/bin`, spawns, and composes. Restriction is composition: `only eo9:text/text $ <prog>` strips exec/fs.
- **Bare metal (aarch64/QEMU)** — boots to an interactive eosh over serial; the unmodified shell runs,
  describes, and composes programs; with `wasm-codegen` the kernel compiles compositions to native aarch64
  on the machine with Cranelift (`entropy.seeded $ cruncher … → ok: digest(…)` with no baked artifact).
  **Idle is 0.0% host CPU** (UART-RX interrupt + event-driven WFI); guest sleeps wake on the timer interrupt.
  **Child fuel + preemption**: children run on a sliced fuel budget so a compute-bound child can't take the
  machine (the boot demo shows a short job finishing while a long one and an unbounded spinner run, then the
  spinner is killed), and `program=<name> … max-fuel=N` kills a runaway with `abnormal(killed)`. **Ctrl-C**
  interrupts a foreground job and returns to the prompt; **kill cascades** to descendants; a **per-child
  spawn cap** bounds fork-bomb-style nesting. **Children inherit the full session environment (fs/io/exec),
  so `eosh> eosh` works on metal** — the nested shell runs a program and an on-target-compiled composition
  as a grandchild; `only` still strips the surface. Headless modes (`demo`, `program=<name> [k=v …]`)
  self-power-off.
- **The website (`www/`)**: static site + standalone Rust server with built-in ACME TLS, security headers
  (CSP/nosniff/Referrer-Policy/COOP; HSTS on TLS only), Accept-Encoding negotiation of pre-compressed
  `.br`/`.gz` siblings, and **Cloudflare-friendly caching**: large immutable assets ship under
  content-fingerprinted URLs (`web-eo9.<hash>.wasm`, resolved via `vm/assets.json`) served `public,
  max-age=31536000, immutable` with **no per-request hashing**, while short-cached HTML/manifest flip to the
  new URLs the instant a rebuild changes the bytes. Two in-browser demos: `/try` (jco-transpiled example
  components on the browser's engine, grant/revoke demo) and **`/vm` — the real runtime stack** as a ~6 MiB
  wasm32+Pulley blob (~1.2 MiB brotli on the wire): the **eosh shell boots and runs**, 16 programs
  (examples + coreutils) execute against browser root providers + an in-blob in-memory fs, with fuel +
  entropy parity with native and JSPI suspension for sleep/read-line. A `cargo xtask check-web-vm` drift
  guard keeps the committed assets current.
- **README.md** — every example verified against the current build.
- `cargo xtask ci` — one gate over the host, guest, and kernel workspaces; build-guest precedes tests.
- **Six user studies** (CLI dev, security engineer, embedded/OS engineer, web-platform dev, PL researcher,
  novice) with a cross-session triage in `docs/user-studies/00-synthesis.md`.
- **Upstreaming**: per-family feasibility reports in `docs/upstreaming/`, plus three locally staged,
  review-ready contribution branches (wasmtime CM-async no_std in `~/code/wasmtime-nostd`; wit-parser no_std
  decoding and wasm-wave no_std `wit` in `~/code/wasm-tools-nostd`) awaiting owner review/push.

## In the browser today — and the honest caveats

The `/vm` page runs the **real stack**: `eosh>` boots, 16 programs run (hello/cruncher/outcomes/readwrite +
the 12 coreutils), the real algebra does `load`/`describe`/`only`, and execution is genuine wasmtime+Pulley
with fuel and entropy matching native byte-for-byte. Two things are NOT yet real in the browser (both in
progress, `area/18-web-complete`):

- **`$`/`&` composition** currently returns a clean "composition needs the compiler, not in the browser yet"
  refusal — there is no in-blob codegen (cranelift is std/mmap-blocked on wasm32). The fix in progress is a
  bounded server-side `/vm/compile` endpoint that fuses+compiles store-program compositions and returns a
  Pulley image ("compiled on the server").
- **`only`** in the browser records the allow-set but `spawn` still links all root providers, so it does not
  yet *narrow* — the restricted-linker wiring is in progress.

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages incl. `eo9:pci`; root handles live in their API interface for fs; `configure` is sync) | `wit/` | v0 complete; message/perf are placeholders; disk/net/pci to migrate to the root-handle convention when needed |
| Component algebra: `$`, `&`, `only`, `rename`, `configure`, describe/load/save (+ `--wiring`) | `crates/eo9-component` | complete incl. law tests, soundness corpus, and a generative property suite; drop-law/renamed-residual/configured-middleware (bug 1) all fixed; only the spec `≡`/identity clarification remains |
| Runtime: fuel-metered resumable tasks, WAVE args/outcomes, caps, fs/io + text/time/entropy linking, exec provider, image serialization, readable trap reasons | `crates/eo9-runtime` | usermode-complete for current scope |
| Scheduler (no_std, conserved fuel, deterministic policy) | `crates/eo9-sched` | complete for single-core; not adopted by the CLI/kernel loop (kernel uses round-robin + fuel-slicing) |
| Module store + compile cache (content-addressed, blake3-verified, hash-keyed) | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk, clone-first open-exec, post-open fd re-verification) | `crates/eo9-providers-unix` | complete; net deferred |
| eofs core (CoW/Merkle, lz4-by-default, snapshots, crash-consistency) | `crates/eofs-core` | engine complete; provider/mkfs not started |
| Guest SDK + 19 stub providers (none/deny families, seeded, memfs, frozen/fuzzy clocks, readonly, pci-none, fs.overlay) with documented defaults | `guest/` | complete for current WIT; pci.deny/filtered, loopback, capture deferred; guest wit-bindgen is a temporary git pin (0.249 family) |
| Coreutils (cat, ls, find, wc, head, stat, mkdir, rm, cp, touch, echo, rng) | `guest/coreutils/` | complete; seeded under bare names; also run in the browser |
| eosh (full grammar, evaluator, env/envinfo, friendly error rendering) | `guest/eosh` | done for current scope; runs as `eo9 shell`, recursively under itself (usermode + metal), and in the browser |
| Integration suites (capability laws, determinism, invoker-configured env, default configuration, overlay layering, compose diagnostics, soundness corpus, generative property suite, interposition, kill/linearity, CLI transcripts) | `tests/eo9-integration` + `crates/eo9/tests` | green; QEMU tier not started |
| Usermode binary `eo9` (run/store/describe/compile/cache/shell, layered session, recursive child env, stderr outcomes, --max-fuel, seeding, --wiring) | `crates/eo9` | done for current scope |
| Embeddable runtime (`Eo9` builder, Sandbox + Host backends behind a `ProviderSource` seam) | `crates/eo9-embed` | complete; foundation for `eo9 bundle` and the wasm32 backend |
| Website + server + `/try` + `/vm` (real-stack wasm32+Pulley blob; eosh shell + 16 programs in-browser; browser providers, HTTP store, JSPI; compression, security headers, fingerprinted immutable caching) | `www/` | deployable; in-browser composition (`/vm/compile`) + `only`-narrowing in progress; /try jco-dedup queued |
| Bare-metal kernel (aarch64: boot, MMU, GICv2 + UART-RX event-driven idle, kernel providers, sync + async guests, baked-in store, boot-to-interactive-eosh, on-target Cranelift codegen, interactive composition, child fuel/preemption, Ctrl-C/kill-cascade/per-child-cap, nested eosh; vendored CM-async + compile-layer + algebra no_std forks) | `kernel/` | MVP + full preemption/interrupt/containment depth complete; W^X in progress; riscv64/x86_64 + QEMU test tier not started |

## In progress right now

- **Web demo completion** (`area/18-web-complete`): `only`-attenuation via a restricted linker (so `only`
  actually narrows in the browser), seeding a provider into `/bin` to exercise `$`/`&`, and a bounded
  server-side `/vm/compile` endpoint so in-browser `$`/`&` composition actually compiles+runs.
- **W^X for JIT code pages** (`area/12-wx`): map on-target-generated code executable-not-writable — the last
  metal-depth item.

## Next up (rough order)

1. **Algebra — remaining**: define `≡`/instance-identity in SPEC; surface the `ProviderExportsUnused`
   ("exports match nothing") warning host-side.
2. **WIT follow-ups** (kept stable this wave): the `eo9:rt/diagnostics` post-trap export for guest panic
   *messages*; an `eo9:exec` wiring field so eosh (not just the CLI) can show the `--wiring` tree; an eosh
   `program-failure` three-way class so `shell -c` gives honest 0/1/2/3 exit codes.
3. **Web**: finish the in-progress `/vm/compile` + `only`-narrowing; the `/try` jco glue dedup; a
   reproducible-build fix for the path-dependent wasm32 blob hash; COEP/Permissions-Policy headers; blob-size
   trim (lazy-fetch `/bin`).
4. **Kernel breadth**: riscv64/x86_64 ports and the QEMU test tier (real-board bring-up ordering is the one
   roadmap question the owner will settle once the demos look good — depth before breadth before hardware).
5. Demo packaging (`cargo install eo9` without a checkout) and the Bundle milestone (`eo9 bundle` on
   eo9-embed); `eo9 new` scaffold + per-package guest builds.
6. eo9:pci follow-ups (deny/filtered stubs, a virtio-over-PCI consumer); net provider + Message API; eofs
   milestone 2+ (provider, mkfs, store-on-eofs, writable storage on metal).
7. Housekeeping: crates.io name; upstream PR submission when the owner opens the staged branches.

See `GAPS.md` for known limitations and the user-study triage.
