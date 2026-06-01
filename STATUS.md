# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design),
`docs/user-studies/` (external-perspective findings and their triage).

_Last updated: 2026-06-01, master at acb6c5e. Headline: **round 3 of the virtual user studies (five
personas: storage, network, driver, returning novice, distribution) produced 96 findings, and the 37
fix-now items are already fixed and merged — including three real data-loss bugs, a kernel memory-safety
hole, a contract violation, and a release blocker that internal testing had missed. Alongside the studies,
the bind entrypoint landed (resource-owning providers are configurable; configure refusals are typed
errors, never traps), PCI drivers got real interrupt delivery, and the metal shell got a writable
MAC-verified `/bin` (`save` a composition, power-cycle, it's still there).**_

## Round 3 user studies and the fix wave (2026-05-30 → 06-01)

The wave began with four feature merges, then five user studies, then six fix merges driven by what the
studies found.

**The feature merges that set the stage:**
- **The bind entrypoint** (`eebc10e`, hardened by `c1bd95c`): `configure` is now realized as direct API
  aliases + one `eo9:rt/configured.bind` export that executors call after instantiation. Resource-owning
  providers are configurable — `pci.filtered --allow "[{segment: 0, bus: 0, device: 1, function: 0}]" $
  lspci` shows exactly one device on metal, compiled on-target — and the forwarding binder (with its whole
  caveat list) is deleted. `bind` returns `result<_, string>`, so an invalid configuration is a typed
  pre-run refusal ("compose-time configuration refused: …") on every target, never a trap. In SPEC.
- **PCI INTx interrupt delivery** (`9d048e5`): the gpex lines route through the GIC (aarch64) and PLIC
  (riscv64) to drivers; `disk.virtio` now halts the CPU in WFI during device waits instead of polling
  (verified by per-boot diagnostics on both architectures). x86_64 keeps the polled fallback.
- **Writable /bin on metal** (`00d1eb2`): `save <name> = <expr>` at the metal prompt persists the program
  to the store disk under the same keyed-MAC discipline as the compile cache (namespaced tags — a reviewer
  attack that copied one entry over another's path through eofs's own write machinery was rejected); baked
  programs can't be shadowed or deleted; everything survives power cycles.
- **The tail batch** (`89eebb7`): TCP listen/accept depth (which found and fixed two middleware bugs),
  typed configure refusals, and hash-named immutable vm.js/vm.css.

**The studies (sessions 07–11, `docs/user-studies/`):** five context-free personas, all demos real
(release builds, QEMU on up to three architectures, packet captures, the served site). Verdicts ranged
from "It wasn't me. They made it make sense" (the returning novice who bounced off round 1) to "most
'capability OS' papers never get this far" (the driver developer). Full triage in
`docs/user-studies/00-synthesis.md`.

**The fix merges the studies drove:**
- **eofs data integrity** (`cb410c2`): rewrites are atomic (a failed write can no longer destroy the old
  content); CoW garbage is reclaimed (images no longer brick — `rm` frees space); the silent
  transaction-rollback on uberblock corruption now warns loudly; foreign (non-eofs) images are refused
  instead of silently reformatted; `--disk` images are flock'd; concurrent `eo9 -c` invocations no longer
  corrupt each other; integrity failures are distinguishable from I/O errors; README gained a verified
  persistence section.
- **PCI device quiesce** (`02460f9`): a driver task's devices are disarmed (bus-master cleared, interrupts
  masked) before any of its DMA buffers are freed, on every teardown path — closing a real memory-safety
  hole (devices DMA-ing into freed kernel heap). Driver error text now survives the disk→fs boundary.
- **Net stack fixes** (`9b2bc10`): the ~6.7 s ARP stall is gone (wire-visible replies are picked up in
  milliseconds); a TCP RST is now reported as `connection-refused` (matching the loopback mock — mock and
  metal agree on error semantics); the per-layer net stubs + sockcheck are baked into the kernel store so
  the typed-denial-on-metal demos actually run.
