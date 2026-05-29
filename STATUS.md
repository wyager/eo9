# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design),
`docs/user-studies/` (external-perspective findings and their triage).

_Last updated: 2026-05-29, master at 6f3c405. Headline: **Eo9 now runs at parity on three bare-metal
architectures (aarch64, riscv64, x86_64 — boot-to-eosh, on-target compilation from W^X pages, preemption,
Ctrl-C, ~0% idle), has real wasm device drivers (virtio-blk, virtio-net) over `eo9:pci`, a layered network
stack with sockets on metal through three composed wasm layers, and persistent storage (eofs over a host
file in usermode, over a real virtio disk on metal, surviving power cycles).** Guest panics now carry their
message into `trapped(reason)` on every target._

## The drivers / networking / persistence / three-architecture wave (2026-05-28 → 29)

- **Layered networking** (`04c0eae`, SPEC `a6a4275`): `eo9:net` is now three independent capabilities —
  `l2` (frames/MACs), `l3` (IP/routes/raw datagrams), `l4` (TCP/UDP sockets) — each with its own root
  handle, `.none`/`.deny` stubs per layer (seeded under dotted names, `36a0878`), and `net.l4.loopback`,
  an in-memory transport that gives tests working sockets with no lower layers.
- **Real wasm device drivers over `eo9:pci`**: the kernel gained an opt-in `eo9:pci` root provider (ECAM
  enumeration, config space, BARs, DMA buffers; granted only via the `pci` boot token) plus an `lspci`
  example (`fe6a143`); then **`disk.virtio`** — a virtio-blk driver component exporting `eo9:disk`
  (`cce3036`) — and **`net.virtio`** — a virtio-net driver exporting `eo9:net/l2` (`59c0db2`). DMA only
  via `alloc-dma`; each device needs a second explicit grant (`disk` / `net` QEMU flags).
- **Sockets on metal through three composed wasm layers** (`834f72e`): `net.l4.over-l2`, a smoltcp-based
  TCP/IP middleware (smoltcp 0.12, guest-workspace only), imports l2 and exports l4. At the metal prompt,
  `net.virtio $ net.l4.over-l2 $ l4check` resolves a real DNS name through QEMU's user network — driver →
  TCP/IP stack → a program importing only `eo9:net/l4`, compiled on-target. With `net.l2.deny` underneath
  the same program gets a typed denial in under a second — every layer independently mockable.
- **Persistent storage**: eofs milestone 2+3 — the `fs.eofs` provider component (imports `eo9:disk`,
  exports `eo9:fs`, `877738d`), then a file-backed `--disk <image>` grant + `eo9 mkfs.eofs` + persistence
  across processes in usermode (`fee878b`). On metal, `disk.virtio $ fs.eofs $ readwrite/ls/cat` writes to
  a real virtio disk and the data **survives a full QEMU power cycle** (`cce3036`).
- **riscv64 port complete** (`f651106` → `b6b7403`): per-arch split of the kernel (aarch64 byte-identical),
  then boot/interrupts/timer, host-AOT components + boot-to-eosh, Sv39 + W^X + on-target codegen (one
  vendored crate added: registry cranelift-codegen 0.132.0 with a four-constant no_std fix for the riscv64
  backend, provenance-diffed), and interactive parity (Ctrl-C, 0.0% idle).
- **x86_64 port complete** (`27d4edd` → `6f3c405`): PVH direct boot to long mode, 8259 PIC + PIT, the
  SSE/float-ABI story (kernel stays soft-float; SSE enabled in hardware for generated code; engine ISA
  flags mirror the precompile set with a CPUID load-time probe), then 4 KiB NX page tables + W^X
  (`CR0.WP`+`EFER.NXE`) and on-target codegen. **All three architectures are at functional parity and all
  three are in the `cargo xtask ci` featureless-kernel gate.**
- **Guest panic messages** (`f8dc070`, browser `9047c7f`, SPEC rider `c8e6695`): a write-once
  `eo9:rt/diagnostics.report-panic` import (runtime contract, not a capability — never guest-readable,
  surfaced only inside `trapped(reason)`) carries `panic!` message + location into trap outcomes on
  usermode, metal, and the browser.
