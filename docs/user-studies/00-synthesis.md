# User studies — cross-session synthesis and finding triage (2026-05-27)

Three sessions: 01 CLI developer, 02 security engineer, 03 embedded/OS engineer (full reports in this
directory). Every finding below is dispositioned **Fix now**, **Tracked** (GAPS/roadmap with a next step),
or **Owner decision**. Nothing is dropped.

## Convergent findings (raised by 2–3 personas independently)

| # | Finding | Disposition |
|---|---|---|
| 1 | **Unconfigured configurable providers trap at runtime** ("provider used before configure"): `time.frozen $ hello`, `entropy.seeded $ rng`, `fs.memfs $ anything` panic mid-run with a raw backtrace, in userspace and on metal. CLI persona: "if it composed, it runs or fails typed." | **Owner decision pending** (defaults vs refuse-before-run vs hybrid); fix dispatched immediately after the call. |
| 2 | **README/docs don't run as written**: package-level `only eo9:text,eo9:time` rejected (full interface refs required); `entropy.seeded $ cruncher` is a no-op (cruncher imports nothing); fresh-store `eo9 hello` fails (only the shell path seeds); error text in docs doesn't match the raw output. | **Fix now** — the README verification pass (queued behind the overlay/child-caps re-land) rewrites every example against real output; seeding on any first run (not just shell) included. |
| 3 | **Raw internal error strings** leak to users: `RestrictError::RequiredOutsideAllowList([...])`, `SpawnError::Internal(...linker...)`, raw `eo9:io/buffers` linker error for fs-needing children (metal), friendly message exists for missing fs but not missing exec. | **Fix now** — error-rendering pass in eosh/CLI (+ kernel shell). |
| 4 | **Outcome line / exit-code ergonomics**: the typed outcome goes to stdout (pollutes pipes), no `--quiet`/porcelain mode, `success(…)` vs `ok: …` differ between front doors, and the 0/1/2/3 contract collapses to exit 1 + debug string one shell layer down (`-c`). | **Fix now** (planner default: program stdout stays stdout; outcome line moves to stderr with a flag to re-enable/JSON it; unify rendering; honest `-c` exit codes). Owner may veto the stderr choice. |
| 5 | **Debugging story**: guest SDK panic handler discards the panic message; no source lines in backtraces; `--debug-info` has no observable effect and reuses the cached image (cache key ignores it — bug); kernel exception dump unsymbolized; no documented debugger workflow. | **Fix now**: preserve panic messages; include `--debug-info` in the cache key. **Tracked**: source-line backtraces, debugger workflow (new GAPS items). |
| 6 | **CPU is the weakest limit**: a zero-import busy loop spins until Ctrl-C (no `--max-fuel` in the CLI), and on metal one looping child takes the machine (no preemption/fuel yet) — the embedded persona's #1 blocker. | **Fix now**: `--max-fuel` / session fuel ceiling in usermode. **Tracked/roadmap**: child fuel + eo9-sched on metal moves up to the next kernel milestone (owner to confirm ordering). |

## Single-persona findings

