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
the kernel env text (#18), the TOCTOU interim fd re-verification (#9), and the `--debug-info` cache-key
claim was investigated and found already correct (#5, closed). Still open from round 1: panic-message
preservation and the debugger story (#5), `describe` attenuator visibility (#7), child-grant
visibility/entropy opt-in (#8), signed stores (#11), hostile-component suite (#12), metal scheduling and
storage (#6, #13), real-board ordering (#14), instrumentation (#15), codegen parity (#16), authoring
friction (#17), `only` package shorthand (#20).

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

## What landed well in round 2

The /vm page running the real stack with native-matching entropy and fuel parity; the site's restraint
(small front page, no third-party JS); sealing, `only` position semantics, the action law on stateful
providers, and determinism-by-substitution surviving adversarial probing; the seeded-RNG and frozen-clock
demos as the moment the model "clicked" for the novice; refusal-before-run naming exact imports.