- **Shell/website teaching fixes** (`9fee776`): every example in `help` is machine-verified by a test (the
  broken `&` example can't recur); the `&` teaching error survives applied arguments; `let` confirms what
  it bound; `eo9:rt/*` imports are annotated as no-authority; the page terminal renders stderr styled
  (no more `[stderr]` noise); welcome.txt only recommends programs that exist.
- **Release tooling** (`be91552`): `eo9 --version` (which never falls through to program resolution); real
  package sizes in the publish pre-flight; `make setup` failures propagate; crates.io metadata + MSRV 1.94
  on all 8 publishable crates; the `pci.deny` stub now exists; guest builds are checkout-independent
  (--remap-path-prefix) for 50 of 51 components.
- **The bind error channel** (`c1bd95c`, counted above): study 08's "configure traps" finding fixed at the
  root.

**Found by the wave's own reviews (the process working on itself):** a session-lock ordering race in the
per-process-sessions fix (fix in flight), and the fs-eofs checkout-dependence that keeps the bundle drift
check out of `cargo xtask ci` for now (plan/01 D15).

## The polish / persistence / configure-baking wave (2026-05-29 → 30)

- **The try-it page behaves like a terminal** (`72368b4`, `fbc5f38`): you type straight into the terminal
  (live line + cursor on the `eosh>` prompt line itself, paste works, no separate input box, no stray
  prompt glyphs); the terminal breaks out of the prose column (~109 columns, so `help` doesn't wrap); the
  examples use bare defaults and the copy reads "Run a program" with a `describe hello` / `hello --name
  user` pairing.
- **`hello` takes optional arguments everywhere** (`72368b4`): `name`/`excited` are `option<…>` with
  in-program defaults, so bare `hello` works at the browser, usermode, `-c`, and metal prompts; the CLI
  binder wraps option-destined values as `some(…)` and binds unsupplied options to `none`; the kernel demo
  and headless runner parse option-typed arguments too.
- **The shell teaches itself** (`b49e952`): `help` explains the operators (`$`, `&`, `only`, configure)
  each with a one-line example plus an explore-the-sandbox block (`ls /bin`, `describe`, `imports`, `env`);
  every target's banner says "type `help` to explore"; `describe` of a provider shows its configure
  arguments; and **`env` works in the browser** — the blob seeds a real session manifest (`/session`)
  describing the page's actual grants.
- **Accurate `&` refusals** (`bdbb3e1`): `entropy.seeded & echo` now says ``  `echo` is a program, not a
  provider`` and suggests `entropy.seeded $ echo` (it used to blame the wrong operand); both operand
  orders, both-binaries, and configured-operand cases are covered by tests.
- **Purge-free caching** (`6312996`, plan/15 D28): the `/vm` blob and store keep `immutable, max-age=1y`
  (hash-named), but every mutable URL (HTML, vm.js, vm.css, assets.json) is now `no-cache` with strong
  ETags — a deploy propagates immediately with cheap 304 revalidation; **no Cloudflare purge is ever
  needed** (the old `max-age=3600` on .js/.css was exactly the purge-to-deploy bug).
- **PCI reach** (`1ab0217`): the `eo9:pci` root provider works on **riscv64** (per-arch PCI map; ECAM +
  BAR-window gigapage under Sv39) — `lspci` and the full `disk.virtio $ fs.eofs` storage stack run on a
  riscv64 virtio disk; **`pci.filtered`** ships (allow-listed PCI attenuator; unconfigured = deny-all,
  verified on metal); the INTx/GIC-PLIC interrupt-delivery design is recorded (plan/12 D57) for a dedicated
  pass that converts virtio-blk's poll as its proof.
- **`eo9:disk` round-out** (`db896eb`, SPEC `eb7e74a`): the disk API gains `size` and async `flush`;
  disk.mem (no-op), disk.virtio (real `VIRTIO_BLK_F_FLUSH`), and the `--disk` file device (fsync) implement
  them; `fs.eofs` uses `size` (no more probe gallop) and flushes at every commit boundary — durability
  verified across a metal power cycle. A spawn that fails for want of the block device now names `--disk`
  and `mkfs.eofs`. The `l4-over-l2-config` addressing interface exists in WIT but cannot be baked yet (see
  the D21 decision below).
- **A persistent compile cache on metal** (`aaf7d27`, plan/12 D58): boot aarch64 with the `storedisk`
  grant and on-target compile results are cached to an eofs filesystem on a dedicated virtio disk —
  **~1.4 s of Cranelift becomes ~2 ms across a full power cycle**. Every disk-loaded artifact is verified
  against a keyed blake3 tag (key baked into the kernel image, 0600 on disk, never committed) **before**
  the unsafe deserialize; the tag also covers the cache key so entries cannot be swapped; tampered bytes
  are refused and recompiled, never executed (byte-flip test reproduced by the reviewer). Without the
  grant, behavior is byte-for-byte today's.
- **Compound configure arguments bake** (`ca255c8`, plan/03 D20): `configure` now accepts records, tuples,
  `option<…>`, and `list<…>` (nested), laid out in canonical-ABI form in a constant arena at compose time;
  >16-flat-param signatures spill to a parameter record instead of being rejected. Existing
  scalar/string/enum configurations are **bit-for-bit unchanged** (hash-verified old vs new). What still
  refuses: variants/results/flags/handles, and **any config interface on a resource-owning provider** —
  that needs the plan/03 D21 `bind`-entrypoint design (owner decision pending), which is what keeps
  `pci.filtered --allow […]` and `l4-over-l2-config` unusable for now.
- **Web assets current** (`b5d4ede`): the blob and store were rebuilt for the changed components (the `&`
  wording, the disk/net stubs); blob `web-eo9.563c4cb367d769bf.wasm`, 8.85 MiB raw / 1.73 MiB brotli.
- **Kernel store is 21 entries** (pci.filtered joined); the **`eo9-components` bundle is 50** components.

## The drivers / networking / persistence / three-architecture wave (2026-05-28 → 29)

- **Layered networking** (`04c0eae`, SPEC `a6a4275`): `eo9:net` is three independent capabilities — `l2`
  (frames/MACs), `l3` (IP/routes/raw datagrams), `l4` (TCP/UDP sockets) — each with its own root handle,
  `.none`/`.deny` stubs per layer, and `net.l4.loopback`, an in-memory transport for tests.
- **Real wasm device drivers over `eo9:pci`**: the opt-in `eo9:pci` root provider (`pci` boot token) plus
  `lspci` (`fe6a143`); **`disk.virtio`** — virtio-blk exporting `eo9:disk` (`cce3036`) — and
  **`net.virtio`** — virtio-net exporting `eo9:net/l2` (`59c0db2`). DMA only via `alloc-dma`; each device
  needs a second explicit grant (`disk` / `net` QEMU flags).
- **Sockets on metal through three composed wasm layers** (`834f72e`): `net.l4.over-l2` (smoltcp 0.12,
  guest-workspace only) imports l2 and exports l4; `net.virtio $ net.l4.over-l2 $ l4check` resolves a real
  DNS name on metal, compiled on-target, and reports a refused TCP probe as `connection-refused` — the same
  typed error the loopback mock gives. The per-layer deny/none stubs, `net.l4.loopback`, and `sockcheck`
  are in the kernel's baked store (user study 08 F4 — earlier revisions of this claim predated that, and
  the typed-denial demo could not actually be run at the metal prompt), so
  `net.l2.deny $ net.l4.over-l2 $ l4check` → typed denial and `net.l4.loopback $ sockcheck` → `echoed(…)`
  both run on metal.
- **Persistent storage**: eofs M2+M3 — the `fs.eofs` provider (`877738d`), the `--disk <image>` grant +
  `eo9 mkfs.eofs` + cross-process persistence in usermode (`fee878b`), and on metal
  `disk.virtio $ fs.eofs $ …` surviving full power cycles.
- **riscv64 and x86_64 ports complete** (`f651106`→`b6b7403`, `27d4edd`→`6f3c405`): all three architectures
  at functional parity — boot-to-eosh, on-target Cranelift codegen from W^X pages, preemption, Ctrl-C,
  ~0% idle — and all three in the `cargo xtask ci` featureless-kernel gate.
- **Guest panic messages** (`f8dc070`, browser `9047c7f`): the write-once `eo9:rt/diagnostics.report-panic`
  sink carries `panic!` message + location into `trapped(reason)` on every target.
- **eosh via WIT** (`e7f198b`): `describe` shows the composition wiring tree at the prompt; `shell -c`
  exits with the honest 0/1/2/3 contract.
- **Publishing/packaging**: the `eo9-components` bundle is derived strictly from the component list and
  `eofs-core` joined the publish sequence (`b20d3be`) — the `cargo install eo9` chain is 8 crates,
  dry-runs green, awaiting the owner's `cargo publish`.
- **Website**: two pages — the explanatory front page and the auto-booting try-it shell at `/vm`; the in-blob
  Cranelift→Pulley compiler makes the browser VM fully self-hosted (`cbe8fe6`); the `www` workspace is in
  the CI gate.

## Works today (usermode, on master, CI-gated)

- `eo9 run <name-or-path> [--flags]` — real components end to end: WAVE-typed flags checked against the
  program's signature (optional `option<…>` parameters fall back to program defaults — bare `eo9 hello`
  works), three-way outcomes (`success`/`failure`/`abnormal`) with exit codes 0/1/2/3, the outcome line on
  **stderr** by default, store-resolved dotted names or host paths, immutable `open-exec`, memory limits,
  `--max-fuel`, and a compile cache. A first run on an empty store seeds the bundled components (50 in the
  bundle), and seeded bindings auto-refresh on upgrade.