- **eosh follow-ups via WIT** (`e7f198b`): `describe` at the eosh prompt shows the composition wiring tree
  (`eo9:exec/component-algebra.wiring`); the eosh world's `program-failure` carries the inner command's
  class so `eo9 shell -c` exits with the same honest 0/1/2/3 contract as `eo9 run`.
- **Kernel store** now bakes 20 entries (eosh, examples, basic coreutils, fs.eofs, both virtio drivers, the
  TCP/IP middleware, l2check/l4check), so the metal shell can `ls /bin`, `cat`, compose drivers, etc.
- **Publishing/packaging**: the `eo9-components` bundle is derived strictly from the component list (49
  components after this wave) and `eofs-core` joined the publish sequence (`b20d3be`) — the
  `cargo install eo9` chain is 8 crates, dry-runs green, awaiting the owner's `cargo publish`.
- **Website**: the site is now two pages — the explanatory front page and an auto-booting try-it shell at
  `/vm` (program pickers and the old jco `/try` page removed, `40a4116`/`94bc94e`); the try-it page carries
  five verified examples (incl. a frozen-clock composition) and an honest "what this is" paragraph; the
  browser blob registers `wiring` and the diagnostics import, so in-browser `describe` shows the tree and
  browser panics carry messages (`9047c7f`); the `www` workspace is in the CI gate, the blob is linted,
  trimmed (opt-level z), and built with `--remap-path-prefix` (`51550e9`).

## Since the overnight batch (2026-05-28)

- **In-blob compiler** (`cbe8fe6`): the kernel's no_std compile fork builds for wasm32 as-is, so `/vm`
  compiles fused compositions client-side (Cranelift→Pulley, ~50–110 ms); the server-side `/vm/compile`
  endpoint and its inputs were **removed** (`d844313`) — the browser VM is fully self-hosted.
- **Positional + variadic arguments** (`55f5615`): bare values fill `main`'s params in order and a final
  `list<string>` is the variadic tail — `cat a.txt b.txt`, bare `ls`, `head --lines 1 a b` (coreutils
  re-signatured; browser blob updated; the kernel codec gained list parsing + the empty-tail default).
- **Upgrade-safe stores** (`8f4eded`): seeded bindings auto-refresh when the binary's bundled set changes
  (user bindings never touched); `eo9 store reseed` + a recovery hint on stale-component spawn failures.
- **Fresh-machine UX**: top-level `Makefile` (`make setup/shell/www/qemu/ci`, auto-runs setup when a tool is
  missing) and `cargo xtask doctor` with friendly missing-tool errors.
- **`cargo install eo9` prep** (`ba2a358`): the `eo9-components` bundle crate + crates.io metadata across the
  publish chain; stable Rust suffices; `cargo xtask package` dry-runs green — the publish sequence is ready
  for the owner to run.
- **Site refresh** (`731a2c3`, `5f3731b`): /vm terminal click/Enter fixes (verified with real browser
  events) and the site copy brought up to date with current reality.

## Works today (usermode, on master, CI-gated)

- `eo9 run <name-or-path> [--flags]` — real components end to end: WAVE-typed flags checked against the
  program's signature, three-way outcomes (`success`/`failure`/`abnormal`) with exit codes 0/1/2/3, the
  outcome line on **stderr** by default (`--outcome` to override) so pipes carry only program output,
  store-resolved dotted names or host paths, immutable `open-exec` (APFS clonefile, refuse-by-default on
  non-COW), memory limits, `--max-fuel` (a runaway program is killed → `abnormal`, exit 2), and a compile
  cache whose hits launch from the cached image with zero codegen. A first run on an empty store seeds the
  bundled components automatically (49 in the bundle), and seeded bindings auto-refresh on upgrade.
- Filesystem access is opt-in: `--fs-root <dir>` grants a rooted fs capability (jailed; opened descriptors
  re-verified to still resolve under the root); without it, fs-requiring programs are refused with a clear
  message and fs-optional programs observe absence.
