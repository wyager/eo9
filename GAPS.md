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

## Ungranted-platform composition spins instead of refusing typed (found 2026-06-10, L2 msd lane)
On a QEMU boot whose command line carried `pci` but NO `platform=` grant,
`usb.ohci $ usb.msd $ mdcheck` at the eosh prompt never returned: the serial stream
filled with `liveness: stranded runnable: a child or service was runnable across an
entire idle backstop (n=…)` lines until the gate's 300 s timeout. The same chain on a
`pci platform=pl031-rtc` boot refuses promptly and typed (usb.ohci's no-controller
probe) — which is how check-usb always boots and how check-msd's refusal arm now
boots. Expected per the capability posture: an ungranted import should surface as a
typed denial/refusal, never a spin. Reproduction: drop `platform=pl031-rtc` from
check-msd's `-append` and run the gate's step 2. Needs a kernel/spawn-lane look
(possibly the same grant-propagation family as the composed-spawn PCI refusal above).

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

## [GRADUATED 2026-06-08, M3-fix round: now flaky SOLO — needs a real bug hunt] svc_shell flaked once under parallel workspace test load (review, 2026-06-08)
UPDATE (usb M3-fix round): `restart_cycles_complete_while_the_foreground_is_quietly_blocked`
failed in `cargo xtask ci` again AND then failed 1-of-3 SOLO runs
(`cargo test -p eo9-integration --test svc_shell`) on an otherwise idle machine —
assertion at svc_shell.rs:410 "the backoff lifecycle (trap, 2 delayed restarts,
give-up) completed during the quiet gap". Not load-conditional; per the original
watch item this graduates to a service-registry timing bug hunt (the usb lane does
not touch svc paths). Original entry:
During the area/09 merge review, `svc_shell` failed once (exit 101) in the parallel
`cargo test --workspace` run, then passed 8/8 solo and the full ci re-ran green. The
branch under review does not touch svc/shell paths — smells like load-sensitive
timing in the service-restart tests. IDENTIFIED (usb lane, 2026-06-08 late):
`svc_shell::restart_cycles_complete_while_the_foreground_is_quietly_blocked`
(assert at svc_shell.rs:410), failing ~1-of-3 SOLO on an idle machine — a real
service-registry timing bug, not load sensitivity. SECOND SPECIMEN (curl merge
review, teed per protocol): `disk::tests::read_only_opens_share_the_image`
(crates/eo9-providers-unix/src/disk.rs:483), passes solo, failed under concurrent
lane batteries — likely BlockingPool timing under CPU starvation; distinct bug from
the svc_shell one. The hunt lane should take both: run each under artificial load +
solo loops, with logs retained.