- Filesystem access is opt-in: `--fs-root <dir>` grants a rooted fs capability; without it, fs-requiring
  programs are refused with a clear message.
- **Persistent storage (eofs)**: `eo9 mkfs.eofs <image>` formats a host file; `--disk <image>` grants
  `eo9:disk` over it (opt-in, never ambient); `fs.eofs $ <program>` persists across processes. The disk API
  carries `size` and `flush` (fsync on the host file; real virtio FLUSH on metal), and eofs flushes at
  every commit boundary. A spawn missing the grant names `--disk` and `mkfs.eofs`.
- **Coreutils** (12 guest programs): `cat ls find wc head stat mkdir rm cp touch echo rng` — positional and
  variadic args (`cat a.txt b.txt`, bare `ls`); the basic set is baked into the kernel store too.
- **Networking is layered and mockable**: `eo9:net/l2`/`/l3`/`/l4` with `.none`/`.deny` per layer;
  `net.l4.loopback` for tests; `net.l4.over-l2` (smoltcp) turns any l2 into real sockets — on metal that l2
  is the `net.virtio` driver.
- `eo9 store add|ls|gc|reseed`, `eo9 describe` (+ `--wiring`), `eo9 compile`, `eo9 mkfs.eofs`.
- Deterministic execution proven on real components: seeded/frozen providers compose onto unmodified
  programs; runs are byte-identical and sealed against ambient providers.
