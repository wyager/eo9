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
