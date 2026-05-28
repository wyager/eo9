# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-27 (overnight; master at 4962464, after the configure-sync / bug-1-fix /
browser-eosh-shell / UART-RX-idle / Ctrl-C+cascade / describe-wiring / property-suite batch)._

## Decisions pending with the owner

- **Compose-time vs run-time provider parameters.** Changing a seed changes the composed artifact and forces
  a recompile. Owner parked the "late-bound parameter" idea until there is a clean design; revisit if
  deterministic sweeps start thrashing the compile cache.

## Settled directions (recorded so they're not re-litigated)

- **Owner rulings 2026-05-27 (the open design calls) — now implemented:** (1) **`configure` is synchronous
  and minimal** — DONE (`af9cb34`): binds compile-time constants, must not block/perform I/O; this fixed
  bug 1. (2) **Guest trap reasons are readable** — DONE (`dc53e70`, host-side render of the demangled
  backtrace); the panic *message* itself still needs an `eo9:rt/diagnostics` post-trap export (follow-up
  below). (3) **`describe --wiring` full composition tree** — DONE CLI-side (`00bfaf7`); the eosh-side tree
  needs an `eo9:exec` WIT field (follow-up). (4) **Entropy stays in the default child set** — no-op by
  decision; spawn-time grant visibility shipped under `-v`. (5) **Roadmap order: depth → breadth → real
  hardware** — the D38 metal-depth hardening is essentially done (idle, preemption, Ctrl-C, cascade, caps;
  W^X in progress); riscv64/x86_64 QEMU ports next, then real-board (owner will obtain an aarch64 board once
  the demos look good). `only` package shorthand, `-c` stderr outcomes, and `-v` grant visibility — DONE
  (`a212595`); honest `-c` 0/1/2/3 exit codes still need an eosh `program-failure` WIT class (follow-up).
- **The in-browser real-stack VM shipped** (supersedes the 2026-05-26 "/try v2 deferred" ruling): the owner
  re-opened it on 2026-05-27; the wasm32+Pulley blob runs on `/vm` through milestone 2 (fiberless callback
  execution behind an off-by-default vendored feature, browser root providers, HTTP program store, JSPI
  suspension, retail-Chrome-verified). The server now has compression, security headers, and
  Cloudflare-friendly content-fingerprinted immutable caching with no per-request blob hashing. `/try` v1
  remains alongside it. **Milestone 3 (eosh in the browser) shipped** (`4962464`): the real algebra + the
  unmodified eosh boot in the blob; the shell resolves `/bin` and runs 16 programs (examples + coreutils).
  Remaining (in progress, `area/18-web-complete`): in-browser `$`/`&` via a server-side `/vm/compile`
  endpoint, and `only`-narrowing via a restricted linker. (plan/18, plan/15)
- **No upstreaming until a compelling MVP** (owner ruling 2026-05-26), refined 2026-05-27: feasibility
  reports live in `docs/upstreaming/`, and three contribution packages are staged locally for owner review
  and push (wasmtime CM-async no_std; wit-parser no_std decoding; wasm-wave no_std `wit`). wit-component
  should start as a "make wasm-metadata no_std" design issue; wac needs an appetite-check issue.
- **On-target codegen forked cranelift rather than waiting for upstream** (owner ruling 2026-05-26) — done;
  the vendored compile-layer + algebra forks live under kernel/vendor, provenance-reviewed.
- **Unconfigured providers never trap** (owner ruling 2026-05-27, option C): standard stubs self-bind
  documented defaults; providers with no sensible default must export only their config interface so
  unconfigured composition is a compose-time mismatch (SPEC "export shape encodes whether configuration is
  required"). Implemented for the four configurable stubs; the compose-time export-shape diagnostic landed (a
  required API import satisfiable only by a config-only provider is refused with an "apply `configure(…)`"
  hint); `describe` marking of required config args is still queued.
- **Root-handle resources live in the API interface** (owner ruling 2026-05-27, option 1): done for
  `eo9:fs`; disk/net/pci migrate by the same mechanical recipe when one gains a multi-instance consumer.