- Invoker-side provider configuration via the algebra: `configure` is **synchronous** and never traps when
  unconfigured (documented defaults). **Compound argument values bake**: records, tuples, options, lists
  (nested), and wide parameter lists — with compose-time canonical-ABI layout and bit-identical artifacts
  for all previously-working configurations. Resource-owning providers' config interfaces await the D21
  `bind`-entrypoint decision.
- **Algebra correctness**: drop-law, renamed residuals, configured middleware (bug 1), the generative
  property suite, and the seeded soundness corpus all in place; `≡` / instance identity / `empty` defined
  in SPEC; **`&` refusals name the offending operand** and suggest the right operator.
- **`fs.overlay` + algebraic layering**: guest-leaf layering purely in the algebra.
- **`eo9 shell` / eosh**: tab completion, capability-aware `env`, **a `help` that teaches the operators
  with examples and an explore-the-sandbox loop**, a banner that points at it, `describe` with the wiring
  tree (and configure args for providers), honest `-c` exit codes, the layered session filesystem, and
  recursive `eosh> eosh`.
- **Diagnostics**: a trapped guest reports the `panic!` message + location plus a demangled, address-free
  backtrace — usermode, metal, and browser.
- **Bare metal — three architectures at parity (aarch64, riscv64, x86_64 under QEMU)**: each boots to an
  interactive eosh over serial from a 21-entry baked store, runs host-AOT components, and **compiles
  compositions on-target from W^X pages** (same digests and seeded-entropy values on all three). Child
  fuel + preemption, Ctrl-C, kill-cascade, per-child caps, ~0% idle, nested eosh, clean self-power-off.
  The opt-in `eo9:pci` provider runs on **aarch64 and riscv64**: `lspci`, `pci.filtered $ lspci`
  (deny-all attenuation), `disk.virtio $ fs.eofs $ …` (data persists across power cycles), and
  `net.virtio $ net.l4.over-l2 $ l4check` (real DNS through slirp, aarch64). With the **`storedisk`**
  grant (aarch64), on-target compile results persist to a MAC-verified eofs disk cache across reboots.