- **Persistent storage (eofs)**: `eo9 mkfs.eofs <image>` formats a host file with Eo9's native CoW/Merkle
  filesystem; `--disk <image>` grants `eo9:disk` over it (opt-in like `--fs-root`, never ambient); then
  `fs.eofs $ <program>` reads and writes a filesystem that **persists across processes** — and the same
  `fs.eofs` runs on metal over the `disk.virtio` driver (below).
- **Coreutils** (12 guest programs, each importing only what it needs): `cat ls find wc head stat mkdir rm
  cp touch echo rng` — fs tools run only under a granted root, `echo` needs only text, `rng` consumes real
  entropy (`entropy.seeded --seed 43 $ rng --count 3` is the canonical deterministic-RNG demo). Arguments
  are positional and variadic where natural: `cat a.txt b.txt`, bare `ls`, `head --lines 1 a b`. The basic
  set is also baked into the kernel store, so they work at the metal prompt too.
- **Networking is layered and mockable**: `eo9:net/l2`, `/l3`, `/l4` are separate capabilities with
  `.none`/`.deny` stubs per layer; `net.l4.loopback` gives tests working in-memory TCP/UDP with no lower
  layers; `net.l4.over-l2` (smoltcp) turns any l2 into real sockets — on metal that l2 is the `net.virtio`
  driver and a program importing only l4 does a real DNS lookup.
- `eo9 store add|ls|gc|reseed`, `eo9 describe` (+ `describe --wiring` for the full composition tree),
  `eo9 compile`, `eo9 mkfs.eofs`.
- Deterministic execution proven on real components: seeded/frozen providers compose onto unmodified
  programs and runs are byte-identical and sealed against ambient providers (integration suites).
- Invoker-side provider configuration via the algebra: `configure` is **synchronous** (binds compile-time
  constants, never blocks), covering freestanding sync and async-API providers, including configured
  middleware over a configured provider. **Unconfigured configurable providers never trap**: the standard
  stubs self-bind documented defaults; flags/`configure` override.
- **Algebra correctness**: the no-op-drop law, renamed-residual artifacts, configured-middleware (bug 1) all
  fixed; a generative property suite + a seeded soundness corpus assert encoder-soundness, the action law,
  sealing, `only`, and rename; **`≡`, instance identity, and `empty` are now defined in SPEC** (`489b3f5`).
- **`fs.overlay` + algebraic layering**: an ordinary `eo9:fs` middleware with two named slots (`upper`,
  `lower`); guest-leaf layering works purely in the algebra.
- **`eo9 shell` / eosh**: tab completion, capability-aware `env`, friendly error rendering, `describe` with
  the composition wiring tree at the prompt, honest `-c` exit codes (0/1/2/3 matching `eo9 run`), the
  layered session filesystem (`/bin` read-only, `--fs-root` data writable), and full-environment child
  inheritance so **`eosh> eosh` works**; restriction is composition (`only … $ prog`).
- **Diagnostics**: a trapped guest reports a readable reason — the `panic!` message and source location
  (via the write-once `eo9:rt/diagnostics` sink) plus a demangled, address-free backtrace — on usermode,
  metal, and in the browser.
- **Bare metal — three architectures at parity (aarch64, riscv64, x86_64 under QEMU)**: each boots to an
  interactive eosh over serial from a 20-entry baked store, runs host-AOT components, and **compiles
  compositions on-target with Cranelift from W^X code pages** (same digests and seeded-entropy values on
  all three). Child fuel + preemption, Ctrl-C, kill-cascade, per-child spawn caps, event-driven idle
  (~0% host CPU), nested eosh, and clean self-power-off everywhere. On aarch64 the opt-in `eo9:pci`
  provider (boot token `pci`) enables the wasm drivers: `lspci`, `disk.virtio $ fs.eofs $ …` (data
  persists across power cycles), and `net.virtio $ net.l4.over-l2 $ l4check` (real DNS through slirp).
  Headless modes (`demo`, `program=<name> [k=v …]`) self-power-off on every arch.