## Design decisions deliberately parked

- **Configure for resource-owning providers** (fs.memfs, disk.mem, net-style): parked until there are
  concrete consumers. The documented-defaults rule removes the day-to-day pain (fs.memfs needs no configure
  at all); the binder-vs-runtime-assisted choice still stands for providers that will need real arguments.
  (plan/03 D13)
- **Content-only vs layout-dependent eofs node hashes** — decide at eofs milestone 2. (plan/14 D4)
- **Component-typed arguments** (`interpret (…)`) — revisit when something consumes it. (plan/10 D6b)
- **dma-buffer ↔ `eo9:io` buffer relationship** (eo9:pci) — needs a unified buffer story when the first
  real PCI provider exists. (plan/02 D14)
- **Exec-copy cleanup / crates.io name** — operational niceties, owner-facing.

## Functional gaps (implementation exists, coverage incomplete)

### Algebra correctness (from the PL study)
- **FIXED — `fs.none $ <fs-consumer>` encode/validation failure** (study 05 #2, merge 6438b22): compose/extend
  now skip wiring an authority-free import from a provider that doesn't also satisfy the package's authority
  interface, per the no-op-drop law; the `time.none`/`text.none`/`entropy.none` family is healed too and the
  seeded soundness corpus guards it.
- **FIXED — `rename` on a residual import produced an invalid artifact** (study 05 #3, merges 6438b22 +
  82b2eeb): `Component::executable_bytes()` strips the implements annotation and is now fed to the compiler
  in the runtime, CLI, and kernel compile paths, while describe/store-hash/cache-key keep the full bytes —
  renamed-residual artifacts compile and run.
- **FIXED — Configured middleware over a configured provider traps** (study 05 #1, merge `af9cb34`): the
  root cause was that an async `configure` gate (sync-lifted) couldn't wait for a non-eager configure that
  reaches through another configured provider. Making `configure` a synchronous WIT `func` (it only binds
  compile-time constants) removed the gate entirely; the two interposition cases (`$` and `&`) now pass.
- **FIXED — Generative property-test suite** (study 05 #6, merge `26ddc28`): `algebra_properties.rs`
  enumerates compose / nested-compose / `&` / `only` / `rename` over a fixed cap-vocabulary catalog (510+
  cases) plus a guest-backed sweep over resource-owning/stateful providers, asserting
  encoder-validates-when-defined-else-typed-refusal, the action law `(x & y) $ c ≡ x $ y $ c`, sealing,
  `only`, and rename — deterministic, ~1.5 s. (The seeded `soundness_corpus` also remains.)
- **OPEN — `≡` and instance identity are undefined** in the spec's laws (when do two importers share one
  provider instance; does the action law preserve it); the identity element `empty` has no concrete
  spelling. The property suite defines `≡` operationally (same outcome + emitted text) as a stand-in.
  (study 05 #5, plan/13 D16)
- **OPEN — The spec-promised "exports match nothing" warning never reaches the user**: `compose_checked`
  returns `ProviderExportsUnused`, but surfacing it in eosh/CLI needs the host-side exec WIT. (study 05 #7)
- Binder caveats (unchanged): depends on wasmtime 45's CM-async ABI encodings (one constants block);
  suspended-subtask path not yet exercised end-to-end; cancellation of an in-flight forwarded call traps;
  >4 flat params / variant results / >16-flattened-param cases rejected with clear errors.
- Kernel algebra errors map to `Internal(String)` rather than the specific WIT variants; eosh `envinfo`
  still classifies authority by the `/types`-name heuristic instead of the structural `authority_free` flag.

### Runtime / providers (usermode)
- **Guest-facing `resume` unsupported (E5)**: children are fuel-sliced from the parent's donation; no
  guest-directed scheduling. (plan/04 D11/E5)
- **Fuel-quantum resume shim** (10k granularity) until wasmtime can park a fiber at fuel exhaustion.
- **Runtime links no disk/net/pci interfaces yet**; perf is a placeholder; Message API unstarted (blocks
  `text.capture`, pipes, parent↔child channels). `net.loopback`, `pci.deny`/`pci.filtered` stubs pending.
- **Codegen determinism not verified bit-for-bit**; cache keys carry `compiler_deterministic = false`; the
  embedded study also saw a fused-composition re-run that did not hit the cache — investigate. (plan/04 D3)
- **fs path containment is canonicalize-then-operate** with a post-open fd re-verification as the shipped
  interim; openat2/`RESOLVE_BENEATH`-style walks remain the real fix. Minor: a guest can distinguish whether
  an out-of-root symlink target exists (Denied vs NotFound). (plan/08 D7/D13)
- **Store/cache integrity is blake3 but unauthenticated** — no signing/provenance story yet.
- Shell `env` reads a session-manifest file as an interim for a real introspection surface; `/bin` and
  `session` are reserved names that shadow same-named `--fs-root` entries (and surprise users in `ls`);
  the session overlay is composed host-side — interposing the guest `fs.overlay` component is the recorded
  follow-up to make it algebraic. (plan/10 D9, plan/11 D15)

### Bare metal
- **FIXED — child fuel + preemption** (embedded persona's #1 blocker, merge e5b97c6): children run on a
  sliced fuel budget (10k yield interval) driven from a reworked check-out/poll/check-in loop, so a looping
  child no longer takes the machine; `program=… max-fuel=N` kills a runaway with `abnormal(killed)`. eo9-sched
  is still not adopted (round-robin + fuel-slicing suffices; revisit with guest-directed `resume`/E5).
- **FIXED — nested eosh on metal** (merge e5b97c6): metal children now inherit the full session environment
  (read-only store fs, io buffers, exec), so `eosh> eosh` works on metal incl. an on-target-compiled
  grandchild composition; `only` still strips the surface.
- **FIXED — kernel scheduling/interrupt depth** (plan/12 D39–40): a real PL011 RX interrupt + event-driven
  WFI took idle from ~1% to **0.0%** host CPU (`388962f`); **Ctrl-C** interrupts a foreground job, **kill
  cascades** to orphaned descendants, and a **per-child spawn cap** bounds nesting (`a127861`). Residual:
  the idle waker is still single-slot (needs a queue when multiple host futures park concurrently); nested
  shells still share the one serial console.
- **Other metal gaps**: **W^X for JIT code pages — in progress** (`area/12-wx`: map on-target code
  executable-not-writable); exceptions are fatal; on-target codegen determinism not bit-compared and measured
  ~25–35% slower than host AOT (verify opt-level parity); no instrumentation for peak compile heap / phase
  timings / cache-hit reasons; no writable storage or fused-artifact cache on metal; the kernel store image
  lacks the coreutils. (plan/12 D22–40, studies 01/03)
- **Wasmtime version bumps are not free**: re-verify the binder/executor ABI-constant blocks and re-AOT all
  artifacts on any bump off 45.
- riscv64/x86_64 ports and the QEMU test tier not started; real-board bring-up unscheduled (owner decision).

### Website / in-browser demos
- **FIXED — server hardening** (web-dev study, merges 3afc833 + 14c0443): pre-compressed `.br`/`.gz`
  siblings with Accept-Encoding negotiation (the /vm blob is ~320 KB brotli on the wire); security headers
  (CSP, X-Content-Type-Options, Referrer-Policy, COOP; HSTS on TLS only); and content-fingerprinted immutable
  URLs (`web-eo9.<hash>.wasm` via `vm/assets.json`) served `public, max-age=31536000, immutable` with no
  per-request hashing, short-cached HTML/manifest for instant deploys, and a `check-web-vm` drift guard.
  Deferred: COEP/Permissions-Policy headers; the disclosure sentences landed.
- **Path-dependent wasm32 blob build** (new): `build-web-vm` in a different checkout directory yields a
  different blob hash with no source change; `check-web-vm` validates committed self-consistency, not
  rebuild-match, so it won't catch cross-machine churn — wants a reproducible-build fix (e.g.
  `--remap-path-prefix`) before CI runs build-web-vm on another machine. (plan/15 D22)
- **/try v1**: not content-fingerprinted (its jco modules cross-import by relative path — needs the deferred
  shared-intrinsics work); ~570 KB of mostly duplicated jco glue (split shared intrinsics + minify;
  compression already covers the wire cost); stub composition blocked by the upstream js-component-bindgen
  TDZ bug (issue text drafted, plan/15 D11, D21).
- **/vm**: milestone 3 SHIPPED — fs + io providers in the blob, the in-blob `eo9:exec` surface, and the
  **eosh shell booting + running 16 programs** (`f419df8` … `4962464`). Remaining (in progress,
  `area/18-web-complete`): in-browser `$`/`&` via a bounded server-side `/vm/compile` endpoint (in-blob
  codegen is std/mmap-blocked), `only`-narrowing via a restricted linker, and seeding a provider into `/bin`
  to exercise composition. Blob now ~6 MiB raw / ~1.2 MiB brotli — a lazy-fetch `/bin` trim is wanted. The
  stackful-lift `sleepy` canary is refused on the fiberless host (page says so), though some engines
  (Bun.WebView) actually run it. JSPI support outside Chromium still to re-check. (plan/18 D7–17, study 04)

## Tracked from the user studies (see docs/user-studies/00-synthesis.md for the full triage)

- Debugging: preserve guest panic messages (owner design call above), source-line backtraces, a documented
  debugger workflow, symbolized kernel exception dumps.
- Onboarding/authoring: `eo9 new` scaffold; per-package guest builds; auto-pickup (or a loud warning) for
  guest crates missing from `GUEST_COMPONENTS`; optional/defaulted `main` args (WAVE-binder gap); a beginner
  tutorial that defines store/component/provider vocabulary.
- Error-quality consistency: fs errors still render as `fs("FsError::…")` debug text; deleting on the
  read-only `/bin` layer reports NotFound for a visible file; shell-path refusals print twice and exit 1 vs
  3 on the direct path; `eo9 store --help` errors instead of printing help; the outcome line needs a leading
  newline guard when program output doesn't end in one.
- Security follow-ups: hostile-component CI suite + fuzzing of the fs provider and ABI boundary; signed
  stores/provenance; W^X on metal; align the symlink Denied/NotFound oracle.
- Performance/instrumentation: compose/compile/run timing split, cache-hit reasons, peak compile heap;
  on-target vs host-AOT parity; zero-cost-layer claim needs a benchmark or softer wording.
- The `--debug-info` cache-key claim from study 01 was investigated and found already correct (closed).

## Minor nits / housekeeping

- Guest `wit-bindgen` is a temporary git pin (upstream main, 0.249 family) — return to a crates.io pin at
  the first published release with wit-parser ≥ 0.249. (plan/07 D9–10)
- `eo9:exec/args` (types-only) is linked only when exec is granted, contra the types-always-available
  convention.
- Guest-level kill-then-wait test deferred; host-level covered.
- plan/04 D12 still describes the (long-fixed) binder trap; update to point at plan/03 D12–13.
- Empty per-process exec-copy directories are never cleaned from the temp dir.
- `eo9-sched` not yet adopted by the CLI drive loop.
- Root host workspace manifest lacks a `license = "MIT"` field (guest/www have it).
- `eo9-embed`: exit-code mapping nit (0/1/2 vs 0/1/2/3); consolidate the `eo9` binary onto eo9-embed;
  engine/cache reuse; an exec-through-Host end-to-end test. (plan/16)
- kernel/vendor/README.md is missing the algebra-crate section (wit-parser, wac-*, wit-component,
  wasm-wave) — documented only in plan/12 D30–35.
- The owner pushes master to GitHub (github.com:wyager/eo9); planner-side agents never push.