- **The website (`www/`)**: a two-page static site + standalone Rust server with ACME TLS, security
  headers, pre-compression negotiation, and **purge-free caching** (immutable hash-named files; `no-cache`
  + ETag on everything mutable). The try-it page **auto-boots the real eosh shell** (~8.85 MiB raw /
  ~1.73 MiB brotli blob): you type straight into the full-width terminal, 16+ programs run, the in-blob
  Cranelift→Pulley compiler does client-side composition, `only` genuinely narrows, `env`/`describe`/
  `help` work for sandbox exploration, and guest panics carry messages. `check-web-vm` guards asset drift;
  the `www` workspace is in the CI gate.
- **README.md** — examples verified against the build.
- `cargo xtask ci` — one gate over the host, guest, kernel (all three bare-metal targets), and www
  workspaces.
- **Six user studies** with a cross-session triage in `docs/user-studies/00-synthesis.md`.
- **Upstreaming**: three locally staged contribution branches on ice awaiting owner review/push.

## In the browser today

The try-it page (`/vm`) auto-boots the **real stack**: `eosh>` comes up with no clicks, you type straight
into the terminal, 16+ programs run, the real algebra does `load`/`describe`/`compose`, and execution is
genuine wasmtime+Pulley with fuel and entropy matching native byte-for-byte.

- **Composition compiles in the blob** (Cranelift→Pulley, ~50–110 ms, no server).
- **The sandbox is explorable**: `help` teaches the operators, `ls /bin` lists what's installed, `describe`
  shows imports and configure args, `env` reports the page's real grants from the seeded session manifest.