- **The website (`www/`)**: a two-page static site + standalone Rust server with built-in ACME TLS,
  security headers, Accept-Encoding negotiation of pre-compressed siblings, and content-fingerprinted
  immutable caching (no per-request hashing). The try-it page **auto-boots the real eosh shell** in a
  wasm32+Pulley blob (~8.8 MiB raw / ~1.7 MiB brotli): 16+ programs, the in-blob Cranelift→Pulley compiler
  (client-side composition, no server), genuine `only`-narrowing, browser panic messages, and five verified
  examples below the terminal. The old jco `/try` page and its build machinery are removed. `check-web-vm`
  guards asset drift; the `www` workspace is in the CI gate.
- **README.md** — examples verified against the build (the `cat` examples use positional args).
- `cargo xtask ci` — one gate over the host, guest, kernel (all three bare-metal targets), and www
  workspaces; build-guest precedes tests.
- **Six user studies** (CLI dev, security engineer, embedded/OS engineer, web-platform dev, PL researcher,
  novice) with a cross-session triage in `docs/user-studies/00-synthesis.md`.
- **Upstreaming**: per-family feasibility reports in `docs/upstreaming/`, plus three locally staged,
  review-ready contribution branches (wasmtime CM-async no_std in `~/code/wasmtime-nostd`; wit-parser no_std
  decoding and wasm-wave no_std `wit` in `~/code/wasm-tools-nostd`) on ice awaiting owner review/push.

## In the browser today

The try-it page (`/vm`) auto-boots the **real stack**: `eosh>` comes up with no clicks, 16+ programs run,
the real algebra does `load`/`describe`/`compose`, and execution is genuine wasmtime+Pulley with fuel and
entropy matching native byte-for-byte.

- **Composition compiles in the blob**: the kernel's no_std compile fork runs in the browser targeting
  Pulley, so fused compositions (`$` and `&`, both harness-covered) compile client-side in ~50–110 ms with
  no server involvement.
- **`only` genuinely narrows**: a child is instantiated with a linker restricted to the admitted import
  set; `describe` shows the wiring tree; guest panics carry their message into the trapped reason.
