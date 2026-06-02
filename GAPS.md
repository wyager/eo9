# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-06-01 (master at acb6c5e, after the round-3 user studies and their fix wave: five
personas — storage, network, driver, returning novice, distribution — produced 96 findings; 37 are fixed
and merged, 41 are tracked below, 18 await owner decisions. The wave also closed the bind-entrypoint
decision (resource-owning providers are configurable), shipped PCI INTx interrupt delivery, the writable
MAC-verified /bin on metal, and the typed configure-error channel.)_

## Decisions pending with the owner

The round-3 studies produced a batch of genuine design questions (the per-study triage tables in
docs/user-studies/00-synthesis.md cite the evidence). Grouped by theme:

**Storage (study 07):**
- **Rollback policy** (S7-1): when the newest uberblock is bad, mount now *warns* and falls back — should
  it instead refuse and demand explicit operator recovery? (Warn-and-mount vs operator-gated rewind.)
- **Auto-format existence** (S7-2): foreign images are now refused, but a genuinely all-zero device still
  auto-formats on first mount. The participant's stronger ask: remove auto-format entirely; formatting is
  always explicit (`mkfs.eofs`).
- **Uberblock geometry** (S7-9): 2 adjacent slots in the first 8 KiB — one torn 8 KiB write kills the
  volume. Format change; decide before any format-stability promise.
- **`rename`/atomic-replace in `eo9:fs`** (S7-12): "on a CoW filesystem, atomic replace should be the easy
  path." WIT addition.
- **Operator-side safety in the threat model** (S7-19): the capability model protects programs from each
  other; nothing frames what protects the operator's *data* from the operator's own grants. SPEC paragraph.

**Networking (study 08):**
- **Wait policy / deadlines in the l4 WIT** (F8): caller-supplied deadlines vs provider constants — the
  participant urges deciding "now, while each interface has exactly one implementation."
- **Multi-program networking** (F9): when two networked programs share one NIC — per-program network
  identity (own MAC/IP per program) vs a shared-stack provider. Must be designed before multi-programming.
- **`io(string)` catch-alls** (F15): keep as the pragmatic escape hatch vs enumerate further typed cases.