## svc_shell flaked once under parallel workspace test load (review, 2026-06-08)
RESOLVED (flake lane, 2026-06-09; cause 3 restated after a review block — the first
mechanism only narrowed the window and failed the reviewer's 10-burner soak 1-in-10) —
three distinct causes on area/23-flake-fixes:
1. `restart_cycles_complete_while_the_foreground_is_quietly_blocked` (was
   svc_shell.rs:410): the test asserted the whole backoff lifecycle finished at the
   end of a fixed 600ms quiet gap, but each restart decision recompiles the policy
   component (~210ms/decision in debug; instrumented), so three decisions + 120ms of
   delays exceed the window — 16/200 solo, 10/10 at load ~30. The park machinery is
   precise (respawns 0–1ms after due). Test now polls `svc list` for the terminal
   state; lifecycle-shape asserts unchanged. 0/501 solo, 0/505 under load after.
2. `disk::tests::read_only_opens_share_the_image` (disk.rs:483): the pool worker
   delivered a completion while still holding its Arc<File> clone, so the exclusive
   flock could outlive the observed write completion and the provider drop; the next
   open got WouldBlock. 2/1000 solo, 2/2000 at load ~30. Fixed: the clone is dropped
   before the completer runs (completion observed ⇒ that op's handle released).
   0/2000 solo, 0/4000 under load after.
3. The load-16-36 "fails 3-of-3 on master" mode was the park backstop's liveness
   detector, not the tests: its foreground arm tripping the suites' no-`liveness:`
   gate (foreground=true) on completions that were delivered but resumed late under
   load. Final mechanism (per review): the foreground arm is REMOVED by category —
   while the drive thread is parked, foreground runnability is defined as "doorbell
   rang" (Task::is_runnable), so any parked-side poll that sees the foreground
   runnable has proven delivery; a missed edge (completer never rings) is invisible
   to that poll however gated, detectable only completer-side at the ring site
   (documented in the detector). A fired-waker gate alone (first attempt) merely
   narrowed the race and still failed a 10-burner soak 1-in-10. The services arm —
   the detector's real value, state that can change without this thread's wake-set —
   stays, with two time-shaped exclusions: fired waker (late resume) and a restart
   deadline crossing into dueness during the park window. Acceptance soak: full
   suite x20 under 10 burners (load ~14-28), 0 failures, 0 liveness findings.
Residual (tracked, by design for now): restart-policy decisions recompile the policy
per decision (docs/design/policy-components.md blesses precompile-and-cache as the
upgrade); ~210ms/decision debug-build floor on restart-lifecycle latency.
NEW SPECIMEN (editor-wrap lane, 2026-06-09): same test, different bound — the 30s
*marker* deadline ("the session never printed \"detached: crasher\"",
svc_shell.rs:370) blown in 2 of 4 full `cargo xtask ci` runs and once solo
immediately after a full battery (the initial detach compiles the child + policy
components in a debug build, on a still-contended machine); solo runs on a settled
machine pass every time (13.4s single, 8/8 suite in 32s), and the other 2 ci runs
were fully green. The lane's diff (eosh editor output layer) does not touch
svc/exec. Same debug-compile-latency residual as above, hitting the fixed marker
bound instead of the (now-polled) gap — the marker wait wants the same
poll-not-deadline treatment or a longer bound.

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

## eosh session history is unbounded (restored entry, 2026-06-08)
`Session::history` (eosh-core/src/session.rs:62,103) grows without bound per executed
line. Believed GAPS'd earlier but the entry never landed (caught by the repl study's
cross-check — the no-drop rule working). Fix folds into the incremental-parser M2
editor work: the recall ring becomes a capped view (e.g. 64) of session history.

## [FIXED 2026-06-08, USB M3-fix — per-region quiesce hooks] Platform-provider DMA teardown has no generic quiesce (recorded 2026-06-08, USB M0 lane)
RESOLUTION: `RegionDef::quiesce` (per-device fn, run at claim release before the
task's DMA buffers free); the board OHCI regions drop the controller to UsbReset.
The M3 board idle-reset incident was this gap live (an operational OHCI DMA-writes
HccaFrameNumber into the HCCA every 1 ms — after teardown, into freed heap). The
PCI-exclusivity-convergence half of the entry remains open. Original entry:

`eo9:platform` frees a task's DMA buffers at teardown exactly like `eo9:pci` — but a
platform device has no bus-master bit, so the provider cannot generically revoke a
device's licence to DMA before the memory returns to the heap (pci_provider's
quiesce-before-free, study 09 finding 6, has no platform analogue). Harmless today:
the only region tables are the QEMU test regions (PL031/PL061, no bus mastering) and
the empty board table. BEFORE the M1 board lane adds the RK3588 OHCI regions, region
teardown must gain a device-aware quiesce hook (for OHCI: HcControl -> reset, which
halts all schedule DMA) or an equivalent containment story. Also recorded there: the
PCI provider's claim exclusivity is still per-task while platform's is machine-wide —
converge PCI on the machine-wide discipline (its recorded follow-up).

## RTL8125 PHY degrades across kexec + repeated claims (bench, 2026-06-08 night)
After a kexec jump plus several claim/release cycles in one boot, the NIC degraded:
first DHCP windows with zero wire RX (link reported up), then hard LinkDown on later
claims — while switch/device LEDs looked normal. A full reset (SYSTEM_RESET → U-Boot
→ fresh boot) recovered it completely (ARP resolved first try). Suspects: the kexec
quiesce path leaving PHY state the next claim's warm re-init (ram-code skip path)
doesn't recover, or cumulative u2/PHY state across rapid claims. Driver lane: consider
a full PHY reset on claim when the previous owner was quiesced-by-kexec, or always.
Bench rule meanwhile: if LinkDown appears, cold-cycle rather than retrying claims.
New datum (bench, 2026-06-09 evening, NO kexec involved): one completed cycle plus
two spawns KILLED mid-link-bring-up (Ctrl-C ~3s in, drive-stats bracket probes), then
the 4th spawn hung silently — no codegen/dhcp/error output for 150s; drive-stats
showed the driver fuel-yielding (~32k rungs/160s) with zero progress and zero events:
the autoneg/link poll spinning forever. So kill-mid-bring-up aggravates the same
degradation without kexec — the kill path's release likely strands the PHY mid-autoneg.
Also: the driver should not spin SILENTLY forever on a dead link — it needs a typed
LinkDown refusal after a bounded autoneg window (counted polls are the doctrine-accepted
clock there), so the console sees an error instead of a hang. SYSTEM_RESET recovered
this instance too.

## svc_shell flake has WIDENED to a 3-of-3 CI blocker on master (usb rebase round, 2026-06-08 night)
On pristine master 24c8578 (fresh detached worktree, no branch changes), `cargo test
-p eo9-integration --test svc_shell` failed 3 of 3 consecutive runs, 1-2 tests each,
rotating among: restart_cycles_complete_while_the_foreground_is_quietly_blocked (the
original specimen), soundness_a_detached_child_cannot_use_what_its_detacher_did_not_compose,
detach_list_stop_clear_lifecycle, services_die_with_the_process — several with
"liveness: the park backstop found stranded work (foreground=true, services=false,
n=1)" in the transcript. Host was under multi-agent load (runtimes 17s-59s), which
the original entries predicted would worsen it, but solo-on-idle failures were
already recorded. This now BLOCKS any branch's `cargo xtask ci` from exiting 0
reliably; the graduated bug-hunt lane is urgent. The usb branch documents it as the
sole ci failure with this master baseline as proof.

