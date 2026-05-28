# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design),
`docs/user-studies/` (external-perspective findings and their triage).

_Last updated: 2026-05-27, master at 14c0443. The bare-metal MVP (boot-to-eosh + on-target Cranelift codegen
+ interactive composition on aarch64/QEMU) remains complete. Since the last refresh this batch landed: the
PL-study algebra fixes (the authority-free drop law, `executable_bytes` so renamed-residual artifacts run,
compose diagnostics, a seeded soundness corpus), kernel child-fuel preemption + a reworked drive loop +
**nested `eosh` on bare metal**, and a website pass (compression, security headers, and Cloudflare-friendly
content-fingerprinted immutable URLs with no per-request blob hashing). Master is pushed by the owner to
GitHub (github.com:wyager/eo9)._

## Works today (usermode, on master, CI-gated)

- `eo9 run <name-or-path> [--flags]` — real components end to end: WAVE-typed flags checked against the
  program's signature, three-way outcomes (`success`/`failure`/`abnormal`) with exit codes 0/1/2/3, the
  outcome line on **stderr** by default (`--outcome` to override) so pipes carry only program output,
  store-resolved dotted names or host paths, immutable `open-exec` (APFS clonefile, refuse-by-default on
  non-COW), memory limits, `--max-fuel` (a runaway program is killed → `abnormal`, exit 2), and a compile
  cache whose hits launch from the cached image with zero codegen. A first run on an empty store seeds the
  ~36 bundled components automatically (no shell start required).
- Filesystem access is opt-in: `--fs-root <dir>` grants a rooted fs capability (jailed; opened descriptors
  are re-verified to still resolve under the root); without it, fs-requiring programs are refused with a
  clear message and fs-optional programs observe absence.
- **Coreutils** (12 guest programs, each importing only what it needs): `cat ls find wc head stat mkdir rm
  cp touch echo rng` — fs tools run only under a granted root, `echo` needs only text, `rng` consumes real
  entropy (`entropy.seeded --seed 43 $ rng --count 3` is the canonical deterministic-RNG demo).
- `eo9 store add|ls|gc`, `eo9 describe`, `eo9 compile`; store + cache under `~/.eo9/store`.
- Deterministic execution proven on real components: seeded/frozen providers compose onto unmodified
  programs and runs are byte-identical and sealed against ambient providers (integration suites).
- Invoker-side provider configuration via the algebra covers freestanding sync **and** async APIs
  (`configure(time.frozen, …) $ configure(entropy.seeded, seed=…) $ program`); resource-owning providers
  still configure by composition only (see GAPS). **Unconfigured configurable providers never trap**: the
  standard stubs self-bind documented defaults (empty memfs, the 2000-01-01 frozen instant, 1 ms fuzzy
  granularity, seed `0xE09`), so `time.frozen $ hello`, `entropy.seeded $ rng`, `fs.memfs $ readwrite` all
  run deterministically; flags/`configure` override.
- **Algebra correctness**: an authority-free import is dropped cleanly per the no-op-drop law (so
  `fs.none`/`time.none`/`text.none`/`entropy.none` compose+validate instead of failing the encoder), a
  renamed-residual artifact compiles and runs (the compiler is fed the implements-stripped
  `executable_bytes` form while describe/store-hash/cache-key keep the full bytes), and a seeded soundness
  corpus asserts every shipped stub × consumer yields a validated component or a typed refusal — never an
  internal encoder failure. (One composition bug remains; see GAPS.)
- **`fs.overlay` + algebraic layering**: an ordinary `eo9:fs` middleware with two named slots (`upper`,
  `lower`) — reads upper-first, listings union with upper winning, writes to lower, upper never mutated.
  With the root-handle-in-the-interface convention (`fs-impl` now lives in `eo9:fs/fs`), guest-leaf layering
  works purely in the algebra: `with memfs-A as upper, memfs-B as lower $ fs.overlay $ readwrite` composes,
  validates, and round-trips end-to-end in the integration suite.
