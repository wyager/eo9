# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-06-02 (master at a1f0c10, after the executor, video, and async-first waves: the
round-3 owner decisions are ruled and implemented, eo9:svc v1–v3 landed (services, boot-runs-init, the
virtual-NIC switch), eo9:gfx + gpu.virtio shipped video, and the async conversion took the suspension wall
down — both flagship study failures now run on metal. The waves also flushed out and fixed a usermode lost
wakeup, a virtio cancellation-misattribution window, and the silent-on-target-compile "freeze".)_

## Decisions pending with the owner

Most round-3 questions were ruled 2026-06-01/02 (recorded under settled directions below). Still genuinely
open:

**Storage (study 07):**
- **Uberblock geometry** (S7-9): 2 adjacent slots in the first 8 KiB — one torn 8 KiB write kills the
  volume. Format change; decide before any format-stability promise.
- **`rename`/atomic-replace in `eo9:fs`** (S7-12): "on a CoW filesystem, atomic replace should be the easy
  path." WIT addition.

**Networking (study 08):**
- **Wait policy / deadlines in the l4 WIT** (F8): the SPEC bounded-await rule and per-op provider deadlines
  now exist (the l4 stack ships recv/connect/send-flush bounds); whether deadlines become *caller-supplied*
  in the WIT is still the open call.
- **`io(string)` catch-alls** (F15): keep as the pragmatic escape hatch vs enumerate further typed cases.
- **No graceful close in the l4 WIT** (plan/09 D44): dropping a `tcp-connection` queues the FIN but nothing
  pumps it out — `net.text` must use a bounded throwaway `accept` to flush the close handshake after its
  consumer's last operation. Wants `close: async func(tcp-connection)` as a first-class bounded await.
  WIT addition; pairs naturally with the F8 caller-supplied-deadlines call.
- **No authentication on the network shell** (plan/09 D44): `net.text` is cleartext, unauthenticated
  telnet — whoever reaches the port owns the session. Deliberate for the dev bring-up lane (owner ruling:
  SSH deferred), and contained today by the loopback-bound hostfwd (`tcp:127.0.0.1:5555-:23`); on any real
  LAN (the RTL8125 board lane) this is a wide-open root shell until an authenticated transport exists.
  Closing it is the SSH/authenticated-transport decision, not a tweak to net.text.
- **Concurrent network shell sessions** (plan/09 D44, the handle-transfer finding): live l4 connections
  cannot cross task stores and one NIC is one task's claim, so telnet sessions are sequential (one fused
  task per session); the brief's bounded-concurrency-of-4 waits on the Message API or a host-side per-task
  text broker. Same root cause as: a network session's *child* programs write to the machine console, not
  the socket (text is satisfied per-task from the executor environment; only the fused session task carries
  the socket text).