- **`only` genuinely narrows**; `describe` shows the wiring tree; guest panics carry their message.
- **Honest caveats**: programs are Pulley-interpreted (slower than native/metal for compute-heavy work);
  the blob hash is path-dependent across checkout directories (~410-byte residue); every guest-SDK change
  re-fingerprints all `/vm` assets; a click-through on the live deployed site awaits the owner's next
  redeploy.

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages; layered `eo9:net` l2/l3/l4 + `l4-over-l2-config`; `eo9:pci`; `eo9:rt/diagnostics`; sync `configure`; `eo9:disk` with `size`/`flush`) | `wit/` | v0 complete; message/perf are placeholders; `l4-over-l2-config` exists but baking it awaits the D21 decision |
| Component algebra: `$`, `&`, `only`, `rename`, `configure` (incl. compound argument baking), describe/load/save (+ wiring) | `crates/eo9-component` | complete incl. law tests, soundness corpus, generative property suite, compound-config suite; resource-owning configure awaits D21 |
| Runtime: fuel-metered resumable tasks, WAVE args/outcomes (incl. `option` defaults), caps, fs/io/disk + text/time/entropy linking, exec provider (incl. `wiring`), diagnostics sink, image serialization | `crates/eo9-runtime` | usermode-complete for current scope |
| Scheduler (no_std, conserved fuel, deterministic policy) | `crates/eo9-sched` | complete for single-core; not adopted by the CLI/kernel loop |
| Module store + compile cache (content-addressed, blake3-verified) | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk incl. the file-backed `--disk` device with fsync flush) | `crates/eo9-providers-unix` | complete; host net root deferred |
| eofs (CoW/Merkle engine, lz4, snapshots; `fs.eofs` provider; `mkfs.eofs`; commit-boundary flush) | `crates/eofs-core` + `guest/stubs/fs-eofs` | engine + provider + mkfs + durability done; persistence verified in usermode, on metal, and as the metal compile cache |
| Guest SDK + stub/driver components: none/deny families, seeded, memfs, frozen/fuzzy clocks, readonly, fs.overlay, the layered net stubs + loopback, `fs.eofs`, `disk.virtio`, `net.virtio`, `net.l4.over-l2`, `pci.filtered` | `guest/` | complete for current WIT; `text.capture` deferred; guest wit-bindgen still a git pin |
| Coreutils (12 tools, positional/variadic args) | `guest/coreutils/` | complete; run in usermode, the browser, and the metal store |
| eosh (full grammar, evaluator, teaching `help`, env/envinfo, describe-with-wiring + configure args, accurate operand errors, program-failure classes) | `guest/eosh` | done for current scope; runs as `eo9 shell`, recursively, on all three metal arches, and in the browser |
| Integration suites (capability laws, determinism, configured env incl. compound, overlay, soundness corpus, property suite, interposition, net loopback/deny, eofs persistence, CLI transcripts) | `tests/eo9-integration` + `crates/eo9/tests` | green; QEMU tier still manual/scripted, not in ci |
| Usermode binary `eo9` (run/store/describe/compile/cache/shell, `--disk` + `mkfs.eofs`, layered session, positional/variadic + optional args, seeding + auto-reseed) | `crates/eo9` | done for current scope; crates.io publish prep complete (8-crate sequence) |
| Embeddable runtime (`Eo9` builder, Sandbox + Host backends) | `crates/eo9-embed` | complete |
| Website + server + the auto-booting try-it terminal (type-in-terminal, full-width, explore-the-sandbox examples, in-blob compiler, browser `env`; purge-free caching; www in the CI gate) | `www/` | deployable; live-site redeploy + click-through awaits the owner |
| Bare-metal kernel: **aarch64, riscv64, x86_64 at parity** (boot-to-eosh from the 32-entry store, on-target codegen from W^X pages, preemption, Ctrl-C, ~0% idle); `eo9:pci` on aarch64 + riscv64 with the virtio wasm drivers, INTx interrupt delivery, device quiesce, `pci.filtered`/`pci.deny`/`pci.none`; the `storedisk` MAC-verified persistent compile cache + writable `/bin` (`save` survives power cycles); vendored no_std forks | `kernel/` | breadth complete for QEMU; real-board bring-up unscheduled; x86_64 PCI grant + MSI/MSI-X + net.virtio interrupt conversion + QEMU test tier queued |

## In progress right now

- **The session-lock race fix**: the per-process-sessions change (eofs-integrity wave) creates the lock
  file non-atomically, so a concurrent sweep can delete a live session dir (~50% flake under load; found
  by the dist-fixes reviewer). Fix: create+lock under a temp name, then rename into place.

## Next up (rough order)

1. **The round-3 owner decisions** (the full list with evidence is at the top of `GAPS.md`): storage
   policy (rollback / auto-format / uberblock geometry / `rename` / operator threat model), networking
   design (NIC sharing, l4 deadlines, error catch-alls), driver model (IOMMU, vendor:device filtering,
   long-running drivers), website voice (authoring statement, jargon, banner, browser `save`), and
   release process (hosted CI, publish automation/rehearsal, crate naming, guest-SDK publishing).
2. **The round-3 tracked queue**, highest-leverage first: the nested-forwarding bug (the flagship
   `pci.filtered $ disk.virtio $ fs.eofs` attenuation pattern fails at first I/O), the fused-composition
   compile-cache miss (blocks all metal networking/driver iteration), the fsck/scrub/df storage surface,
   net.virtio's interrupt conversion, and the "tools must tell the truth" env/refusal pass.
3. **Real-board bring-up (aarch64)** when the owner has hardware.
4. **Publishing**: the owner runs the prepared 8-crate `cargo publish` sequence (then a README install
   section); `eo9 bundle`; `eo9 new` scaffold. The publish pre-flight is green; hosted CI and the
   local-registry rehearsal are owner decisions above.
5. **Upstreaming**: the three staged branches remain on ice until the owner reviews/pushes them.

See `GAPS.md` for known limitations and the user-study triage.