| # | Finding (persona) | Disposition |
|---|---|---|
| 7 | `describe` cannot show interposed attenuators (`fs.readonly $ cat` looks like `cat`) — wants a wiring/layer view for audit (02) | **Owner decision** (new inspection surface). |
| 8 | Children's silent default grant (incl. entropy) read as contradicting the explicit-authority pitch; suggested printing the grant at spawn or making entropy opt-in (03) | **Tracked** — fold grant-visibility into the overlay/child-caps re-land (env already shows it; add spawn-time visibility); entropy-opt-in is an owner call if wanted. |
| 9 | TOCTOU window: canonicalize-then-open with no fd re-verification; interim ask = re-verify the opened fd until openat2-style resolution lands (02) | **Fix now** (small hardening in eo9-providers-unix). |
| 10 | Symlink-target-existence oracle (Denied vs NotFound distinguishes whether an outside target exists) (02) | **Tracked** (minor; align the two errors). |
| 11 | Store/cache integrity is blake3 but unauthenticated — no signing/provenance (02) | **Tracked** (signed stores, post-MVP item made explicit). |
| 12 | Hostile-component test suite + fuzzing of the fs provider and ABI boundary wanted in CI (02) | **Tracked** (test-suite work item, area 13). |
| 13 | Writable storage on metal + a fused-artifact cache; identical composition re-run was not faster (cache not hitting for fused artifacts — investigate) (03) | **Tracked** (eofs-on-metal milestone; cache-key investigation is a fix-now bug check). |
| 14 | Real-board bring-up should jump ahead of riscv64/x86_64 QEMU ports; "runs on bare metal" overclaims while only QEMU virt is supported (03) | **Owner decision** (roadmap ordering). |
| 15 | Footprint/instrumentation: peak heap during on-target compile unknown; wants compose/compile/run timing split + cache-hit reasons (03) | **Tracked** (instrumentation work item). |
| 16 | On-target codegen ~25–35% slower than host AOT on the cruncher microbenchmark; opt-level parity unverified (03) | **Tracked** (verify settings parity). |
| 17 | Authoring friction: no `eo9 new` scaffold, no per-package guest build, option-typed args still required (existing WAVE-binder gap), no defaults (01) | **Tracked** (`eo9 new` + per-package build are good next usermode items; optional-args gap bumped). |
| 18 | Metal `env` text still claims composition/codegen "not available yet" right after a composition succeeded (01, 03) | **Fix now** (kernel session-manifest text). |
| 19 | fs.memfs cannot serve a single operation through the shell (combination of #1 and the resource-owning-configure limitation) (01, 02) | Resolves with #1's fix (defaults make fs.memfs need no configure); noted under the parked binder decision. |
| 20 | Package-level `only eo9:text` shorthand: should it be accepted (expanding to the package's interfaces) instead of requiring `eo9:text/text`? (01, 02, 03 all tripped on it) | **Owner decision** (algebra UX); README will use the full form either way until decided. |

## What landed well (keep doing)

Pre-execution refusals naming exact imports; attenuation-by-composition in the program's own typed
vocabulary; `describe`/`env` inspection; determinism via seeded/frozen providers; store/cache tamper
detection; trap containment; the compose→fuse→compile-on-target→run loop on metal with bit-identical
results; boot-to-prompt speed; performance generally a non-issue. Across all three sessions, trust losses
came from documentation overclaim and off-happy-path rough edges — never from the core model or speed.

---

# Round 2 (sessions 04–06, 2026-05-27) — triage

Sessions: 04 web-platform developer (eo9.org, /try, /vm), 05 PL/type-systems researcher (spec + algebra),
06 novice developer (getting-started). Same disposition rule: every finding is **Fix now**, **Tracked**
(GAPS/roadmap), or **Owner decision**; nothing is dropped.

## Round-1 status update

Now FIXED on master: the unconfigured-provider trap (#1 — documented defaults, never-trap rule), the
README-doesn't-run items (#2 — verified install order, full interface refs, real outputs), raw error
strings in the main paths (#3), outcome-line-to-stderr + `--max-fuel` + fresh-store seeding (#4, #6),
the kernel env text (#18), the TOCTOU interim fd re-verification (#9), `describe` attenuator visibility
(#7 — `describe --wiring` shows the full composition tree), `only` package shorthand (#8/#20 — `only eo9:text`
admits the package), child-grant spawn-time visibility (#8 — under `-v`), and the `--debug-info` cache-key
claim was investigated and found already correct (#5, closed). Metal scheduling (#6) is FIXED (child fuel +
preemption — a looping child no longer takes the machine). Still open from round 1: panic-MESSAGE
preservation and the debugger story (#5 — the readable backtrace shipped; the message needs an
`eo9:rt/diagnostics` export), entropy-opt-in (#8 — decided no-op), signed stores (#11), hostile-component
suite (#12), real-board ordering (#14), instrumentation (#15), codegen parity (#16), authoring friction
(#17), writable storage on metal (#13).

## Round-2 findings

| # | Finding (session) | Disposition |
|---|---|---|
| R2-1 | Configured guest middleware over a configured guest provider traps (`time.frozen --… $ time.fuzzy --… $ hello`, and `&` form) — override-law counterexample, shape missing from the suite (05) | **Fix now** (algebra wave). |
| R2-2 | `fs.none $ <fs-consumer>` fails encode/validation instead of dropping the unmatched export (no-op-drop law violation) (05) | **Fix now** (algebra wave). |
| R2-3 | `rename` on a residual import yields an invalid artifact (codegen rejects the import name) (05) | **Fix now** (algebra wave). |
| R2-4 | The laws' `≡`, instance identity/sharing under composition, and the `empty` element are unspecified (05) | **Fix now** (SPEC clarification) + **Tracked** (tests). |
| R2-5 | Generative property suite over component triples requested — would have caught R2-1..3 (05) | **Tracked** (area 13 work item, high priority). |
| R2-6 | The spec-promised "exports match nothing" warning never fires (05) | **Fix now** (algebra wave). |
| R2-7 | `describe` hides interposed attenuators (reconfirmed) (05) | **Owner decision** (round-1 #7). |
| R2-8 | Zero-cost-layer and artifact-identity claims unevidenced (no identity-middleware benchmark, no user-facing digest) (05) | **Tracked** (benchmark or soften; expose composition digest). |
| R2-9 | No response compression anywhere; /vm blob 1.21 MiB raw vs ~290 KB brotli (04) | **Fix now** (web hardening). |
| R2-10 | No security headers (CSP, HSTS, XCTO, COOP/COEP) (04) | **Fix now** (web hardening). |
| R2-11 | Caching: max-age only, no ETag/fingerprinted assets → stale-blob window after deploys (04) | **Fix now** (web hardening). |
| R2-12 | /try ships ~570 KB of ~90% duplicated jco glue (04) | **Tracked** (split shared intrinsics + minify). |
| R2-13 | Two missing disclosure sentences: /try's refusal is launcher JS; /vm's components import nothing yet (04) | **Fix now** (one sentence each). |
| R2-14 | /vm determinism claim is self-asserted by the blob; bare-metal leg unverified (04) | **Tracked** (point at the native cross-check; verify or soften). |
| R2-15 | vm.js error path hard-codes one cause; no instantiateStreaming fallback (04) | **Fix now** (web hardening). |
| R2-16 | README getting-started order broken on a fresh checkout (install before build-guest) (06) | **FIXED** (README pass: build-guest → install --force). |
| R2-17 | New guest crates silently ignored unless added to `GUEST_COMPONENTS`; failure surfaces later as a confusing store error (06) | **Tracked** (auto-pickup or loud warning; `eo9 new` scaffold). |
| R2-18 | Error-quality inconsistency: `fs("FsError::…")` debug text, NotFound for a visible read-only /bin file, double-printed shell refusals, exit 1 vs 3 across front doors, `eo9 store --help` errors (06) | **Fix now** (next error-rendering pass) / **Owner decision** for the `-c` exit-code unification. |
| R2-19 | Outcome line glues onto program output without a trailing newline (06) | **Fix now** (small). |
| R2-20 | `/bin`/`session` entries appear in `ls` of a `--fs-root` session and surprise users (06) | **Tracked** (presentation; document or filter). |
| R2-21 | Vocabulary is the on-ramp blocker; participant supplied a 7-step beginner-tutorial outline (06) | **Tracked** (tutorial/getting-started doc). |
| R2-22 | STATUS/GAPS lagged reality (described /vm as deferred after it shipped) (04) | **FIXED** (this refresh); keep docs current per merge. |

## Round-2 status update (post-overnight-batch, master 4962464)

Now FIXED on master: **R2-1 (configured-middleware trap)** — closed by making `configure` synchronous
(`af9cb34`), not the event-driven binder; **R2-5 (generative property suite)** — landed (`26ddc28`); **R2-7
(`describe` attenuator view)** — `describe --wiring` shipped (`00bfaf7`); plus the earlier R2-2 (`fs.none`
drop-law), R2-3 (`rename`-residual via `executable_bytes`), R2-9 (compression), R2-10 (security headers),
R2-11 (caching), R2-13 (disclosures), R2-15 (vm.js error path), R2-16/R2-22. Still open: R2-4 (`≡`/identity
in SPEC), R2-6 (surface the "exports match nothing" warning host-side), R2-8 (zero-cost-layer benchmark /
composition digest), R2-12 (/try jco dedup), R2-14 (/vm determinism cross-check), R2-17 (guest auto-pickup /
`eo9 new`), R2-18 (error-quality + honest `-c` exit codes — needs an eosh `program-failure` WIT class),
R2-19 (outcome-line newline), R2-20 (`/bin` in `ls`), R2-21 (beginner tutorial). Batch follow-up: the wasm32
blob build is path-dependent (different checkout dir → different hash) — wants a reproducible-build fix
before cross-machine CI.

## What landed well in round 2

The /vm page running the real stack with native-matching entropy and fuel parity; the site's restraint
(small front page, no third-party JS); sealing, `only` position semantics, the action law on stateful
providers, and determinism-by-substitution surviving adversarial probing; the seeded-RNG and frozen-clock
demos as the moment the model "clicked" for the novice; refusal-before-run naming exact imports.

# Round 3 (sessions 07–11, 2026-05-31) — triage

Five personas over the surface that grew since round 2: persistence (eofs/`--disk`/`storedisk`), the
layered network stack, the PCI/driver substrate, the redesigned two-page website, and the cargo-install
packaging chain. Every demo was real (release builds, QEMU boots on up to three architectures, packet
captures, the served site); every participant was context-free and tool-less.

**The meta-finding: the round paid for itself.** Internal testing and reviewer passes had missed — and these
five sessions found — three real data-loss bugs (silent transaction rollback, auto-formatting foreign
images, failed rewrites destroying old content), a kernel memory-safety hole (devices left bus-mastering
into freed heap), a contract violation (configure refusals trapping), a release blocker (the publish
pre-flight was red on master), and a 6.7-second wire-visible stall. All of the above are now fixed and
merged (see the per-study tables below).

## The personas and their verdicts

| # | Persona | Focus | Verdict (their words) |
|---|---|---|---|
| 07 | Storage/database engineer | eofs, durability, the disk stack | "A correct core with no operational armor … for a storage system, the boring parts are the product." Would not store real data yet; called the Merkle blast-radius, tamper-recovery lifecycle, and cross-arch portability "genuinely impressive." |
| 08 | Network/systems engineer | l2/l3/l4, drivers, sockets-on-metal | "Not yet a networking system you'd deploy. It's a networking architecture you can finally test" — would use it today as a deterministic network-test harness. |
| 09 | Driver/firmware developer | eo9:pci, writing a wasm driver, containment | "Most 'capability OS' papers never get this far." The no-quiesce + no-IOMMU combination was "disqualifying as shipped" (now fixed/decision-pending respectively). |
| 10 | Returning novice | The website + shell discoverability | "It wasn't me. They made it make sense." Bounced off round 1 in two minutes; ended round 3 planning a weekend install. |
| 11 | Devtools/distribution engineer | cargo install, Makefile/doctor, publish chain | "The best 'ship a runtime plus its payload through a source registry' design I've seen" — but "no-ship this week, yes-ship in ~2 weeks" (blockers now fixed or owner-pending). |

## Consolidated triage

96 findings across the five studies. Final dispositions after the fix wave (merges
`cb410c2` eofs-integrity, `02460f9` pci-teardown, `9b2bc10` net-fixes, `c1bd95c` bind-error-channel,
`9fee776` ux-fixes, `be91552` dist-fixes): **37 fixed, 41 tracked (GAPS), 18 owner decisions** (some
findings split across categories — the split is noted per row). Nothing dropped.

### Study 07 — storage engineer (22 findings)

| # | Finding | Final disposition |
|---|---|---|
| S7-1 | Silent txg rollback on newest-uberblock corruption | **Fixed** (`cb410c2`: loud mount-time warning, corrupt≠blank classification) + **owner decision** pending on the rewind *policy* (warn-and-mount vs refuse-and-recover) |
| S7-2 | `--disk`/mkfs auto-format any non-eofs file | **Fixed** (`cb410c2`: blank = all-zero 64 KiB; foreign images refused, mkfs needs `--force`) + **owner decision** pending on removing auto-format entirely |
| S7-3 | Images brick when CoW garbage exhausts space | **Fixed** (`cb410c2`: gc on NoSpace + retry; `rm` frees space) + **tracked**: df/scrub/eviction surface |
| S7-4 | Failed rewrite destroys previous content | **Fixed** (`cb410c2`: truncate+write is one transaction, failures roll back) |
| S7-5 | Corruption errors flattened to `Io(string)` | **Fixed** (`cb410c2`: "integrity check failed:" marker) + **tracked**: the `eo9:fs` WIT integrity variant (plan/14 D24) |
| S7-6 | Concurrent `eo9 -c` corrupts the session store | **Fixed** (`cb410c2`: per-process sessions) — note: the fix introduced a lock-ordering race found in review; a follow-up fix is in flight |
| S7-7 | No locking on `--disk` images | **Fixed** (`cb410c2`: flock, exclusive RW/shared RO) + **tracked**: format-level multi-mount protection |
| S7-8 | No fsck/scrub/verify/df surface | **Tracked** (GAPS: storage operational surface) |
| S7-9 | Uberblock geometry (2 adjacent slots, first 8 KiB) | **Owner decision** (on-disk format change) |
| S7-10 | Corruption-detection vs crash-consistency conflation; no full-stack fault injection | **Tracked** (GAPS: crash-injection harness) |
| S7-11 | `readwrite` example teaches truncate-then-write | **Fixed** (`cb410c2`: docs note + safe pattern) |
| S7-12 | No rename/atomic-replace in `eo9:fs` | **Owner decision** (WIT addition) |
| S7-13 | Missing-`--disk` refusal buries the remedy | **Fixed** (`cb410c2`: remedy-first spawn refusals) |
| S7-14 | README has no persistence story | **Fixed** (`cb410c2`: verified Persistence section) |
| S7-15 | Metal `env` omits pci/disk grants, claims no writable fs | **Tracked** (kernel session-manifest wording) |
| S7-16 | Compile cache and guest disk mutually exclusive per boot | **Tracked** (existing: machine-global device claiming) |
| S7-17 | `ls /bin` baked vs disk-saved undifferentiated | **Tracked** (existing: plan/12 D60) |
| S7-18 | Outcome line glues onto unterminated output (R2-19) | **Tracked** (carried fix-now from round 2, still open) |
| S7-19 | Operator-side hazard outside the capability threat model | **Owner decision** (SPEC paragraph on operator-side safety) |
| S7-20 | Mount banner counts artifacts without verifying them | **Tracked** (fold into the scrub work) |
| S7-21 | Scale untested (large files/dirs/images) | **Tracked** (test-suite work item) |
| S7-22 | No fuzzing of the on-disk parser | **Tracked** (extends the round-1 fuzzing item) |

### Study 08 — network engineer (18 findings)

| # | Finding | Final disposition |
|---|---|---|
| F1 | Malformed configure address traps (contract violation) | **Fixed** (`c1bd95c`: bind returns `result<_, string>`; typed pre-run refusal on all three targets) |
| F2 | ~6.7 s ARP stall (driver spin × pump batch) | **Fixed** (`9b2bc10`: 2M→2k poll bound, empty-result semantics; TCP probe now completes inside its deadline) |
| F3 | Wire-visible TCP RST reported as `timed-out` | **Fixed** (`9b2bc10`: wire truth beats the clock) + **tracked**: mock-fidelity conformance test |
| F4 | Net stubs missing from kernel store; STATUS overclaim | **Fixed** (`9b2bc10`: 8 stubs + sockcheck baked; STATUS corrected) |
| F5 | "Tools must tell the truth" pass (refusal wording, env honesty, only wording) | **Partially fixed** (`cb410c2`: remedy-first refusals; `9fee776`: rt/diagnostics annotation) + **tracked**: env stub recommendations, metal env grants |
| F6 | Identical fused compositions never hit the metal compile cache | **Tracked** (bumped: blocks all metal networking iteration) |
| F7 | No background pump; stack frozen between l4 calls | **Tracked** (scheduler adoption + document the semantics) |
| F8 | Wait policy/deadlines absent from the net WIT | **Owner decision** ("decide while each interface has exactly one implementation") |
| F9 | Multi-program networking undesigned (NIC sharing) | **Owner decision** (per-program identity vs shared stack) |
| F10 | No DHCP / runtime-learned addressing | **Tracked** (DHCP-in-middleware as the existence proof) |
| F11 | IPv6 is types-only; docs imply more | **Tracked** (wire v6 or state IPv4-only) |
| F12 | No throughput/pps numbers | **Tracked** (instrumentation, after F2) |
| F13 | l3 has no provider and no consumer | **Tracked** (existing; "admit it's speculative") |
| F14 | Socket/buffer constants hard-coded | **Tracked** (expose via configure) |
| F15 | `io(string)` catch-all in every net error | **Owner decision** ("where typed errors go to die") |
| F16 | Serial input dropped after heavy compiles | **Tracked** (existing plan/12 D49 + compile correlation) |
| F17 | Capture-based (pcap-level) test assertions | **Tracked** (area-13 work item) |
| F18 | STATUS/GAPS lag the bind-entrypoint landing | **Fixed** (this synthesis + the dist/net STATUS corrections) |

### Study 09 — driver developer (15 findings)

| # | Finding | Final disposition |
|---|---|---|
| 6 | No device quiesce on teardown (DMA into freed heap) | **Fixed** (`02460f9`: disarm + mask before any DMA free, all teardown paths, both architectures) |
| 1 | `pci.filtered` under the storage stack fails (nested forwarding suspends) | **Tracked** (GAPS: promoted from caveat to reproduced bug; demo 3c is the regression test) |
| 2 | Disk→fs boundary swallows driver error messages | **Fixed** (`02460f9`: `DeviceError::IoNamed`, text reaches the shell) |
| 3 | "Device too small" masks "no device visible" | **Fixed** (`02460f9`: remediation-naming probe error) |
| 4 | `pci.deny`/`pci.none` not in the kernel store | **Fixed** (`02460f9`: pci.none; `be91552`: the pci.deny stub created + baked) |
| 5 | Missing-grant refusal only after a 30 s compile | **Tracked** (check grants at compose time, before Cranelift) |
| 7 | No IOMMU — device DMA unconstrained | **Owner decision** (smmuv3 plan vs defer to real-board; soften containment wording meanwhile) |
| 8 | Device claiming is per-task, not machine-wide | **Tracked** (with the storedisk-coexistence item) |
| 9 | DMA contract unstated in WIT | **Fixed** (`02460f9`: documented in plan/02 D22 + driver module docs; wit/ doc-comment application pending) |
| C2 | Address-keyed allow-lists fragile across boot configs | **Owner decision** (vendor:device matching in pci.filtered) |
| C3 | No long-running driver concept | **Owner decision** (tied to Message API / supervision design) |
| C4 | Async API vs all-eager reality (queue depth 1) | **Tracked** (async bridge follow-up + the driver-API consequence) |
| P1 | Compile cache unusable with PCI grants | **Tracked** (same root as machine-global claiming) |
| P3 | No fault-injection surface for drivers | **Tracked** (a `pci.fault` middleware stub) |
| 5(R1) | MSI/MSI-X, FLR, hot-plug, AER all unsupported | **Tracked** (enumerate the unimplemented surface in one place) |

### Study 10 — returning novice (17 findings)

| # | Finding | Final disposition |
|---|---|---|
| 1 | Help's own `&` example fails | **Fixed** (`9fee776`: working example + a test that runs every help example) |
| 2 | `/welcome.txt` recommends `wc` (absent from browser store) | **Fixed** (`9fee776`: only existing programs + harness check) |
| 3 | `&` teaching error vanishes when args present | **Fixed** (`9fee776`: kind check first; suggestion preserves args) |
| 4 | `save` vs `ls /bin` disagreement in browser | **Owner decision** (session-overlay design) |
| 5 | Failed-`let` cascade misleading | **Fixed** (`9fee776`: teaches bindings vs programs, no enum leak) |
| 6 | `let` succeeds silently | **Fixed** (`9fee776`: confirms name/kind/exports) |
| 7 | `eo9:rt/diagnostics` unexplained | **Fixed** (`9fee776`: annotated in describe/imports) |
| 8 | fs errors leak `FsError::…` enum text | **Tracked** (R2-18, error-quality consistency) |
| 9 | Browser boot banner jargon | **Owner decision** (humanize vs keep provenance) |
| 10 | `ok:` vs `success(…)` rendering split | **Tracked** (outcome-line unification) |
| 11 | `eo9 store --help` errors | **Tracked** (R2-18) |
| 12 | `[stderr]` prefix + stray blank line in page terminal | **Fixed** (`9fee776`: line-buffered streams, styled errors) |
| 13 | Front page vs try-it page `only` spelling | **Fixed** (`9fee776`: short form on both) |
| 14 | "Passed open, not run" wording | **Fixed** (`9fee776`) |
| 15 | No authoring/language statement on the site | **Owner decision** (site prose + roadmap commitment) |
| 16 | Front-page jargon ("language-theoretic principles") | **Owner decision** (the owner's voice) |
| 17 | Page's explore section omits `env` | **Tracked** (plan/18 D32 one-liner) |

### Study 11 — distribution engineer (24 findings)

| # | Finding | Final disposition |
|---|---|---|
| D9 | Publish pre-flight red on master (stale bundle) | **Fixed** (`7c4f3d4`/`c5ca409`: bundle refreshed; the recurrence guard is D9b) |
| D9b | Bundle drift check not in any gate | **Tracked** — it went into CI (`be91552`) and was pulled back out (`23699a9`): fs-eofs builds are not yet checkout-independent (cargo metadata-hash residue from the out-of-workspace eofs-core path dep). Candidate fixes recorded in plan/01 D15; the check remains in `cargo xtask package`. |
| D1 | No `eo9 --version` | **Fixed** (`be91552`: --version/-V/version, never falls through to name resolution) |
| D3 | `make setup` swallows doctor failures | **Fixed** (`be91552`) |
| D5 | Duplicate help entry | **Fixed** (`be91552`) |
| D10 | Pre-flight prints "0 KiB" sizes | **Fixed** (`be91552`: real sizes from tmp-crate/) |
| D15 | Unpinned wasm-tools install | **Fixed** (`be91552`: ~1.250 pin) |
| D11/D14 | No crates.io metadata / MSRV | **Fixed** (`be91552`: readme/keywords/categories/homepage, rust-version 1.94 verified on stable) |
| D12 | No platform statement / non-unix gate | **Tracked** (README platforms section + compile_error gate still open) |
| D13 | Registry users can't author programs; undocumented | **Tracked** (document the scope) + **owner decision** (publish the guest SDK?) |
| D21 | Stale plan docs (7 crates; STATUS "green" was false) | **Fixed** (`be91552` + this synthesis) |
| D2 | Doctor needs ~200 crates compiled first | **Owner decision** (accept/document vs dependency-light doctor) |
| D4 | Duplicate setup/doctor summaries | **Tracked** (fold when D2 is decided) |
| D7 | `env readwrite` mispredicts a refusal | **Tracked** (inspection bug) |
| D8 | `success(…)` vs `ok: …` split | **Tracked** (same as study 10 #10) |
| D16 | Downgrade silently reverts bundled programs | **Tracked** (record seeder version, warn) |
| D17 | No hosted CI / Linux unproven | **Owner decision** (participant blocker #1) |
| D18 | Manual 8-step publish, no tags/changelog | **Owner decision** (release policy + `xtask publish` automation) |
| D19 | The eo9 crate untestable before real publish | **Owner decision** (local-registry rehearsal vs accept risk) |
| D23 | Bundle size headroom unmonitored | **Tracked** (size assertion in pre-flight) |
| D24 | Crate names not reviewed for permanence | **Owner decision** ("the only irreversible decision") |
| D6 | Kernel build warnings during README flow | **Tracked** (already in GAPS) |
| D20 | `store --help` error | **Tracked** (already in GAPS; same as study 10 #11) |
| D25 | `~/.eo9` not XDG-compliant | **Tracked** (defer) |

## What landed well in round 3

The metal MAC/tamper lifecycle (a tampered cached artifact was detected, named, refused, recompiled, never
executed — under attack by both a study and a reviewer); kernel-attested interrupt completion on both GIC
and PLIC; one hash-verified wasm driver binary running unmodified on two ISAs with cross-architecture disk
portability; the README being 100% accurate (every example runs as written — round 1's top complaint is
dead); the clean-install story (a registry user gets a working OS with zero wasm toolchain); the website
turnaround ("there's something to try — that's 70% of the answer by itself"); and the discovery loop
(`help` → `ls /bin` → `describe` → `env` → compose) carrying a novice from zero to verified capability
sealing in one session.