- **Honest caveats**: programs are interpreted (Pulley), so compute-heavy work is slower than native/metal;
  the blob hash is path-dependent across checkout directories (~410-byte cargo-metadata residue; same-path
  rebuilds are byte-identical); every guest-SDK change re-fingerprints all `/vm` assets (~11 MB of binary
  churn per such merge); a click-through on the **live deployed** site awaits the owner's next
  push/redeploy (headless-Chrome verification with real keyboard events is part of the workflow now).

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages; layered `eo9:net` l2/l3/l4; `eo9:pci`; `eo9:rt/diagnostics`; sync `configure`; root handles in the API interface for fs) | `wit/` | v0 complete; message/perf are placeholders; `eo9:disk` size/flush ops and an `l4-over-l2-config` interface are queued |
| Component algebra: `$`, `&`, `only`, `rename`, `configure`, describe/load/save (+ wiring) | `crates/eo9-component` | complete incl. law tests, soundness corpus, generative property suite; `≡`/identity/`empty` defined in SPEC |
| Runtime: fuel-metered resumable tasks, WAVE args/outcomes, caps, fs/io/disk + text/time/entropy linking, exec provider (incl. `wiring`), diagnostics sink, image serialization, readable trap reasons | `crates/eo9-runtime` | usermode-complete for current scope |
| Scheduler (no_std, conserved fuel, deterministic policy) | `crates/eo9-sched` | complete for single-core; not adopted by the CLI/kernel loop |
| Module store + compile cache (content-addressed, blake3-verified, hash-keyed) | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk incl. the file-backed `--disk` device, clone-first open-exec, post-open fd re-verification) | `crates/eo9-providers-unix` | complete; host net root deferred (the layered guest stack covers metal) |
| eofs (CoW/Merkle engine, lz4, snapshots, crash-consistency; `fs.eofs` provider; `mkfs.eofs`) | `crates/eofs-core` + `guest/stubs/fs-eofs` | engine + provider + mkfs done; persistence verified in usermode (host file) and on metal (virtio disk); store-on-eofs queued |
| Guest SDK + stub/driver components: none/deny families, seeded, memfs, frozen/fuzzy clocks, readonly, fs.overlay, the 7 layered net stubs + `net.l4.loopback`, `fs.eofs`, `disk.virtio`, `net.virtio`, `net.l4.over-l2` | `guest/` | complete for current WIT; `pci.deny`/`pci.filtered` and `text.capture` deferred; guest wit-bindgen still a git pin |
| Coreutils (cat, ls, find, wc, head, stat, mkdir, rm, cp, touch, echo, rng) | `guest/coreutils/` | complete; positional/variadic args; run in usermode, the browser, and the metal store |
| eosh (full grammar, evaluator, env/envinfo, describe-with-wiring, program-failure classes, friendly errors) | `guest/eosh` | done for current scope; runs as `eo9 shell`, recursively, on all three metal arches, and in the browser |
| Integration suites (capability laws, determinism, configured env, overlay, soundness corpus, property suite, interposition, net loopback/deny, eofs persistence, kill/linearity, CLI transcripts) | `tests/eo9-integration` + `crates/eo9/tests` | green; QEMU tier still manual/scripted, not in ci |
| Usermode binary `eo9` (run/store/describe/compile/cache/shell, `--disk` + `mkfs.eofs`, layered session, stderr outcomes, --max-fuel, positional/variadic args, seeding + auto-reseed, wiring) | `crates/eo9` | done for current scope; crates.io publish prep complete (8-crate sequence incl. `eofs-core` and the `eo9-components` bundle) |
| Embeddable runtime (`Eo9` builder, Sandbox + Host backends) | `crates/eo9-embed` | complete; foundation for `eo9 bundle` and the wasm32 backend |
| Website + server + the auto-booting try-it shell (wasm32+Pulley blob with the in-blob compiler, only-narrowing, wiring, panic messages; two-page site; compression, security headers, fingerprinted immutable caching; www in the CI gate) | `www/` | deployable; live-site redeploy + click-through awaits the owner |
| Bare-metal kernel: **aarch64, riscv64, x86_64 at parity** (boot-to-eosh from the 20-entry store, on-target Cranelift codegen from W^X pages, fuel/preemption, Ctrl-C, kill-cascade, caps, event-driven ~0% idle, nested eosh); opt-in `eo9:pci` (aarch64) with the virtio-blk/net wasm drivers; vendored CM-async + compile-layer + algebra + cranelift-codegen no_std forks | `kernel/` | breadth complete for QEMU; real-board bring-up unscheduled; riscv64/x86_64 PCI providers + MSI/INTx + QEMU test tier queued |

## In progress right now

- Nothing on area branches — the drivers/networking/persistence/three-architecture wave is fully merged.

## Next up (rough order)

1. **Real-board bring-up (aarch64)** when the owner has hardware — the QEMU breadth work is done.
2. **WIT round-out batch**: `eo9:disk` `size`/`flush` ops (FLUSH-on-commit durability for `disk.virtio`),
   the `l4-over-l2-config` address-override interface, surfacing `ProviderExportsUnused` ("exports match
   nothing"), and a friendlier shell-side missing-`--disk` refusal.
3. **Driver/track follow-ups**: store-on-eofs (the artifact cache on a real disk), MSI/INTx interrupt
   delivery, a riscv64 PCI provider so the drivers run there too, `pci.deny`/`pci.filtered` stubs, DHCP /
   IPv6 / an l3 export / deeper listen-accept coverage for the TCP/IP middleware.
4. **Publishing**: the owner runs the prepared 8-crate `cargo publish` sequence (then a README install
   section); the `eo9 bundle` milestone on eo9-embed; `eo9 new` scaffold.
5. **Web polish**: live-site redeploy + click-through; blob lazy-fetch trim; COEP/Permissions-Policy
   headers; full cross-checkout blob reproducibility.
6. **Kernel residuals**: kernel-image-internal W^X + guard regions; the headless `program=` runner should
   carry panic messages and honor `program=eosh`; kernel-side full wiring trees; QEMU test tier in ci.
7. **Upstreaming**: the three staged branches remain on ice until the owner reviews/pushes them.

See `GAPS.md` for known limitations and the user-study triage.