## Bundle checkout-dependence is a CLASS, not one component (kernhyg lane, 2026-06-08; breadth verified 2026-06-09)
The fs-eofs path-dep byte-churn applies to EVERY component consuming an
out-of-workspace path dep (-C metadata hash): now curl, l4check, hidcheck, usbcheck,
net.rtl8125, usb.ohci, usb.ohci-pci via eo9-dns/eo9-ohci/eo9-rtl8125/eo9-curl-core.
BREADTH (area/37 lane observation; reviewer-verified, then root-cause SPLIT at the
area/37 merge): the all-87 staleness has TWO causes that compound. (1) The residue
class proper — components built from a different checkout path. (2) A wit/ file
change ripples into EVERY component's compiled module (the bindings layer consumes
the whole wit directory), so any branch touching wit/*.wit legitimately changes all
87 components' bytes — verified at the area/37 merge: the main checkout itself
rebuilt all 87 differently after the usb.wit change, self-deterministically, with
the inner core module (not just metadata) shifting. Practical consequence is the
same for both causes: a full `refresh-components` anywhere but the main checkout
writes residue, and a wit-touching merge requires the full 87-blob refresh from the
main checkout (plus the web-vm blob rebuild, which embeds component bytes). Standing rule sharpened: refresh-components is
MAIN-CHECKOUT-ONLY, lanes commit only the components their diff actually changed, and
the reviewer treats ANY bundle diff from a worktree as suspect-residue first.
Real fix candidates: workspace-ize the shared crates or pin metadata hashing.

## Network kexec residuals (area/21-kexec, 2026-06-08)
The lane shipped with three recorded residuals (docs/board/net-kexec.md has the full
posture):
- **Cleartext preshared secret**: oskexec's mandatory >=16-byte secret gates the TCP
  peer, but it travels cleartext on the LAN — a passive sniffer who also wins the race
  inside the one-shot window could replay it. Same class as the net.text telnet entry
  above, but with an actual gate in front because the authority is total. Upgrade
  path: a challenge-response handshake, which needs a real hash (blake3 is already in
  the tree) reachable from the guest world — deliberately NOT hand-rolled in the lane.
- **kexec + granted PCI DMA**: the staging region is heap-external so no capability
  can address it, but a bus-mastering PCI device could still DMA over it (the standing
  no-IOMMU posture). The `kexec` token is documented as the same total-authority class
  as `pci`; an IOMMU lane would close both.
- **TCG transfer pace decays — measured, no hard cap**: the TCP-staging path's rate
  decays from ~2 MiB/s to ~30-65 KiB/s as bytes accumulate (reproduced on every run;
  guest-side buffer reuse and ack batching in oskexec improved but did not remove it —
  the per-64-KiB recv/stage/send call chain is the unit that slows). Ruled OUT by the
  full-image soak (`EO9_CHECK_KEXEC_FULL=1 cargo xtask check-kexec`, 2026-06-08): a
  cumulative round/handle cap — the full 62.5 MiB (953 ack intervals) staged, verified,
  and kexec'd green in ~21 min, well past the ~654-ack point where an earlier run
  tripped the (then 10 s, now 60 s) sender stall alarm. Remaining suspicion for the
  net/runtime lanes: per-call accumulation in the async machinery or kernel-heap
  free-list growth — a profile, not a guess, is the next step. Consequences encoded:
  the default gate flashes a minimal-store kernel B (narrated, not silent; the soak
  arm covers full size), send_image.py --tcp uses a 60 s stall window, and the gate is
  on-demand like check-telnet/check-usb rather than in `ci`. The board runs native and
  is wire-bound.

## Service spawns carry no root capability grants (bench, 2026-06-09)
Both `detach` and init's services config refuse compositions with unsatisfied ROOT
imports (platform/console-sink/etc) — correctly, per "a detached service runs with
exactly what its detacher composed" — but there is no way to grant roots to a
service at all, so the demo plan's `station` config (`kbd = usb.ohci $ usb.kbd
restart restart.always`) cannot exist. The plan assumed service spawns inherit the
boot grant tokens like the console session does. Lane: the svc registry's spawn
path should link root providers per the same boot grants (operator-authored config
= console-equivalent trust), and init's config grammar needs either composition
support or saved-composition references for multi-component services.
RESOLVED (svc-grants lane, 2026-06-09, area/29-svc-grants): the kernel registry now
links the boot-granted operator roots (pci/platform/gfx/kexec/console-sink — the
same boot-constant bits as the console's spawn linker) plus the ambient time/entropy
roots into every service; detach's typed not-closed refusal stays for anything this
boot cannot satisfy (fs/exec/svc/net/unknown interfaces, or an ungranted root).
init's config grammar gained `$` chains (provider flags = configure args; the last
segment is the binary); the `station` boot token bakes the demo config (the
usb.ohci-pci variant under QEMU, where the platform table has no OHCI); the new
`check-station` gate proves the always-on keyboard service end to end with zero
typed foreground commands. Posture recorded in SPEC ("Services and detachment"),
executor-model.md (the kernel refinement + the intersect-with-the-detaching-session
rule any future narrower detach-holding session must apply), and svc.rs. The
usermode registry is unchanged (composed-only).

## init config grammar: `$` in a flag-value position parses as the value
(review train, 2026-06-09) `kbd = usb.ohci --region $ usb.kbd` eats the `$` as the
`--region` value and refuses with a confusing-but-typed error rather than naming the
likely mistake (a missing flag value before a chain separator). Operator-authored
input, hinted by the error text — cosmetic. Lane: teach the config parser to refuse
`$` (and `=`) as a bare flag value with a "did you forget the value?" message.

## Battery gates: unbounded `child.wait()` after `poweroff`
(review train, 2026-06-09) Several QEMU gates (check-station and siblings) end with
an unbounded `child.wait()` after issuing `poweroff` — a guest that wedges during
shutdown hangs the gate instead of failing it. Shared pattern across gates. Lane:
a bounded wait (generous, e.g. 60s) with a loud timeout failure, applied to the
shared gate helper rather than per-gate.

## Editor parser step costs ~50 ms on target — needs its own study lane (bench, 2026-06-09)
The bench root-caused multi-second backspace on the Orange Pi to `eosh-inc`'s
combinator step cost: ~50 ms per `state.step()` ON TARGET (host-side the same step is
microseconds), measured as ~52 ms/char forward-echo latency and a backspace slope of
~54 ms/char while `on_backspace` still replayed the whole line. The replay is FIXED
(repl-m3 lane: snapshot-per-char state stack, backspace is O(1), differential tests
pin pop == reparse), but the underlying per-step cost is the real anomaly and is NOT
fixed — suspects: per-step combinator clone/alloc churn (`Box<dyn>`/`Rc` trees) under
the target allocator, possibly scaling with vocabulary breadth (~38 /bin components in
the head alternation). The M3 layer adds up to two `completions()` walks per typed
word character (the name-mark oracle) and per-flag candidate `Words` branches — cheap
on host, unprofiled on target; the same lane should measure them (the oracle can drop
its String allocations via a dedicated non-allocating trait query if it shows).
Follow-up lane: profile a single step on the board (alloc counters first), then either
arena-allocate or shrink-clone the combinator states or precompile the grammar's static skeleton.
Until then per-key feel on the board is bounded by ~50 ms/char echo.

UPDATE (area/33 study lane, 2026-06-09 — docs/study/parser-step-cost.md): the parser
stack is measured and CANNOT be the 50 ms. Host-native a tracked keystroke (step + the
two M3 oracle walks) is ~14 µs / ~820 allocs; the same component bytes under our kernel
+ on-target cranelift + in-wasm dlmalloc on QEMU/HVF cost ~98 µs net (wasm tax ~7×);
A76-scaled that bounds the parser layers at ~0.2–0.4 ms/keystroke — ≤1% of the
observed number. The board's own data corroborates: O(1) backspace (zero parser work)
measures ~54 ms, same as typing, and the pre-M3 slope's ~0.85 s fixed intercept matches
the 1 s idle backstop (the scavenge-rescued-RX era — a liveness finding, not a parser
cost). The residue is a board-only layer; the discriminating probes (in-guest timing
builtin, isolated-vs-burst keystroke, cache-attribute memcpy probe, FTDI latency-timer
audit of the bench harness) are listed in the study §5 and belong to a board lane. The
real parser-lane findings: the oracle walks are ~95% of per-key cost (a non-allocating
early-exit name query is the 5–7× first rung) and the word-end provide_args rebuild()
is an O(N) 1.4–3.0 ms (HVF) spike that should re-arm incrementally — ladder in §6.

UPDATE (area/34 fuel-yield-latency lane, 2026-06-09): **H1 — fuel-yield quantization
riding a wake timer — is REFUTED**, and the board residue is sharpened. The QEMU
reproduction under the board's exact station topology (echolat.py `--config station`:
init + the kbd service chain + eosh as a fuel-sliced console child) shows NO
tracked-key anomaly — station == plain, ~0.1 ms/key HVF, ~1.5 ms TCG — and the new
`drive-stats` counters prove the mechanism H1 required does not exist: a fuel yield
DOES ring the child's poll waker and the drive loop re-polls hot (tracked keys cross
~30–50 FUEL_QUANTUM slices per key, all hot; the executor never sleeps mid-keystroke).
Fuel burn is deterministic, so the board crosses the same quanta: ~46 ms tracked /
~3 ms flag ≈ 30–50 drive passes at **~1 ms+ per pass on the A76 vs ~6.5 µs for the
identical pass on HVF** — a ~150–200× per-pass execution anomaly, the same categorical
multiplier §5 cornered, now pinned to the drive pass (and incompatible with cadence
quantization: pre-fix the station loop never parked at all, and 3 ms is no cadence
multiple). What the instrumentation found INSTEAD, both fixed on the area/34 branch:
(1) the hot branch's blanket `wake_idle()` re-rang every parked future each pass — one
hot pass made all later passes hot, so the station executor NEVER parked (27.8 M
passes / 3 min, 100% CPU at an idle prompt, board included); now due-event delivery
only, and the owner's executor ruling is in force — the 10 ms/1 s idle re-poll caps
are deleted, the idle arm is exactly the earliest real deadline (sleeps, the 5 s board
watchdog-pat obligation, the QEMU-only 1 s feed-kick), idle CPU measures 0.0% plain /
~6–9% station-TCG (the remainder is the kbd service's own 2 ms HID pacing — a
service-side follow-up: make usb.kbd intx-driven); (2) console-sink `inject` raised no
wake at all — a USB keystroke sat until the next timer — now an input-arrival edge is
checked before any park (QEMU A/B: injected-key median 9.2 → 6.2 ms, min 3.1 →
0.76 ms; the residue is QEMU's own OHCI HID poll pacing). Detectors per doctrine: a
park-gate scan (rung-after-check-in = loud `liveness:` finding + hot recovery; proven
to fire under the `chaos-strand-runnable` arm while holding ~0.1 ms keys), and
stranded-input scavenges now report on EVERY wake kind. The board residue stays OPEN
as: per-drive-pass cost ≈ 1 ms on the A76 (fiber resume/suspend, store traversal,
or the §5 memory-attribute suspicion) — one boot of the `drive-stats` image
(`EO9_KERNEL_FEATURES_EXTRA=drive-stats`) gives passes/s + the rung/wake histogram to
split per-pass overhead from raw guest-execution slowness. Queued follow-ups: the
usermode twin (`drive_with_services`' 10 ms ambient park backstop — its stranded-work
detector currently DEPENDS on that cap firing, so the deletion needs the detector
redesigned completer-side first; S→M), and usb.kbd's 2 ms poll pace → intx-driven.

UPDATE (area/38 first-poll-parks lane, 2026-06-09): the silicon drive-stats round
KILLED area/34's "~1 ms+/pass on the A76" inference — the board's empty-bracket spin
measures **~7.5 µs/pass on silicon**, matching HVF's 6.5 µs: the A76 pass machinery is
fine. H2 (per-keystroke first-poll-pending host futures — fs/oracle reads parking
deadline-less until the kbd pacing rescues them) is **also refuted** by the new
host-call census + park-composition histogram (drive-stats, this lane): a tracked
keystroke makes exactly TWO host calls (read-key 1.05/key, text-write 1.1/key; fs-*
≈ 0 — the M3 oracle is pure guest compute, and the vocabulary fs walk runs once per
PROMPT, so "~34 ≈ /bin+builtins" was numerology), there are ZERO mid-key parks on
TCG (back-to-back tracked keys: median 0.1 ms, ONE park for the whole 20-key burst
on plain — composition K, event-woken; no hangs, no 1 s rides), and the only park
compositions that exist anywhere are healthy: KSD (read-key + kbd-sleep,
deadline-armed) and K. The silicon ~34 deadline-parks/key now have a simpler
arithmetic identity: **parks/key ≈ inter-key gap ÷ 2 ms in every dataset** (QEMU
50 ms settles → 23.6/key ≈ 50/2; silicon ~34 ≈ 57–68/2) — the parks count the GAP
between keys at the kbd pacing, not the key, and the wake counters cannot
distinguish a slow key from a slow harness. Prime suspect for the flat 52–61 ms
(uniform across tracked/value/backspace — the signature of a per-round floor, and
§5's FTDI-latency-timer/reader-pacing audit was never run): the bench harness's
send→observe round itself. The discriminator is now IN the image: a kernel-side
**key→echo meter** (input-edge stamp → first guest `text.write`; immune to harness
pacing) prints in every drive-stats dump — TCG measures 0.45–0.6 ms mean. CLOSED (bench
round, 2026-06-09, post-FTDI-replug): the board meter reads **key→echo mean 2.4 ms
on silicon** (count=41 total-us=97411 max-us=36051; the max is a single outlier),
wake-event ≈ keystroke count (input is event-woken), parks all KSD (healthy
read-key + kbd-pace) — and the U-Boot serial console echoed through the same bench
harness at ~56 ms, pinning the 52–61 ms external numbers on the harness's FTDI
round-trip floor, not the kernel. The keystroke-latency residue is RESOLVED: fix
the bench harness (FTDI latency timer / reader pacing), not the image. Also landed: the
orphaned-pend detector (a park with running work checked in but NO registered idle
waker and NO requested deadline is loud — the deadline-less silent-pend shape H2
predicted; zero firings anywhere today, it guards against one being introduced).
Still open beside this: the task.wait self-wake spin (area/35 class A — 133k
passes/s, 100% CPU whenever a foreground job is waited on; the empty-bracket control
measured it directly on silicon).

## build-kernel opi5plus overwrites the shared QEMU kernel ELF — silent wrong-binary boots
(repl-m3 lane workaround, recorded by the review train, 2026-06-09) `cargo xtask
build-kernel aarch64 opi5plus [minimal]` builds into the same cargo target path the
QEMU profile uses (`kernel/target/aarch64-unknown-none/release/eo9-kernel`), so a
board build silently replaces the QEMU ELF: the next `qemu`/`check-*` run boots the
DW-UART board binary and produces no serial output on the virt machine's PL011 — a
debug round was lost to exactly this. The check gates rebuild before booting, so they
self-heal, but any direct `-kernel` use of the stale path bites. Lane: per-profile
artifact paths (e.g. a `--target-dir` or renamed output per board profile) so the two
binaries can never shadow each other; until then, sequence board builds AFTER QEMU
gates in any battery.

## USB HID input rides a 2 ms poll over a masked, already-acked interrupt (timer-crutch audit A1, 2026-06-09)
Keystroke forwarding is a poll cadence end to end: `usb.kbd` sleeps `POLL_PACE_NS` =
2 ms between empty `usb::read` polls (guest/stubs/usb-kbd/src/lib.rs:43,193-198) and
the shared OHCI core masks ALL controller interrupts at bring-up ("the polled
driver's suppression discipline", crates/eo9-ohci/src/driver.rs:243-244) — while the
done-queue path already takes and ACKS WritebackDoneHead (driver.rs:540-553), so the
event exists and is thrown away. Per the event-driven-liveness doctrine this is a
class-A crutch. Cost: ≤2 ms added key latency (minor) but the always-on station kbd
service wakes the core every 2 ms forever — the 1 s idle backstop is unreachable and
the station config can never idle. Fix shape: unmask INT_WDH (+RHSC for the
port-connect watch, today a 50 ms sweep) behind a wait surface; `usb::read` = arm TD,
await interrupt, drain; keep the short poll as the fallback where interrupt waits
answer `unsupported`. QEMU leg reachable TODAY (`usb.ohci-pci` rides eo9:pci, whose
enable-interrupts/wait is proven by disk.virtio) — size M. Board leg blocked on
eo9:platform interrupt routing (usb-ohci-plan risk 7, GIC SPIs 216/219) — size L.
Full inventory: docs/study/timer-crutch-audit.md.

## Net receive rides the RX interrupt on QEMU; the board leg (rtl8125) still polls — rk3588 PCIe INTx is the blocker (timer-crutch audit A2 / plan/12 D59; QEMU leg FIXED 2026-06-09, area/36)
THE QEMU LEG IS FIXED (area/36-net-rx-events): an idle telnetd/oskexec listener now
parks the whole executor instead of pegging a core. The landed shape, recorded here
so the board lane can mirror it exactly:
* `eo9:pci` `wait` takes a caller-stated `max-ns` bound, clamped by the provider to
  its own 2 s cap (`INTX_WAIT_BOUND_NS`) — the deadline crosses the driver without
  the driver holding time (doctrine-clean: IntxWait is a device capability).
* `eo9:net/l2` gains `wait-recv(iface, max-wait-ns)` — advisory "park until receive
  work is plausible or the bound passes"; providers without an event source return
  immediately (the documented poll fallback, today's behavior exactly).
* `net.virtio` finds the ISR window, takes one INTx vector at bring-up, un-suppresses
  used-buffer interrupts on the RX ring only (TX stays suppressed — its completions
  are consumed inline), acks the ISR after each consumed receive, and `wait-recv`
  checks the used ring, then parks on `pci::wait`. A bound expiry with RX work
  already in the ring = a missed event → loud `liveness:` line (1st + every 16th),
  never a silent rescue.
* `net.l4.over-l2`'s `wait_until` parks on `wait-recv` after any pump round that
  moved no frame, bounded by min(operation window remaining, smoltcp `poll_delay`) —
  ARP/TCP retransmits and DHCP timers stay deadline-bounded timed obligations.
  `net.text`'s accept retry needs no change: each 4 s accept window is parked.
* Two executor crutches the parked stack then exposed, both fixed in the same lane:
  kernel `task.wait`/`task.runnable` self-woke every poll ("stay runnable so the
  loop keeps turning"), keeping every waiting parent permanently runnable — a fully
  parked telnetd still burned ~190k passes/s through the console's and telnetd's
  waits; replaced by a completion doorbell (the drive loop's own Running→Done
  transition and the kill paths ring parked waiters; waiters also register the
  idle waker, so Ctrl-C rides the input edge — verified 20 ms kill on a parked
  child). And `idle_wait`'s post-wfi blanket `wake_idle` re-polled every parked
  future on maintenance-rated wakes, which mis-rated the next pass runnable and
  false-fired the stranded-runnable detector ~1/s once the stack actually parked;
  the post-wfi path now delivers due events only (Event/Deadline wakes ring; a pure
  maintenance wake rings nothing and re-publishes the consumed unexpired deadline),
  so the detector stays a regression alarm. Measured: idle listener 92.6% → 0.7%
  host CPU, established idle session 83.5% → 0.6%, bare prompt 0.0%, hello
  round-trip over the session 121 → 40 ms, zero `liveness:` lines.
STILL OPEN — the board leg: `net.rtl8125`'s `wait-recv` deliberately returns
immediately (poll fallback; the driver's comment points here). The RTL8125 ISR
exists, but rk3588 PCIe INTx is unwired (`arch::pci_intx::WIRED = false`, deferral
recorded in rk3588_pcie.rs:93), so an idle listener on the board still busy-pumps —
likely the workload class behind the board's first stranded-runnable detector hit
(GAPS 2026-06-08). When the INTx plumbing lands, the board lane mirrors the
net.virtio shape verbatim (ring-check → bounded `pci::wait` → ISR ack → missed-event
finding); the l4/middleware side is already done and needs nothing. Also open,
recorded by area/36: `net.l2.switch`/`net.l2.bridge` ports answer `wait-recv`
immediately rather than forwarding to the uplink — forwarding would let one parked
port starve its sibling's every operation into the typed busy answer for the bound's
duration (the uplink is an exclusive slot); an event-driven park there needs a
cross-port wake (sibling drain → parked port). Idle multi-stack vNIC compositions
therefore still poll. Size S–M, after a guest-local waker shape exists.
Full inventory: docs/study/timer-crutch-audit.md.

## Usermode service drive loop carries a hard-coded 10 ms ambient park backstop (timer-crutch audit A3, 2026-06-09)
`drive_with_services` parks with `Duration::from_millis(10)`
(crates/eo9/src/run.rs:170-175) although every known wake source is already
registered (foreground doorbell, every parked service's doorbell, the earliest
restart deadline — providers.rs:1239-1276); the cap exists for "a wake source this
function does not know about", which is definitionally a crutch under the doctrine.
Cost: an idle `eo9 shell` with any detached service wakes 100×/s instead of ~1×/s.
The event-pure shape already exists in the same file: the foreground-only
`wait_until_runnable` parks indefinitely. Fix: mirror area/34's kernel cap
restructure — lengthen to detector-grade (~1 s) or delete the cap and trust the
registered wake set + the existing park-backstop liveness detector. Size S;
SEQUENCE AFTER area/34 lands so both executors keep one doctrine shape.
Full inventory: docs/study/timer-crutch-audit.md.

## USB HID events: QEMU leg is interrupt-driven; the board's polled reads are the v1 residue (area/37, fixes timer-crutch audit A1+A4 QEMU legs, 2026-06-09)
The discarded-event bug is fixed where the plumbing exists: `usb.ohci-pci` now asks
`pci::enable-interrupts` for one INTx vector at bring-up (the disk.virtio shape) and
the shared OHCI core unmasks exactly WDH+RHSC (+MIE, `Ohci::enable_events`) — `usb::read`
parks on the done-queue writeback instead of answering empty every 2 ms, and the new
`usb::watch-ports` parks on RHSC instead of 50/100 ms port sweeps. Ack discipline
unchanged: WDH is acked once, by `consume_done_queue` after taking HccaDoneHead; RHSC
is acked by the wait paths (port change bits stay readable for the sweeps). `usb.kbd`
and `hidcheck` skip their `POLL_PACE_NS` pacing when `usb::event-driven` answers true.
Measured (QEMU TCG, foreground `usb.kbd --window-ms 40000`, no typing, same build with
the shell's vector request toggled): 1904 read rounds per 40 s window polled — each a
multi-host-call drain through the composed component — vs 4 rounds event-driven (the
2 s wait-bound cadence): ~480× fewer wakes; with typing, wakes are per keystroke and
the gates' 250 ms QMP pacing decodes every transition, hub leg included. NOTE the idle
host-CPU% did NOT move (~83-100% polled, ~99% event-driven, both pegged-class): the
drive loop stays hot for other reasons — see the companion entry below. Liveness
doctrine wired, not just honored: a report found by the post-timeout drain prints a
rate-limited `liveness: usb.ohci-pci:` line (shell), and a connect found by a sweep
after a timed-out `watch-ports` prints `liveness: usb.kbd:` — the fallback rescues
loudly, never silently. REMAINING RESIDUE (the board leg, capability-gated not
cfg-gated): `usb.ohci` already calls `platform::enable-interrupts` and falls back
cleanly when the v1 kernel root answers `unsupported`, so the board keeps today's
2 ms read pace and 50 ms connect sweeps until platform interrupt routing lands
(usb-ohci-plan risk 7 / §6 design note: per-region GIC SPIs 216/219 + an IntxWait
mirror in platform_provider — kernel-side only, zero guest changes). The audit's A1
GAPS entry (area/35) should be folded into this one when both lanes merge.

## The kernel session never idles while any composition is alive — USB event wakes are fixed but the core still burns ~100% (area/37 measurement, 2026-06-09)
Found while proving the A1 fix's wake-rate claim: with the kbd path fully
event-driven (4 guest read rounds per 40 s, see the entry above), the station config
STILL pegs the host vCPU at ~99% (10-sample ps means; before the fix it measured
~83% with dips to ~42%), and a foreground `usb.kbd` window pegs ~99% as well. PC
sampling via QMP (30 × `info registers` at idle) lands almost every sample inside
`wasm::svc::drive_services`+0x28 with the rest in `IntxWait::poll` — the drive loop
is executing passes back-to-back, not parked in `wfi`. Two suspects, both inventoried
by the audit and owned by area/34's executor lane, now with a measurement attached:
(1) the exec surface's `task.wait` host call rings its own waker on every pending
poll ("stay runnable so that loop keeps turning", wasm/shellexec.rs:2555) — any eosh
waiting on a foreground child is permanently runnable, so the session loop takes the
hot branch (`wake_idle`, no `idle_wait`) for the child's whole lifetime; (2) even
with nothing runnable (station at the bare prompt + one parked service), the
10 ms `IDLE_WAKE_INTERVAL_NS` child-running cap plus the apparent cost of re-polling
parked component instances keeps the duty cycle saturated under TCG (the parked
IntxWait's 2 s bound was observed expiring at ~10 s granularity — re-polls are that
far apart while the core is 100% busy, i.e. each pass is wall-clock expensive).
Consequence for the doctrine: fixing the device-driver crutches (A1 here, A2 next)
is necessary but NOT sufficient for an idle station — the executor lane must land
before the idle-CPU win is observable. Numbers and probes: area/37's lane report.
UPDATE (review train, 2026-06-09): area/34 has since merged — the blanket hot-pass
wake and the 10 ms/1 s caps are gone, and the reviewer measured station idle at
~12% TCG (plain 0.0%), so suspect (2) is resolved; the `task.wait` self-wake half
(suspect 1, the foreground-composition spin) STANDS and remains the area/35 class-A
usermode/kernel follow-up this entry's PC samples corroborate.

## usb `watch-ports` has no gate exercising it (area/37 review, 2026-06-09)
Every gate boots with the keyboard already attached, so usb.kbd's first sweep finds
the device and `watch-ports` (the RHSC park) never runs end to end — it is covered
by the eo9-ohci mock tests only. The cheap gate arm when a lane wants it: boot the
check-usb-hub topology WITHOUT `-device usb-kbd`, start `usb.ohci-pci $ usb.kbd`
(it enters the watch), then QMP `device_add usb-kbd,bus=eo9ohci.0,port=1.1` and
assert the attach banner + a forwarded keystroke — proving RHSC delivery, the
Changed arm, and the liveness line's absence on the event path.

## usb event-mode wait errors map to TimedOut — a latent hot-spin if a vector can ever die mid-task (area/37 review, 2026-06-09)
Both OHCI shells map any `pci::wait`/`platform::wait` error to `TimedOut` (the
polled-fallback round). Sound today: interrupt handles are task-scoped, so no live
task can hold a dead vector, and a real bound expiry takes the full bound. But if a
revocable-vector state is ever introduced (per-device interrupt teardown, hot
unplug), an INSTANTLY-erroring wait plus usb.kbd's cached `event-driven` answer
(pacing dropped) becomes a hot loop: drivers hold no time capability and cannot
self-pace. The degradation design when that day comes: after N consecutive wait
errors the shell drops its vector (waits answer `Unsupported`) and flips the
endpoint's `event-driven` answer false; the consumer re-queries `event-driven` on
empty reads and resumes its pacing. Comments at both Err arms point here.

## INTx deliveries while the core is awake raised no wake — bridged with the INTx edge until area/36 (review train, 2026-06-09)
The wfi-wake argument covers a sleeping core only: an INTx taken MID-PASS records its
delivery (handler masks + counts) and nothing rings the parked waiter, so the
completion rides the wait's own deadline. Invisible pre-area/37 (the 2 ms USB poll
pace rescued it silently — the audit's silent-rescuer shape); event-driven USB reads
surfaced it as exactly one HID report per 2 s wait bound (check-usb-hub deterministic
failure, forwarded(5)). Bridge landed: an INTx-arrival edge (set in intx_record,
cleared by wake_idle) checked at the hot path, the pre-park gate, and the IRQ-masked
re-check — the same pattern as the console input edge, second producer class.
area/36-net-rx-events' wake plumbing complements (NOT subsumes) the edge: its
doorbell and due-event delivery cover completion and wfi-window wakes, while the
awake-window INTx race delivery remains this edge's job — the merged executor checks
it at the hot path, both park gates, the masked re-check, and the post-wfi arm. Same hunt also fixed an OHCI done-queue race: a writeback consumed by the
spurious-wake drain left the next counted reap waiting forever (silent freeze or
device-lost under TCG) — now credit-accounted with a frame-bounded spin, pinned by
a mock regression test.
