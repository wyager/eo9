# Eo9 Implementation Status

Maintained by the planner; refreshed when merges land. Companion docs: `PLAN.md` (how work is organized),
`plan/*.md` (per-area briefs + decisions), `GAPS.md` (known gaps and deferred items), `SPEC.md` (the design),
`docs/user-studies/` (external-perspective findings and their triage).

_Last updated: 2026-06-02, master at a1f0c10. Headline: **three waves landed back-to-back. The executor
model is complete (services, `init`, restart policies as programs, and a virtual-NIC switch — every machine
now boots kernel → init → console on all three architectures). Eo9 has video (`eo9:gfx`, a 146 KB wasm
virtio-gpu driver over the existing PCI provider, pixel-exact screendump verification, and `make gfx` opens
a real framebuffer window). And the async-first rearchitecture is done: every I/O middleware genuinely
awaits, the suspension wall is down — the study-09 flagship `pci.filtered $ disk.virtio $ fs.eofs` chain and
per-port L4 stacks over the switch both run on metal — with an off-by-default inline-first-poll runtime
prototype showing 28–38% faster eager chains.**_

## The executor, video, and async-first waves (2026-06-01 → 06-02)

**Owner decisions from round 3, implemented:**
- **Blank-image probe + crate renames** (`d45c872`): "blank" now means all-zero across the leading 1 MiB,
  the trailing 64 KiB, and whole small devices (the old 64 KiB span ended exactly at btrfs's superblock);
  foreign volumes are refused byte-untouched. `eofs-core` → **`eo9-eofs`** and `eo9-components` →
  **`eo9-bundled-programs`** before first publish (the one-letter `eo9-component` collision is gone).
- **Session-lock race** (`db2f756`): session locks are created+locked under a temp name then renamed
  (visible ⇒ locked), and sweeps/establishment exclude each other — the ~50% under-load flake is gone
  (12/12 loop runs under CPU saturation).
- **IOMMU spike** (`3f4f882`, docs/spikes/iommu.md): the kernel boots untouched under
  `-M virt,iommu=smmuv3` (SMMU defaults to bypass — **no flag day**); a minimal stage-2 SMMUv3 driver is
  ~800–1,200 lines in four independently-verifiable steps; virtio-iommu rejected (no real silicon has it).
  Build it as part of real-board prep.
- **Security policy in SPEC** (`d01f559`, `69f09fd`, `762ac0e` + the audit `a7d9d4b`): hardware mitigations
  are insurance, never load-bearing — Eo9 is secure on MMU-less hardware if the compiler is correct (and
  the kernel's wasmtime config *is* already the no-MMU configuration; the MMU is used only for W^X).
  Cache flushing is a **granted capability** (it externalizes cost). The Spectre audit found bounds-check
  masking ON in usermode but **forced OFF on metal** (wasmtime couples it to signals-based traps), so on
  metal the timer capability is the *only* Spectre mitigation — programs granted real time there are
  trusted w.r.t. side channels.

**Policies are programs** (`fdfa244` SPEC, `532ad8b` design, `61dd29a` swaps): where an API takes a
non-trivial *decision*, it takes a pure component, not a config enum — purity enforced by the capability
system (a policy provably imports nothing). Shipped: `pci.filtered` takes a composed `admit(device-info)`
policy (`pci.admit-address` recovers old behavior; `pci.admit-vendor` filters by ID, fixing study 09's
address fragility); **`fs.filtered`** (per-path allow/deny/read-only verdicts, traversal-defended at both
layers); **`net.l4.filtered`** (a composable transport firewall); and the svc **restart policies** below.

**The executor model — v1, v2, v3 all landed:**
- **v1, usermode** (`60eeac7`, SPEC `bf570c4`): the `eo9:svc` capability — `detach` hands a composed child
  to the service registry and walks away ("outliving your creator is authority": never in default grants);
  a detached child runs with **exactly what its detacher composed** plus log capture (capability soundness,
  adversarially probed); restart policies are pure components (`restart.never`/`always`/`backoff`), and a
  policy that traps or stalls reads as give-up, never a hang; `svc list/log/stop` + `detach name = expr`
  in eosh; `init` runs a service config then its console; registry lifetime is root-process config.
- **v2, metal** (`bbd1823`): the kernel service registry; **boot is now kernel → init → console on all
  three architectures** (default config preserves today's UX exactly); the svc grant is a generation count
  (init=2, console=1, children=0 — no raise path); `poweroff` is a typed intent recognized only from
  init's console child (a service cannot power off the machine — probed); console `exit` restarts the
  console while services keep running; Ctrl-C kills the foreground job, never the console or services.
- **v3, the virtual-NIC switch** (`50c47cc`): `net.l2.switch` — single-owner NICs realized: one upstream
  l2, two named ports with **per-port nominal types** (the first multi-export provider), per-port
  locally-administered MACs, source-rewrite on egress (no spoofing), unknown-unicast dropped never flooded.
  Metal demo pcap-verified: two virtual MACs ARPing through one physical NIC, the real MAC never appearing
  as a source.

**Web/shell** (`927439d`): the prompt-accumulation regression root-caused (line-buffering held the
newline-less prompt; **reading flushes** now) with a new `verify-render` harness that drives the real
`vm.js` DOM pipeline — the layer every other harness bypassed; **`describe` works on the shell's own
builtins and operators, including itself** (`describe describe`, `describe $`), with build-enforced
coverage; the blob registers `eo9:svc` as absent so the svc-aware eosh boots in the browser.

**Video** (`0b64ccf` + `make gfx` in `2e4aefd`): the **`eo9:gfx`** capability — deliberately a framebuffer,
not a GPU (mode/present/read with the owned-buffer round-trip, damage rects, xrgb8888) so a dumb U-Boot
simple-framebuffer can implement it on real boards; `gfx.mem`/`none`/`deny` stubs + the deterministic
`draw` demo; **`gpu.virtio`** — a 146 KB wasm virtio-gpu driver over the existing `eo9:pci` provider
(**zero kernel changes**, double-opt-in, allocate-once DMA); `cargo xtask check-gpu` verifies rendering
**pixel-for-pixel** via QMP screendump; one checksum across usermode RAM, metal RAM, and real scanout;
**`make gfx`** boots QEMU with a framebuffer window + serial in the terminal.

**The async-first rearchitecture (the big one):**
- **The investigation** (`21d4b7f`): the eager-guest suspension root-caused at the canonical-ABI level
  (async-lowered guest calls are *queued, never executed inline* — the callee hasn't run when the caller
  sees `STARTED`), proven by a 7-test hand-written ABI matrix; sync-lifting async-WIT functions is legal
  (validator-cited).
- **The doctrine** (SPEC `06c30e4`, owner ruling): *boundaries are honestly async* — everything that can
  wait is declared and bound async; nothing is sync because it "happens to work"; eagerness (sync
  lift/lower) is a measured, runtime-enforced optimization; **awaits are bounded** (an unbounded await
  across a trust boundary is a liveness bug).
- **The net chain** (`33c3ba6`): `net.virtio` + `net.l4.over-l2` genuinely await (take-out-of-slot guards;
  no borrow crosses an await); the un-ignored acceptance passes and the metal payoff runs: **one NIC →
  the switch → two l4 stacks → both ports answering real DNS** (61 bytes each). Op-phase timing
  1.10s → 1.09s — awaiting costs nothing measurable.
- **The storage chain** (`35c0dc9`): `eo9-eofs` is now **one async-core engine with a sync facade**
  (`Pending` unreachable by construction over sync devices — all 70 awaits audited; the kernel storedisk
  cache and `mkfs` kept their sync API with zero source changes); `fs.eofs`/`disk.virtio`/`pci.filtered`
  await; drop-guards make cancellation safe. **The study-09 flagship runs on metal**:
  `pci.admit-address … $ pci.filtered $ disk.virtio $ fs.eofs $ ls` — with **INTx interrupt pacing
  surviving interposition** (the CPU halts through the filter where the eager driver always degraded to
  polling) and power-cycle persistence through the filtered chain. Usermode A/B: the awaited path measured
  *faster* (4.0s → 3.3s).
- **The hardening matrix** (`a7bd14b`): 21 tests over hand-written genuinely-awaiting ABI fixtures and a
  controllable host clock — **the long-standing GAPS cancellation caveat is refuted**: host kill
  mid-forwarded-park leaks nothing at any depth, `subtask.cancel` cascades cleanly; the only traps are
  pinned ABI contract violations. Plus the `first-poll-inline` design note.
- **First-poll-inline prototype** (`4a846f4`): the vendored runtime can now run a queued guest callee
  **inline on the caller's stack**, falling back to the queue at first genuine suspension (suspension is a
  return value under the callback ABI — no stack capture). Off-by-default feature; the entire hardening
  matrix is outcome-identical with it on; **eager chains −28…38% per spawn+run**; default-on awaits
  quiet-machine + metal numbers and an `xtask` A/B gate.

**Liveness/correctness fixes the waves flushed out:**
- **The usermode lost wakeup** (`bd67f89`): `task.wait` discarded a `Ready` edge (a completion landing in
  the check→register gap rang a doorbell with an empty waiter list); wild hung specimens sampled, the
  window amplified to 40/40, fixed in 3 lines, 320/320 clean under load.
- **The virtio cancellation drains** (`ca6d0c5`): a cancelled in-flight request could have its completion
  misattributed to the next request *and* leave the device DMA-ing shared buffers (torn state; same class
  on net tx). Drain-before-reuse invariant: when an op begins writing device-shared state, the device has
  posted completions for everything previously published. Self-healing under cancelled drains.
- **The gpu "freeze"** (`5fa53e8`): root cause was a **silent on-target compile** blocking the drive loop
  (no kernel compile cache without storedisk; 4s quiet, 15–60s under host load — not a freeze at all).
  Now: a per-session compile cache (repeat draws **4.5s → ~0.3s**), `codegen: compiling …` announcements
  on every compile path, an idle-waker drain-all (a standing console-deafness hazard removed), and a gpu
  ISR ack fix.

## Round 3 user studies and the fix wave (2026-05-30 → 06-01)

Five context-free personas (storage, network, driver, returning novice, distribution) produced 96 findings;
the 37 fix-now items were fixed and merged — including three real data-loss bugs, a kernel memory-safety
hole (device quiesce before DMA free), a contract violation, and a release blocker internal testing missed.
Highlights that remain the foundation of the current tree: the bind entrypoint (resource-owning providers
configurable; configure refusals typed, never traps), PCI INTx interrupt delivery, the writable MAC-verified
`/bin` on metal (`save` survives power cycles), eofs data-integrity hardening (atomic rewrites, gc,
foreign-image refusal, locking), `eo9 --version` + crates.io metadata + MSRV 1.94, machine-verified help
examples, and purge-free site caching. Verdicts: "It wasn't me. They made it make sense" (returning
novice); "most 'capability OS' papers never get this far" (driver developer). Full triage:
`docs/user-studies/00-synthesis.md`.

## Works today (usermode, on master, CI-gated)

- `eo9 run <name-or-path> [--flags]` — real components end to end: WAVE-typed flags checked against the
  program's signature (optional `option<…>` parameters fall back to program defaults — bare `eo9 hello`
  works), three-way outcomes with exit codes 0/1/2/3, store-resolved dotted names or host paths, immutable
  `open-exec`, memory limits, `--max-fuel`, and a compile cache. A first run on an empty store seeds the
  bundled components (70 in the bundle), and seeded bindings auto-refresh on upgrade.
- Filesystem access is opt-in (`--fs-root`); **persistent storage** via `eo9 mkfs.eofs` + `--disk <image>`
  (the eofs engine is async-core with `size`/`flush`, atomic rewrites, gc, foreign-image protection,
  flock'd images); a spawn missing the grant names the flag and `mkfs.eofs`.
- **Services**: `eo9 --svc shell` grants `detach <name> = <expr>` + `svc list/log/stop`; `eo9 init
  <config>` boots a service set + console; restart policies are pure components
  (`restart.never/always/backoff`); a detached child can never exceed its detacher's grants; services die
  with the `eo9` process (root-process config).
- **Coreutils** (12 tools, positional/variadic args) — usermode, browser, and the metal store.
- **Networking layered and mockable**: `eo9:net/l2`/`l3`/`l4` with per-layer stubs; `net.l4.loopback` for
  tests; `net.l4.over-l2` (smoltcp, genuinely awaiting) turns any l2 into sockets; **`net.l2.switch`**
  shares one NIC among isolated virtual MACs; **`net.l4.filtered`** firewalls by composed policy.
- **Attenuation by policy components**: `pci.admit-address`/`pci.admit-vendor $ pci.filtered`,
  `fs.policy-subtree $ fs.filtered`, `net.policy-ports $ net.l4.filtered` — pure decision components,
  fused in, visible in the wiring tree.
- `eo9 store add|ls|gc|reseed`, `eo9 describe` (+ `--wiring`), `eo9 compile`, `eo9 mkfs.eofs`,
  `eo9 --version`.
- Deterministic execution proven on real components; sealed against ambient providers.
- `configure` is synchronous, never traps, bakes compound values (records/tuples/options/lists), and
  works on resource-owning providers via the bind entrypoint; refusals are typed pre-run errors.
- **Algebra correctness**: drop-law, renamed residuals, configured middleware, the generative property
  suite, the soundness corpus, the canonical-ABI eager-guest matrix, and the async hardening matrix
  (cancellation/fan-out/trap semantics pinned); `≡`/identity/`empty` in SPEC.
- **eosh**: tab completion, capability-aware `env`, a teaching `help` with machine-verified examples,
  `describe` on programs *and on the shell's own builtins/operators* (incl. itself), the wiring tree,
  honest `-c` exit codes, `detach`/`svc` builtins, `poweroff` as a typed intent, recursive eosh.
- **Diagnostics**: trapped guests report `panic!` message + location everywhere; codegen announces itself
  (`codegen: compiling …` / `compiled in N ms`) so on-target compiles are never silent.
- **Bare metal — three architectures at parity (aarch64, riscv64, x86_64 under QEMU)**: each boots
  **kernel → init → eosh** over serial from a 50-entry baked store, runs host-AOT components, compiles
  compositions on-target from W^X pages (same digests on all three), with a per-session compile cache
  (repeat compositions ~0.3s), child fuel + preemption, Ctrl-C, kill-cascade, services surviving console
  exits, ~0% idle, and clean `poweroff`. The opt-in `eo9:pci` provider (aarch64 + riscv64) runs the wasm
  drivers: `disk.virtio` (INTx-paced, awaited, persists across power cycles — including through
  `pci.filtered`), `net.virtio` + the full layered net stack (real DNS through three composed wasm layers;
  two virtual MACs through the switch), and `gpu.virtio` (screendump-verified pixels). With `storedisk`
  (aarch64): the MAC-verified persistent compile cache + writable `/bin`.
- **The website (`www/`)**: two pages; the try-it terminal auto-boots the real stack (~9.0 MiB raw /
  ~1.78 MiB brotli blob), type-in-terminal with a DOM-level rendering harness, in-blob Cranelift→Pulley
  composition, `only` narrowing, explore-the-sandbox (`help`/`describe`/`env`), purge-free caching; the
  `www` workspace is in the CI gate.
- `cargo xtask ci` — one gate over host, guest, kernel (all three bare-metal targets), and www workspaces.
- **Eleven user studies** across three rounds with full triage in `docs/user-studies/`.
- **Upstreaming**: three staged contribution branches on ice awaiting owner review/push; the first-poll
  inline optimization is a fourth upstream candidate (wasmtime's own comments invite it).

## In the browser today

The try-it page (`/vm`) auto-boots the real stack: type straight into the terminal, 16+ programs run, the
real algebra composes, the in-blob compiler does client-side `$`/`&`, fuel and entropy match native
byte-for-byte. `describe` explains the shell's own builtins; `env` reports the page's real grants;
`svc`/`detach` refuse gracefully (services are not granted in the browser). Honest caveats: Pulley-
interpreted execution speed; the blob hash is path-dependent across checkouts; gfx/net-switch families are
not yet in the browser `/bin`; a live-site click-through awaits the owner's next redeploy.

## Implemented (libraries / components on master)

| Piece | Where | State |
|---|---|---|
| WIT interfaces (all `eo9:*` packages: layered net + policies, pci, gfx, svc, rt, disk with size/flush, sync configure) | `wit/` | v0 complete; message/perf are placeholders |
| Component algebra: `$`, `&`, `only`, `rename`, `configure` (compound baking, bind entrypoint), describe/load/save (+ wiring) | `crates/eo9-component` | complete incl. law/property/soundness/eager-guest suites |
| Runtime: fuel-metered tasks, WAVE args/outcomes, fs/io/disk/text/time/entropy linking, exec (+wiring), the svc registry, diagnostics, bind calls | `crates/eo9-runtime` | usermode-complete for current scope |
| eofs (async-core CoW/Merkle engine + sync facade; fs.eofs provider; mkfs; durability) | `crates/eo9-eofs` + `guest/stubs/fs-eofs` | complete; persistence verified usermode/metal/cache |
| Module store + compile cache | `crates/eo9-store` | complete for usermode |
| Unix root providers (text/time/entropy/fs/disk) | `crates/eo9-providers-unix` | complete; host net root deferred |
| Guest SDK + stubs/drivers: none/deny families, seeded/frozen/fuzzy, memfs, overlay, layered net + loopback + switch + firewall, fs/pci policy attenuators, restart policies, fs.eofs, disk.virtio, net.virtio, gpu.virtio, gfx family | `guest/` | complete for current WIT; gpu.virtio await-conversion in flight |
| Coreutils (12 tools) | `guest/coreutils/` | complete on all targets |
| eosh (grammar, evaluator, teaching help, describe-on-builtins, svc/detach/poweroff, wiring, honest exit codes) | `guest/eosh` | done for current scope; all targets |
| init (service boot, restart policies, console restart, poweroff recognition) | `guest/init` | done; default boot path on all three arches |
| Integration suites (laws, determinism, soundness, properties, eager-guest matrix, async hardening matrix, net/eofs/policy suites, CLI transcripts) | `tests/eo9-integration` + `crates/eo9/tests` | green; QEMU tier still scripted/manual |
| Usermode binary `eo9` (run/store/describe/compile/shell/init/svc, --disk/--svc, mkfs.eofs, --version, seeding) | `crates/eo9` | done; crates.io publish prep complete (8-crate sequence) |
| Embeddable runtime | `crates/eo9-embed` | complete |
| Website + server + try-it terminal (verify-render DOM harness, purge-free caching, www in CI) | `www/` | deployable; live redeploy awaits owner |
| Bare-metal kernel: three arches at parity; boot-runs-init + kernel svc registry; eo9:pci (aarch64+riscv64) with awaited virtio drivers, INTx, quiesce-before-DMA-free; session compile cache + codegen announcements; storedisk MAC-verified cache + writable /bin; gfx via gpu.virtio; vendored no_std forks (+ off-by-default first-poll feature) | `kernel/` | breadth complete for QEMU; real-board prep next; x86_64 PCI grant + MSI/MSI-X queued |

## In progress right now

- **The gpu.virtio async conversion + ISR alignment** (the last eager driver): apply the awaited-import
  doctrine + the plan/09 D34 cancellation-drain pattern + the ISR-ack fix shape to gpu.virtio, and align
  disk.virtio/net.virtio's no-wait ISR acks at the same time.

## Next up (rough order)

1. **First-poll default-on**: quiet-machine + metal A/B numbers, an `xtask firstpoll-ab` gate, and the
   upstream spec conversation about the one semantic deviation (mid-frame-blocking callees).
2. **Real-board prep (Orange Pi 5 Plus ordered)**: GICv3 support, the DesignWare PCIe config-access shim,
   a U-Boot boot recipe, `gfx.simplefb`; the SMMUv3 driver rides with board #2 (the RK3588's PCIe is not
   behind a usable SMMU).
3. **Browser `/bin` additions**: the gfx family, the per-layer net stubs, the switch — so the browser
   sandbox can demo the same compositions as metal.
4. **Publishing** (owner-side): the prepared 8-crate `cargo publish` sequence, then a README install
   section. **Upstreaming** (owner-side): the three staged branches + first-poll as a fourth candidate.
5. **The remaining tracked queue** (see GAPS): Message API, fsck/scrub/df surface, MSI/MSI-X, x86_64 PCI
   grant, the `ProviderExportsUnused` warning surfacing, error-rendering consistency.

See `GAPS.md` for known limitations and the user-study triage.