- **`eo9 shell` / eosh**: tab completion, capability-aware `env` (+ `env <program>`), friendly error
  rendering (no raw enum/debug strings for `only`/spawn/configure failures). The session filesystem is
  layered — programs read-only at `/bin`, the user's `--fs-root` data writable at `/` — and **children
  inherit the full session environment every generation** (text/time/entropy, the layered fs, and the whole
  `eo9:exec` surface), so **`eosh> eosh` works**: the nested shell resolves `/bin`, spawns, and composes.
  Restriction is composition: `only eo9:text/text $ <prog>` strips exec/fs before spawn.
- **Bare metal (aarch64/QEMU)** — boots to an interactive eosh over serial; the unmodified shell runs,
  describes, and composes programs; with `wasm-codegen` the kernel compiles compositions to native aarch64
  on the machine with Cranelift (`entropy.seeded $ cruncher … → ok: digest(…)` with no baked artifact).
  GICv2 + timer IRQ + `wfi` idle: an idle prompt costs ~1% host CPU (was ~100%); guest sleeps wake on the
  timer interrupt. **Child fuel + preemption landed**: children run on a sliced fuel budget so a
  compute-bound child no longer takes the machine (the boot demo shows a short job finishing while a long
  one and an unbounded spinner run, then the spinner is killed), and `program=<name> … max-fuel=N` kills a
  runaway with `abnormal(killed)`. **Children now inherit the full session environment (fs/io/exec), so
  `eosh> eosh` works on metal** — the nested shell resolves `/bin`, runs a program, and runs an on-target-
  compiled composition as a grandchild; `only` still strips the surface. Headless modes (`demo`,
  `program=<name> [k=v …]`) self-power-off.
- **The website (`www/`)**: static site + standalone Rust server with built-in ACME TLS, security headers
  (CSP/nosniff/Referrer-Policy/COOP; HSTS on TLS only), Accept-Encoding negotiation of pre-compressed
  `.br`/`.gz` siblings, and **Cloudflare-friendly caching**: large immutable assets ship under
  content-fingerprinted URLs (`web-eo9.<hash>.wasm`, resolved via a `vm/assets.json` manifest) served
  `public, max-age=31536000, immutable` with **no per-request hashing**, while the short-cached HTML/manifest
  flip to the new URLs the instant a rebuild changes the bytes. Two in-browser demos: `/try` (jco-transpiled
  example components on the browser's engine, grant/revoke demo) and `/vm` (the **real runtime stack** as a
  ~1.2 MiB wasm32+Pulley blob → ~320 KB brotli on the wire: store-fetched programs with typed args/outcomes,
  browser root providers, fuel + entropy parity with native, JSPI suspension for sleep/read-line; retail-
  Chrome self-test 19/19). A `cargo xtask check-web-vm` drift guard keeps the committed assets current.
- **README.md** — every example verified against the current build (install order, full interface refs,
  configured/default provider forms, stderr outcomes, recursive eosh, layered session, metal transcript).
- `cargo xtask ci` — one gate over the host, guest, and kernel workspaces; build-guest precedes tests.
- **Six user studies** (CLI dev, security engineer, embedded/OS engineer, web-platform dev, PL researcher,
  novice) with a cross-session triage in `docs/user-studies/00-synthesis.md`; round-1 and round-2 fix-now
  items are merged, the rest tracked in GAPS.