**Website voice (study 10) — context was given 2026-06-01; the owner's wording is still owed:**
- **Authoring statement** (#15): one paragraph on the front page ("programs are Rust today; the format is
  language-neutral") — the owner's prose and roadmap commitment.
- **Front-page jargon** (#16): "language-theoretic principles" is still the round-1 filter sentence; the
  try-it page redeems it for anyone who clicks through. Owner's voice either way.
- **Browser boot banner** (#9): humanize the first line vs keep the full provenance text.
- **Browser `save` vs `ls /bin`** (#4): `save` claims a path the per-run fs snapshot can't see — make the
  session overlay include shell-store writes, or reword `save`'s message. Touches the session design.

**Release/distribution (study 11):**
- **Publish automation** (D18): the 8-step manual sequence vs an `xtask publish` with tags/changelog.
- **Publish rehearsal** (D19): the `eo9` crate cannot be end-to-end tested before the real publish — run a
  local-registry rehearsal or accept the risk.
- **Publishing the guest SDK** (part of D13): registry users can run programs but cannot author them; is
  the guest SDK (`eo9-guest`, wit/) a published crate or repo-only?
- **Doctor's compile cost** (D2): `cargo xtask doctor` needs ~200 crates compiled before it can check
  anything. Accept and document, vs a dependency-light doctor.

**Carried from earlier rounds:**
- **Compose-time vs run-time provider parameters.** Changing a seed changes the composed artifact and forces
  a recompile. Owner parked the "late-bound parameter" idea until there is a clean design; revisit if
  deterministic sweeps start thrashing the compile cache.
- **In-kernel (Rust) drivers vs wasm-component drivers for boot-critical devices.** The working direction —
  proven by `disk.virtio`/`net.virtio`, and by the in-kernel `storedisk` virtio-blk driver existing only
  because the cache is kernel infrastructure — is drivers as wasm components over `eo9:pci`, with in-kernel
  Rust reserved for what the kernel needs before it can run components. Formal owner ruling still open.
  (plan/12 D43/D50/D58)

## Settled directions (recorded so they're not re-litigated)

- **Boundaries are honestly async (owner ruling 2026-06-02, SPEC `06c30e4`) — implemented:** everything
  that can wait is declared and bound async; nothing is sync because it "happens to work"; sync glue is a
  measured, runtime-enforced optimization for provably-never-waiting layers; **awaits are bounded** (an
  unbounded await across a trust boundary is a liveness bug). The net and storage chains are converted;
  the eofs engine is async-core with a sync facade; first-poll-inline is the runtime direction for making
  the fast case fast (merged off-by-default). The earlier all-sync convention is superseded
  (docs/spikes/eager-guest-forwarding.md carries the addendum).
- **Round-3 owner rulings (2026-06-01) — all implemented:** (A) uberblock rollback stays warn-and-fall-back
  (S7-1 closed). (B) auto-format stays, with "blank" hardened to a sufficiently large zero span — leading
  1 MiB + trailing 64 KiB + whole small devices (`a9e088a`; S7-2 closed). (C) **NICs are single-owner**;
  sharing is a virtual-NIC provider — `net.l2.switch` shipped (F9 closed). (D) IOMMU: spike now, build at
  real-board prep — done (`3f4f882`, docs/spikes/iommu.md; SMMUv3 bypass-by-default means no flag day;
  study-09 #7 closed). (E) long-running programs live under the **executor model** — `eo9:svc`/init
  shipped v1–v3 (C3 closed). (G) **no hosted CI** — "fragile and annoying"; test locally (D17 closed;
  Linux evidence rides on local runs). (H) crate renames done before publish: `eo9-eofs`,
  `eo9-bundled-programs` (D24 closed). (I) adversarial modification of the host machine is **out of
  scope** — tamper-evidence (the storedisk MAC) yes, host-adversary protection no (S7-19's threat-model
  framing rides on this). Vendor:device matching (C2) shipped as `pci.admit-vendor`.
- **Executor-model rulings (2026-06-01, docs/design/executor-model.md §8) — implemented:** `eo9:svc` +
  `init` naming; detach is an explicit grant (never in default child sets — "outliving your creator is
  authority"); restart policy is **required in v1 and is a pure component**, not a config enum ("prefer
  functions over complex configs"); console exit restarts the console, `poweroff` is an explicit typed
  intent honored only from init's console child; registry lifetime is root-process config (CLI: bound to
  the shell's life).
- **Policies are programs (SPEC `fdfa244`, design doc 2026-06-01):** decision parameters are pure
  components (provably import nothing), bound by ordinary composition. Shipped: pci admit policies, fs
  path policies, net port policies, svc restart policies. Runtime-passed policies stay instantiated (not
  fused); fusion is compose-time only.
- **Hardware mitigations are insurance, never load-bearing (owner ruling 2026-06-01, SPEC `d01f559`):**
  Eo9 must be secure on MMU-less hardware given a correct compiler; W^X stays as defense-in-depth; no
  mitigation zoo. **Cache flushing is a granted capability** (`69f09fd`) because it externalizes cost —
  the symmetric principle: reading microarchitectural state needs the time capability, perturbing it
  needs the cache capability. The Spectre audit (`a7d9d4b`, docs/spikes/spectre-audit.md) is pinned by
  config tests; on metal, masking is forced off (wasmtime couples it to signals-based traps), so the
  timer capability is the only metal Spectre mitigation (`762ac0e`).
- **The bind entrypoint (owner ruling 2026-05-30, plan/03 D21→D23) — implemented and hardened:** a
  configured provider re-exports its provider's API via direct aliases plus one `eo9:rt/configured.bind`
  export; executors call `bind` once after instantiation. Resource-owning providers are now configurable
  (`pci.filtered --allow [{…}]` shows exactly the allowed device on metal; `l4-over-l2-config` bakes
  addressing). The forwarding binder and its caveat list are deleted. `bind` returns `result<_, string>`
  (`c1bd95c`), so a configure refusal is a typed pre-run error on every target — never a trap. In SPEC.
- **Round-3 study fixes (2026-05-31 → 06-01) — merged:** eofs data-integrity hardening (`cb410c2`: atomic
  rewrites, gc, foreign-image refusal, image locking, per-process sessions, rollback warnings), PCI device
  quiesce before DMA free (`02460f9`), the net stack fixes (`9b2bc10`: ARP stall, RST classification,
  kernel-store stubs), the shell/website teaching fixes (`9fee776`: machine-verified help examples), and
  the release tooling (`be91552`: --version, real package sizes, metadata/MSRV, the pci.deny stub).
- **Owner rulings 2026-05-27 (the open design calls) — all implemented:** (1) `configure` is synchronous and
  minimal — DONE (`af9cb34`), and now bakes compound values too (`ca255c8`). (2) Guest trap reasons are
  readable — DONE, with panic message + location via `eo9:rt/diagnostics` (`f8dc070`, browser `9047c7f`).
  (3) `describe` composition tree — DONE on the CLI and at the eosh prompt; provider `describe` also shows
  configure args. (4) Entropy stays in the default child set — no-op by decision. (5) Roadmap order depth →
  breadth → real hardware: **both depth and breadth are complete** (three architectures at parity,
  CI-gated); hardware is on the way (an Orange Pi 5 Plus, ordered 2026-06-02 — prep list under
  "Bare metal" below).
- **Owner feedback 2026-05-29 (try-it page + shell) — all implemented:** type straight into the terminal
  (`72368b4`, polished `fbc5f38`); bare-default examples via optional `hello` args; the explore-the-sandbox
  section + a `help` that teaches operators with examples (`b49e952`); accurate `&` operand errors
  (`bdbb3e1`); `env` works in the browser via a seeded session manifest.
- **Purge-free caching (owner concern 2026-05-29)**: hash-named files cache forever; every mutable URL is
  `no-cache` + strong ETag, so deploys propagate immediately with no CDN purge. Pinned by tests.
  (plan/15 D28, `6312996`)
- **Networking is layered (owner directive 2026-05-28)**: separate `eo9:net/l2`, `/l3`, `/l4` capabilities;
  higher-over-lower stacks are ordinary middleware. Implemented, in SPEC, exercised end-to-end on metal.
- **The in-browser VM is fully self-hosted**: the `/vm` blob runs the real runtime + algebra + eosh + the
  Cranelift→Pulley compiler; the site is two pages; the old jco `/try` page is removed. (plan/18, plan/15)
- **Disk-cached native code is MAC-verified before deserialization (security review 2026-05-29)**: every
  `storedisk` cache entry carries a keyed blake3 tag over `cache-key ‖ length ‖ artifact`, verified before
  the unsafe `Component::deserialize`; the key is generated per-checkout (0600, never committed) and baked
  into the kernel image — tamper-evidence, not a secret-key boundary. Tampered entries are recompiled,
  never executed. (plan/12 D58)
- **No upstreaming until a compelling MVP** (owner ruling 2026-05-26): three contribution packages staged
  locally, **on ice** until the owner reviews/pushes them.
- **On-target codegen forked cranelift rather than waiting for upstream** — done; vendored forks under
  kernel/vendor, provenance-reviewed (incl. the riscv64 four-constant cranelift-codegen copy).
- **Unconfigured providers never trap** (owner ruling 2026-05-27, option C): standard stubs self-bind
  documented defaults; `pci.filtered` unconfigured = deny-all by the same rule.
- **Root-handle resources live in the API interface** (owner ruling 2026-05-27, option 1).

## Design decisions deliberately parked

- **Content-only vs layout-dependent eofs node hashes** — revisit if a guest-visible hash/verify surface is
  added. (plan/14 D4)
- **Component-typed arguments** (`interpret (…)`) — revisit when something consumes it. (plan/10 D6b)
- **dma-buffer ↔ `eo9:io` buffer relationship** (eo9:pci) — unify only if a future driver needs zero-copy
  paths into `eo9:io`. (plan/02 D14)
- **Exec-copy cleanup / crates.io name** — operational niceties, owner-facing.

## Functional gaps (implementation exists, coverage incomplete)

### Algebra correctness (from the PL study)
- **FIXED** — the drop-law failure, renamed-residual artifacts, the configured-middleware trap (bug 1),
  the missing property suite, undefined `≡`/identity/`empty`, the misleading `&` operand attribution
  (`bdbb3e1`), and the scalars-only configure limitation (`ca255c8`) are all fixed and regression-guarded.
- **FIXED — resource-owning configure**: the bind entrypoint (settled above) makes `pci.filtered --allow`
  and `l4-over-l2-config` work; configure refusals are typed pre-run errors (`c1bd95c`). Remaining nit: the
  eosh tokenizer still requires quoting compound literals (`--allow "[{…}]"`) — unquoted commas are a
  recorded follow-up (plan/03 D23).
- **OPEN — The spec-promised "exports match nothing" warning never reaches the user**: `compose_checked`
  returns `ProviderExportsUnused`, but surfacing it in eosh/CLI is still queued. (study 05 #7)
- Binder caveats (narrowed): depends on wasmtime 45's CM-async ABI encodings (one constants block);
  variant/result/flags/handle-typed configure values still refuse (with clear messages); the
  unbakeable-shape refusal is reported under the `Internal` error variant (cosmetic tidy-up queued).
  **RESOLVED — the suspended-subtask and cancellation caveats**: the async hardening matrix
  (tests/eo9-integration/tests/async_*.rs; docs/spikes/async-hardening.md) exercises parked forwarded
  chains end-to-end at depths 0-3 — host kill mid-park leaks nothing, acknowledged `subtask.cancel`
  cascades to `RETURN_CANCELLED`; the only traps are canonical-ABI contract violations, pinned. The
  suspended path runs in production on master: the storage chain (`pci.admit-address $ pci.filtered $
  disk.virtio $ fs.eofs` on metal, INTx through the filter) and the net chain (per-port l4 stacks over
  the switch) both genuinely await (plan/14 D25/D26, plan/09 D33). A callee that ignores `CANCELLED`
  parks its canceller forever (the SPEC bounded-await rule's concrete shape); the converted providers
  carry drop-guards + drain-before-reuse so a cancelled operation can't wedge state or misattribute
  completions.
- **First-poll-inline is DEFAULT-ON in every kernel build** (owner GO ruling 2026-06-03; the quiet-machine
  + metal numbers, the `xtask firstpoll-ab` gate, and the full feature-on battery are in
  docs/spikes/first-poll-inline.md "Default-on"). The escape hatch for an A/B or bisection build is
  `EO9_KERNEL_FEATURES_REMOVE=first-poll-inline`. Still open upstream-side: the spec conversation about
  the one semantic deviation (a callee that computes a long time before its first await blocks the
  caller's frame inline — unreachable from callback-ABI Eo9 guests, pinned by the gate).
- **The cancel-mid-flight metal probe is analysis-pinned, not executable**: the virtio drain-before-reuse
  invariant (plan/09 D34) is argued + unit-covered; an end-to-end cancel-during-DMA probe needs a
  cancellable metal consumer (queued with the hardening lane).
- Kernel algebra errors map to `Internal(String)` rather than the specific WIT variants; the kernel renders
  `wiring` as a leaf only; eosh `envinfo` still classifies authority by the `/types`-name heuristic.

### Runtime / providers (usermode)
- **FIXED 2026-06-02 — intermittent lost wakeup hung `eo9 -c` coreutils runs under load** (found
  2026-06-02 running the hardening matrix's CI; plan/13 D19): the `eo9:exec/task.wait` host fn discarded
  `child.runnable()`'s `Ready` in its child-is-blocked branch, so a child completion landing in the
  check→register window (it drained an empty waiter list — only the sticky flag recorded it) lost its
  only wake and the parent parked forever over a runnable child. Fixed in
  crates/eo9-runtime/src/link.rs (act on the edge: wake and re-poll); root cause, reproduction (40/40
  hangs with the window amplified, 0 after the fix), and rejected alternatives in plan/11
  "Lost-wakeup fix".
- **Guest-facing `resume` unsupported (E5)**: children are fuel-sliced from the parent's donation; no
  guest-directed scheduling. (plan/04 D11/E5)
- **Fuel-quantum resume shim** (10k granularity) until wasmtime can park a fiber at fuel exhaustion.
- **Capability coverage**: still **no host net root provider** in usermode (the layered guest stack covers
  metal); perf is a placeholder; the **Message API is unstarted** (blocks `text.capture`, pipes,
  parent↔child channels).
- **TCP/IP middleware depth**: ships without DHCP / IPv6 (types-only — wire it or say "IPv4 only") / an l3
  export; address overrides now bake (bind entrypoint). From the network study: no mock-fidelity
  conformance test (the same binary against loopback and metal), no background pump (the stack is frozen
  between l4 calls — document the semantics until the scheduler exists), socket/buffer constants are
  hard-coded rather than configurable, and no throughput/pps instrumentation. **FIXED — the metal
  compile-cache misses**: identical fused compositions now hit the per-session compile cache (`1f7a800`;
  repeat compositions ~0.3s), and `storedisk` persists across boots; the svc-detach `compile_component`
  path announces codegen but does not yet consult the session cache (queued). Capture-based (pcap-level)
  test assertions landed for the switch (the v3 pcap demo); generalizing them is a recorded area-13 item.
  (study 08 F3b/F6/F7/F10–F14/F17)
- **Codegen determinism not verified bit-for-bit**; cache keys carry `compiler_deterministic = false`.
  (plan/04 D3)
- **fs path containment is canonicalize-then-operate** with post-open fd re-verification as the shipped
  interim; openat2/`RESOLVE_BENEATH`-style walks remain the real fix. (plan/08 D7/D13)
- **Store/cache integrity**: usermode store is blake3 but unauthenticated (no signing/provenance story);
  the metal `storedisk` cache *is* MAC-verified.
- Shell `env` reads a session-manifest file; `/bin` and `session` are reserved names; the session overlay
  is composed host-side rather than via the guest `fs.overlay`. (plan/10 D9, plan/11 D15)

### Bare metal
- **PCI/drivers**: the `eo9:pci` provider runs on aarch64 and riscv64; **x86_64's PCI map is documented but
  its QEMU arm doesn't wire the `pci` grant yet**. INTx interrupt delivery is live on both GIC and PLIC;
  `disk.virtio` and `net.virtio` wait on interrupts through genuine awaits — **the nested-forwarding bug
  is FIXED** (study 09 demo 3c, the flagship `pci.filtered $ disk.virtio $ fs.eofs` chain, runs on metal
  with INTx pacing surviving the filter). Devices are quiesced before DMA free on every teardown path
  (`02460f9`); cancelled requests are drained before descriptor/buffer reuse (`ca6d0c5`). Still open:
  **gpu.virtio is the last eager-style driver** (conversion in flight, plan/09 D34 pattern; the
  disk/net no-wait ISR-ack alignment rides with it), machine-global device claiming (the `storedisk` vs
  `pci`/`disk` don't-combine rule stands), grant checks only after the compile (softened by the codegen
  announcements but not reordered), MSI/MSI-X / FLR / hot-plug / AER unimplemented, no fault-injection
  surface (a `pci.fault` middleware idea is recorded), and ops are still serialized at queue depth 1 per
  driver (the take/put slot — concurrent submission is recorded as a typed-busy vs queueing refinement).
- **Storage**: the `storedisk` compile cache and the **writable MAC-verified /bin** (`00d1eb2`: `save` at
  the metal prompt survives power cycles) are both live; cache eviction/space management,
  VIRTIO_BLK_F_FLUSH-on-commit, riscv64/x86_64 storedisk enablement, and disk-vs-baked provenance in
  `ls /bin` are queued. (plan/12 D58/D60) From the storage study: no fsck/scrub/df surface (engine
  `verify()` is unreachable), no full-stack crash-injection harness, scale untested (large files/dirs/
  images), no fuzzing of the on-disk parser, the mount banner counts artifacts without verifying them, and
  metal `env` omits the pci/disk grants. (study 07 S7-8/10/15/20/21/22)
- **Kernel hardening residuals**: kernel-image-internal W^X (`.text`/`.rodata`/`.data` split) and guard
  regions; exceptions other than IRQs are fatal; nested shells share the one serial console. (The
  single-slot idle waker is FIXED — `wake_idle` keeps a drain-all list, `1f7a800`.)
- **Graphics residuals**: gpu.virtio renders to a RAM framebuffer presented via virtio-gpu 2D (no
  acceleration — deliberate); `gfx.simplefb` (the U-Boot simple-framebuffer provider for real boards) is
  recorded for real-board prep; the DMA framebuffer must be allocated exactly once (freeing a DMA buffer
  quiesces the device — pinned by a comment + the allocate-once pattern).
- **Diagnostics/runner gaps**: the headless `program=` runner ignores `program=eosh` and does not carry the
  guest panic message (the interactive path does); on-target codegen determinism is not bit-compared;
  no instrumentation for peak compile heap / phase timings / cache-hit reasons. (plan/12)
- **Scripted-console conventions** (not kernel-input bugs): on riscv64, OpenSBI consumes a byte that
  arrives before the kernel exists — wait for the prompt; on every arch a full-speed pasted line can outrun
  the UART model — pace scripted input. (plan/12 D49)
- **Wasmtime version bumps are not free**: re-verify the binder/executor ABI-constant blocks and re-AOT all
  artifacts on any bump off 45.
- **Real-board prep is now scheduled** (an Orange Pi 5 Plus is ordered, 2026-06-02): GICv3 support (the
  kernel is GICv2-only), a DesignWare-PCIe config-access shim behind the existing `PciAccess` seam, a
  U-Boot boot recipe, and `gfx.simplefb`; the SMMUv3/IOMMU driver waits for board #2 (the RK3588's PCIe
  is not behind a usable SMMU — docs/spikes/iommu.md). The QEMU test tier is still scripted/manual rather
  than part of `cargo xtask ci` (per the owner's no-hosted-CI ruling, local runs are the gate).

### Website / in-browser demo
- **Blob reproducibility**: same-path rebuilds are byte-identical, but a build from a *different* checkout
  directory still differs by ~410 bytes of cargo unit-metadata for the `[patch]` path deps — full
  cross-machine reproducibility needs cargo-side or workspace-restructuring work. (plan/18 D26)
- **Asset churn**: every guest-SDK change re-fingerprints all `/vm` store assets (~11 MB of binary churn
  per such merge); if repository weight becomes a problem, move the committed web assets out of git.
- **Performance honesty**: browser programs are Pulley-interpreted — noticeably slower than native/metal
  for compute-heavy runs (the page says so).
- **Browser `/bin` is behind the kernel store**: the gfx family (`gfx.mem`/`draw`), the per-layer net
  stubs, the switch, and `vnic4check` are not in the browser blob yet — queued so the browser sandbox can
  demo the same compositions as metal. `eo9:svc` is registered absent (svc/detach refuse gracefully).
- **Remaining polish**: a click-through on the live deployed site (after the owner's next redeploy);
  lazy-fetching `/bin` pairs to trim the ~9.0 MiB raw blob; hash-named vm.js/vm.css (optional — they are
  `no-cache`-revalidated correctly today); COEP/Permissions-Policy headers; JSPI support outside Chromium
  re-check; the explore-the-sandbox copy could add an `env` line now that browser `env` works (plan/18
  D32's deliberate one-line follow-up).

## Tracked from the user studies (see docs/user-studies/00-synthesis.md for the full triage)

- Debugging: panic message + location now arrive everywhere (DONE); still open — full source-line
  backtraces, a documented debugger workflow, symbolized kernel exception dumps.
- Onboarding/authoring: `eo9 new` scaffold; per-package guest builds; auto-pickup (or a loud warning) for
  guest crates missing from `GUEST_COMPONENTS`; a beginner tutorial that defines store/component/provider
  vocabulary. (Optional/defaulted `main` args landed — `hello` uses them.)
- Error-quality consistency: fs errors still render as `fs("FsError::…")` debug text (round 3 added the
  browser resolution-path and Foreign-image-refusal instances); `ok:` vs `success(…)` outcome rendering
  split; `eo9 store --help` errors instead of printing help; the outcome line glues onto unterminated
  program output (R2-19, reproduced again in round 3); shell-path refusals print twice and exit 1 vs 3 on
  the direct path; `env readwrite` predicts a refusal that doesn't happen (study 11 D7); shell `env` still
  recommends nonexistent stub names and misclassifies rt/diagnostics (study 08 F5 remainder).
- Security follow-ups: hostile-component CI suite + fuzzing of the fs provider, the eofs on-disk parser,
  and the ABI boundary; signed stores/provenance for the usermode store (the metal disk cache is
  MAC-verified); align the symlink Denied/NotFound oracle.
- Performance/instrumentation: compose/compile/run timing split, cache-hit reasons, peak compile heap;
  on-target vs host-AOT parity; the zero-cost-layer claim needs a benchmark or softer wording.
- **Round 3 (sessions 07–11, 2026-05-31): complete.** 96 findings — 37 fixed (the merges are listed in the
  synthesis), 41 tracked (folded into the sections above and below), 18 owner decisions (now ruled — see
  settled directions; the website-voice wording is the one piece still owed). The fix wave's own reviews
  surfaced two more items, both handled: a session-lock ordering race (fixed, `db2f756`) and the fs-eofs
  checkout-dependence that keeps the bundle drift check out of `cargo xtask ci` (plan/01 D15).

### Round-3 distribution/release residuals (study 11)

- **The bundle drift check is not in CI** (D9b): it went in (`be91552`) and came back out (`23699a9`)
  because fs-eofs builds are not checkout-independent — cargo bakes the out-of-workspace `eo9-eofs` path
  dep's manifest path into `-C metadata` symbol hashes, which `--remap-path-prefix` cannot fix. Candidate
  fixes in plan/01 D15 (guest-workspace membership for eo9-eofs / comparison normalization / pinned
  metadata). The check remains in `cargo xtask package`. (Bundle refreshes are therefore done from the
  main checkout — a worktree rebuild of fs-eofs produces a different hash.)
- No platform statement / non-unix compile gate (D12): README "Supported platforms" + `compile_error!` on
  non-unix in eo9/eo9-providers-unix.
- Registry users can't author programs and nothing says so (D13 documentation half).
- Downgrade silently reverts bundled programs (D16): record the seeder version, warn on downgrade.
- Bundle-size headroom unmonitored (D23): a size assertion in the pre-flight (1.08 MiB of 10 MiB today).
- Duplicate setup/doctor summaries (D4); `~/.eo9` not XDG-compliant (D25).

## Minor nits / housekeeping

- Guest `wit-bindgen` is a temporary git pin (upstream main, 0.249 family) — return to a crates.io pin at
  the first published release with wit-parser ≥ 0.249. (plan/07 D9–10)
- `eo9:exec/args` (types-only) is linked only when exec is granted, contra the types-always-available
  convention.
- Guest-level kill-then-wait test deferred; host-level covered.
- plan/04 D12 still describes the (long-fixed) binder trap; update to point at plan/03 D12–13.
- Empty per-process exec-copy directories are never cleaned from the temp dir.
- `eo9-sched` not yet adopted by the CLI drive loop.
- Root host workspace manifest lacks a `license = "MIT"` field.
- Full-feature kernel cargo builds emit two known warnings outside the clippy gate (`arch::NAME` dead code;
  one unnecessary-unsafe block in the x86_64 wasm config path).
- kernel/vendor/README.md documents the cranelift-codegen copy but is still missing the algebra-crate
  section (wit-parser, wac-*, wit-component, wasm-wave) — documented only in plan/12 D30–35.
- The net-l4-over-l2 crate's local world.wit header still mentions an "optional DNS server" its config
  deliberately doesn't take (cosmetic; fix on next touch).
- `time.monotonic-stub` panics when used unconfigured instead of refusing typed (contra the
  never-trap-unconfigured rule; recorded on the conversion wave).
- The eosh tokenizer requires quoting compound configure literals (unquoted commas split arguments) —
  recorded follow-up, plan/03 D23.
- Switch-over-switch stacking should be re-tested now that the chain genuinely awaits (the pre-conversion
  attempt hit the suspension wall; expected to just work).
- The owner pushes master to GitHub (github.com:wyager/eo9); planner-side agents never push.

## Composed-spawn PCI grant refusal (found 2026-06-07, pre-existing)
`pci.filtered $ disk.virtio $ cancelcheck` is refused at instantiation ("boot did not
grant PCI") even on a `pci` boot, while direct interactive `lspci` works — contradicts
the earlier verified filtered-chain flow (plan/12). Needs a lane: the spawn-path grant
propagation for composed pci chains vs the interactive path.

## Backstop detector first real hit: stranded runnable on the board (2026-06-08)
During net.rtl8125's polled gateway wait on the Orange Pi, the idle backstop detector
fired once: `liveness: stranded runnable: a child or service was runnable across an
entire idle backstop (n=1)`. Per the event-driven-liveness doctrine this is a
high-priority bug (a wake edge is missing on some board-profile path the QEMU battery
never exercised). Reproduction context: board profile, polled driver wait, l2check
gateway ARP wait. Needs a kernel-lane investigation once the driver lane settles.

## Board console input truncates at exactly 64 bytes (UART RX FIFO never drained by IRQ, 2026-06-08)
Root-caused on the bench after four incidents: any console line longer than 64 bytes
truncates at EXACTLY byte 64 (deterministic — two identical commands mangled at the
same column; a 64-char command lost only its newline, byte 65). The DW-APB UART RX FIFO
on RK3588 is 64 bytes deep, and the kernel's own backstop printed the mechanism:
`stranded input: the idle backstop scavenged receive bytes the interrupt path missed
(n=16)` — the RX interrupt path is NOT draining the FIFO on the board profile; input
only reaches eosh when the idle backstop scavenges it. Between scavenges the FIFO
overflows silently. QEMU never showed it (different console path). The heartbeat-
collision theory from earlier today was wrong — the hb line in the echo was a bystander.
Bench workaround in place (eosh_cmd.py types in 40-byte chunks with 6s scavenge pauses,
plus a redundant trailing newline). Needs a kernel lane: enable/fix the DW-APB UART RX
interrupt (or an adequate poll cadence) on the board profile so console input drains at
line rate; the liveness backstop scavenge is currently the ONLY input path.

## [FIXED 2026-06-08, area/09 merged — round 9 fiber-sliced codegen] Heartbeat (and possibly watchdog pat) starves during on-target codegen (board, 2026-06-08)
During a 486 KiB composed-component compile on the Orange Pi the 5s `hb` heartbeat
stopped for the whole compile (>12s) — the drive loop isn't running while codegen hogs
the core. If the DW-WDT pat lives in the same loop, any composition that compiles
longer than the 22.4s watchdog period will hardware-reset the board mid-compile.
Today's images compile under that bound; a kernel lane should either pat from a path
that survives long synchronous work or yield periodically inside codegen. (Also the
liveness doctrine angle: a >12s scheduling gap for the drive loop is itself a
starvation signal on a single-runnable workload.)

## wait_until conflates the frozen-clock backstop with wall-clock windows (dhcp lane, 2026-06-08)
The provider pump's `wait_until` bounds waits by ROUND COUNT (standing cap 4096), but
empty receive polls complete in microseconds on fast paths — 4096 rounds elapse long
before the intended wall-clock window (the DHCP lane needed ~20 s for smoltcp's 10 s
discover retransmit and had to raise its cap to 2^20 rounds). Any other deadline built
on the same helper may be silently round-limited rather than time-limited. Needs a
real coarse time source in the wait path (or an explicit two-parameter bound:
rounds AND ticks) — kernel/providers lane.

## Compile-fiber stack has no guard page (review finding, 2026-06-08)
The 2 MiB heap fiber that hosts sliced on-target codegen (and the existing async
fiber stacks, which share the property) is plain heap memory — an overflow corrupts
the heap silently instead of faulting. 4× the 512 KiB main stack the compiles
previously shared, so headroom is real, but a guard page (or a canary check at
fiber exit) belongs in the kernel lane's queue.

## Network-session poweroff no-ops silently (bench, 2026-06-08)
`poweroff` typed in a telnet eosh session does nothing and prints nothing. If the
session stack lacks the power capability, the honest behavior is a typed refusal
("missing capability: power"), not silence. Silent no-op cost a bench recovery
round (the operator assumed the command executed). eosh/session-stack lane: make
the refusal explicit; decide whether network sessions should ever be grantable
power (probably yes behind an explicit telnetd flag — remote reset is operationally
valuable, as this same incident proved via the session-burn workaround).

## svc_shell flaked once under parallel workspace test load (review, 2026-06-08)
During the area/09 merge review, `svc_shell` failed once (exit 101) in the parallel
`cargo test --workspace` run, then passed 8/8 solo and the full ci re-ran green. The
branch under review does not touch svc/shell paths — smells like load-sensitive
timing in the service-restart tests. Watch item: if it recurs, it graduates to a
real bug hunt (record the failing test name and seed next time).

## Check gates can leak the QEMU on the failure path (dhcp lane, 2026-06-08)
A failed check gate (kill()+wait on error) left its qemu-system-aarch64 alive holding
the 5555 hostfwd port; the NEXT gate then died instantly with "serial stream ended"
and zero output — a failure mode that doesn't name the cause. Two fixes: make the
gates' teardown verify process exit (kill-by-port preflight or a bind check before
launch), and make the instant-EOF failure print "port 5555 already in use?" as a
hint. xtask lane, small. Same family (usb lane, same day): check-telnet's FLAT step
timeout killed a healthy run on a loaded host — the round-9 sliced codegen trades
wall-clock for liveness, so compiles take longer under contention while printing
progress the whole way. Gates should use progress-aware timeouts (no-progress alarm,
like the serial-loader sender) rather than flat bounds. Third sighting same day (gfx
lane): three worktree batteries serialized on the single hard-coded 127.0.0.1:5555
hostfwd — parallel lane batteries are now routine, so the gates want a per-worktree
port (xtask --port or derive from the worktree path hash).

## DHCP follow-ups from the merge review (2026-06-08)
(a) check-dhcp's first probe requires upstream DNS (waits for `ok: resolved(`) — the
gate fails offline even though the lease assertions are deterministic; accept the
typed no-answer as an alternate marker or split the probe. (b) The no-DHCP-server
typed timeout has no committed pin — the reviewer's scratch shape (dhcp over
net.l2.echo + stub clock, expect "no lease arrived", ~4.8s) should be adopted as a
real usermode test; shape preserved in the review transcript (.review/ in the
review-rtl worktree).