**Drivers (study 09):**
- **IOMMU** (#7): adopt the participant's QEMU `virt,iommu=smmuv3` + per-device DMA domains plan, or defer
  to real-board work — and soften the SPEC/README containment wording until one of those happens.
- **Device matching for `pci.filtered`** (C2): allow-lists are address-keyed and fragile across boot
  configs; should filtering also/instead match vendor:device?
- **Long-running drivers** (C3): where does a daemon-like driver live? Tied to the Message API /
  supervision design.

**Website voice (study 10):**
- **Authoring statement** (#15): one paragraph on the front page ("programs are Rust today; the format is
  language-neutral") — the owner's prose and roadmap commitment.
- **Front-page jargon** (#16): "language-theoretic principles" is still the round-1 filter sentence; the
  try-it page redeems it for anyone who clicks through. Owner's voice either way.
- **Browser boot banner** (#9): humanize the first line vs keep the full provenance text.
- **Browser `save` vs `ls /bin`** (#4): `save` claims a path the per-run fs snapshot can't see — make the
  session overlay include shell-store writes, or reword `save`'s message. Touches the session design.

**Release/distribution (study 11):**
- **Hosted CI** (D17): the participant's #1 ship blocker — a GitHub Actions (or equivalent) decision, plus
  Linux evidence (today only macOS is demonstrated).
- **Publish automation** (D18): the 8-step manual sequence vs an `xtask publish` with tags/changelog.
- **Publish rehearsal** (D19): the `eo9` crate cannot be end-to-end tested before the real publish — run a
  local-registry rehearsal or accept the risk.
- **Crate naming permanence** (D24): `eofs-core` sits outside the `eo9-` namespace; names are the one
  irreversible decision. Review before publishing.
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
  CI-gated); real-board bring-up happens when the owner has hardware.
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
  the suspended-subtask path **is now exercised end-to-end on the storage chain** — `fs.eofs` and
  `disk.virtio` genuinely await, and study 09's `pci.admit-address $ pci.filtered $ disk.virtio $
  fs.eofs` runs on metal with INTx completion through the filter (plan/14 D25/D26, plan/09 D33); the
  net lane converts on `area/09-net-async`; cancellation of an in-flight forwarded call still traps
  (the `area/04-async-hardening` matrix owns it — the storage providers already carry drop-guards so a
  cancelled operation can't wedge their state slots); variant/result/flags/handle-typed configure
  values still refuse (with clear messages); the unbakeable-shape refusal is reported under the
  `Internal` error variant (cosmetic tidy-up queued).
- Kernel algebra errors map to `Internal(String)` rather than the specific WIT variants; the kernel renders
  `wiring` as a leaf only; eosh `envinfo` still classifies authority by the `/types`-name heuristic.

### Runtime / providers (usermode)
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
  hard-coded rather than configurable, no throughput/pps instrumentation, and identical fused compositions
  never hit the metal compile cache (bumped: this blocks all metal networking iteration). Capture-based
  (pcap-level) test assertions are a recorded area-13 item. (study 08 F3b/F6/F7/F10–F14/F17)
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
  its QEMU arm doesn't wire the `pci` grant yet**. INTx interrupt delivery is live on both GIC and PLIC and
  `disk.virtio` waits on interrupts (`9d048e5`); **`net.virtio` is still polled** (same conversion recipe,
  recorded). Devices are quiesced before DMA free on every teardown path (`02460f9`). Still open from the
  driver study: machine-global device claiming (the `storedisk` vs `pci`/`disk` don't-combine rule stands),
  the **nested-forwarding bug** (`pci.filtered $ disk.virtio $ fs.eofs $ …` fails at first I/O — promoted
  from caveat to reproduced bug, study 09 demo 3c is the regression test), grant checks only after a 30 s
  compile, MSI/MSI-X / FLR / hot-plug / AER unimplemented, no fault-injection surface (a `pci.fault`
  middleware idea is recorded), and the async-API-vs-eager-reality queue-depth-1 limitation.
- **Storage**: the `storedisk` compile cache and the **writable MAC-verified /bin** (`00d1eb2`: `save` at
  the metal prompt survives power cycles) are both live; cache eviction/space management,
  VIRTIO_BLK_F_FLUSH-on-commit, riscv64/x86_64 storedisk enablement, and disk-vs-baked provenance in
  `ls /bin` are queued. (plan/12 D58/D60) From the storage study: no fsck/scrub/df surface (engine
  `verify()` is unreachable), no full-stack crash-injection harness, scale untested (large files/dirs/
  images), no fuzzing of the on-disk parser, the mount banner counts artifacts without verifying them, and
  metal `env` omits the pci/disk grants. (study 07 S7-8/10/15/20/21/22)
- **Kernel hardening residuals**: kernel-image-internal W^X (`.text`/`.rodata`/`.data` split) and guard
  regions; exceptions other than IRQs are fatal; the idle waker is single-slot; nested shells share the one
  serial console.
- **Diagnostics/runner gaps**: the headless `program=` runner ignores `program=eosh` and does not carry the
  guest panic message (the interactive path does); on-target codegen determinism is not bit-compared;
  no instrumentation for peak compile heap / phase timings / cache-hit reasons. (plan/12)
- **Scripted-console conventions** (not kernel-input bugs): on riscv64, OpenSBI consumes a byte that
  arrives before the kernel exists — wait for the prompt; on every arch a full-speed pasted line can outrun
  the UART model — pace scripted input. (plan/12 D49)
- **Wasmtime version bumps are not free**: re-verify the binder/executor ABI-constant blocks and re-AOT all
  artifacts on any bump off 45.
- Real-board bring-up is unscheduled (waiting on hardware); the QEMU test tier is still scripted/manual
  rather than part of `cargo xtask ci`.

### Website / in-browser demo
- **Blob reproducibility**: same-path rebuilds are byte-identical, but a build from a *different* checkout
  directory still differs by ~410 bytes of cargo unit-metadata for the `[patch]` path deps — full
  cross-machine reproducibility needs cargo-side or workspace-restructuring work. (plan/18 D26)
- **Asset churn**: every guest-SDK change re-fingerprints all `/vm` store assets (~11 MB of binary churn
  per such merge); if repository weight becomes a problem, move the committed web assets out of git.
- **Performance honesty**: browser programs are Pulley-interpreted — noticeably slower than native/metal
  for compute-heavy runs (the page says so).
- **Remaining polish**: a click-through on the live deployed site (after the owner's next redeploy);
  lazy-fetching `/bin` pairs to trim the ~8.85 MiB raw blob; hash-named vm.js/vm.css (optional — they are
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
  synthesis), 41 tracked (folded into the sections above and below), 18 owner decisions (listed at the
  top of this file). The fix wave's own reviews surfaced two more items, both handled: a session-lock
  ordering race (fix in flight) and the fs-eofs checkout-dependence that keeps the bundle drift check out
  of `cargo xtask ci` (plan/01 D15).

### Round-3 distribution/release residuals (study 11)

- **The bundle drift check is not in CI** (D9b): it went in (`be91552`) and came back out (`23699a9`)
  because fs-eofs builds are not checkout-independent — cargo bakes the out-of-workspace `eofs-core` path
  dep's manifest path into `-C metadata` symbol hashes, which `--remap-path-prefix` cannot fix. Candidate
  fixes in plan/01 D15 (guest-workspace membership for eofs-core / comparison normalization / pinned
  metadata). The check remains in `cargo xtask package`.
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
- The owner pushes master to GitHub (github.com:wyager/eo9); planner-side agents never push.