- **Upstreaming**: per-family feasibility reports in `docs/upstreaming/`, plus three locally staged,
  review-ready contribution branches (wasmtime CM-async no_std in `~/code/wasmtime-nostd`; wit-parser
  no_std decoding and wasm-wave no_std `wit` in `~/code/wasm-tools-nostd`) awaiting owner review/push.

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages incl. `eo9:pci`; root handles live in their API interface for fs) | `wit/` | v0 complete; message/perf are placeholders; disk/net/pci to migrate to the root-handle convention when needed |
| Component algebra: `$`, `&`, `only`, `rename`, `configure`, describe/load/save | `crates/eo9-component` | complete incl. law tests + soundness corpus; drop-law + renamed-residual bugs fixed; one PL-study bug remains (configured-middleware trap → event-driven binder, see GAPS) |
| Runtime: fuel-metered resumable tasks, WAVE args/outcomes, caps, fs/io + text/time/entropy linking, exec provider, image serialization | `crates/eo9-runtime` | usermode-complete for current scope |
| Scheduler (no_std, conserved fuel, deterministic policy) | `crates/eo9-sched` | complete for single-core; not yet adopted by the CLI loop or kernel (kernel uses round-robin + fuel-slicing) |
| Module store + compile cache (content-addressed, blake3-verified, hash-keyed) | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk, clone-first open-exec, post-open fd re-verification) | `crates/eo9-providers-unix` | complete; net deferred |
| eofs core (CoW/Merkle, lz4-by-default, snapshots, crash-consistency) | `crates/eofs-core` | engine complete; provider/mkfs not started |
| Guest SDK + 19 stub providers (none/deny families, seeded, memfs, frozen/fuzzy clocks, readonly, pci-none, **fs.overlay**) with documented defaults | `guest/` | complete for current WIT; pci.deny/filtered, loopback, capture deferred; guest wit-bindgen is a temporary git pin (0.249 family) |
| Coreutils (cat, ls, find, wc, head, stat, mkdir, rm, cp, touch, echo, rng) | `guest/coreutils/` | complete; seeded under bare names |
| eosh (full grammar, evaluator, env/envinfo, friendly error rendering) | `guest/eosh` | done for current scope; runs as the `eo9 shell` and recursively under itself (usermode + metal) |
| Integration suites (capability laws, determinism, invoker-configured env, default configuration, overlay layering, compose diagnostics, soundness corpus, kill/linearity, CLI transcripts) | `tests/eo9-integration` + `crates/eo9/tests` | green; fully generative property suite still queued; QEMU tier not started |
| Usermode binary `eo9` (run/store/describe/compile/cache/shell, layered session, recursive child env, stderr outcomes, --max-fuel, seeding) | `crates/eo9` | done for current scope |
| Embeddable runtime (`Eo9` builder, Sandbox + Host backends behind a `ProviderSource` seam) | `crates/eo9-embed` | complete; foundation for `eo9 bundle` and the wasm32 backend |
| Website + server + `/try` + `/vm` (real-stack wasm32+Pulley blob, browser providers, HTTP store, JSPI; compression, security headers, fingerprinted immutable caching) | `www/` | deployable; /vm milestone 3 (fs/io providers → eosh in browser) and the /try jco-dedup queued |
| Bare-metal kernel (aarch64: boot, MMU, GICv2 + WFI idle, kernel providers, sync + async guests, baked-in store, boot-to-interactive-eosh, on-target Cranelift codegen, interactive composition, child fuel/preemption, nested eosh; vendored CM-async + compile-layer + algebra no_std forks) | `kernel/` | MVP + preemption + nested-eosh complete; hardening/breadth remain — W^X, UART-RX idle, kill-cascade, riscv64/x86_64, QEMU test tier |

## In progress right now

- **/vm milestone 3** (eosh in the browser): fs + io-buffer providers in the blob, the exec/store surface,
  and a callback-ABI read/sleep demo guest — queued to dispatch against plan/18 D11.

## Next up (rough order)

1. **Algebra correctness — remaining**: fix the configured-middleware-over-configured-provider trap (needs an
   event-driven / two-phase binder), build the fully generative property-test suite over component triples,
   surface the `ProviderExportsUnused` ("exports match nothing") warning host-side, and define `≡`/instance
   identity in SPEC.
2. **/vm milestone 3** (above), then the `/try` jco glue dedup and the deferred web items (COEP/Permissions-
   Policy headers; a reproducible-build fix for the path-dependent wasm32 blob hash).
3. **Kernel hardening/breadth**: UART-RX interrupt (true event-driven idle) + the idle-waker queue; parent
   kill cascading to orphaned grandchildren; a per-child hard fuel cap for shell spawns; W^X for JIT pages;
   then riscv64/x86_64 ports and the QEMU test tier (real-board bring-up ordering is an open owner decision).
4. Demo packaging (`cargo install eo9` without a checkout) and the Bundle milestone (`eo9 bundle` on
   eo9-embed); `eo9 new` scaffold + per-package guest builds.
5. eo9:pci follow-ups (deny/filtered stubs, a virtio-over-PCI consumer); net provider + Message API; eofs
   milestone 2+ (provider, mkfs, store-on-eofs, writable storage on metal).
6. Housekeeping: crates.io name; upstream PR submission when the owner opens the staged branches.

See `GAPS.md` for known limitations, open owner decisions, and the user-study triage.
