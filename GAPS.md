# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-27 (master at 805841a, after the layered-session / coreutils / overlay /
configure-defaults / study-fixes / GIC-idle / in-browser-VM wave)._

## Decisions pending with the owner

- **Package-level `only` shorthand**: should `only eo9:text` be accepted (expanding to the package's
  interfaces) or keep requiring full refs (`eo9:text/text`)? Every study persona tripped on it; README uses
  the full form meanwhile. (synthesis #20)
- **`describe` wiring/attenuator view**: `describe fs.readonly $ cat` is indistinguishable from
  `describe cat` — interposed attenuators are invisible in the residual import surface. Security and PL
  personas both want an audit view. (synthesis #7, study 05 #9)
- **Roadmap ordering**: confirm child-fuel/preemption as the next kernel milestone (embedded persona's #1 —
  one looping child currently takes the machine), and whether real-board bring-up jumps ahead of the
  riscv64/x86_64 QEMU ports. (synthesis #6, #14)
- **Shell `-c` outcome format + exit codes**: unify eosh's `ok:`/`error:` rendering with `run`'s WAVE
  format and propagate honest 0/1/2/3 exit codes through `shell -c` (needs a small eosh-world variant
  addition). (plan/11 D14)
- **Guest panic-message channel**: preserving panic text needs either a hidden import (violates the
  capability model) or a small diagnostic ABI — design call. (plan/10 D10)
- **Child-grant visibility / entropy opt-in**: children inherit the full session environment by design;
  the embedded persona suggested printing the grant at spawn or making entropy opt-in. Decide whether
  spawn-time visibility is enough. (synthesis #8)
- **Compose-time vs run-time provider parameters.** Changing a seed changes the composed artifact and forces
  a recompile. Owner parked the "late-bound parameter" idea until there is a clean design; revisit if
  deterministic sweeps start thrashing the compile cache.

## Settled directions (recorded so they're not re-litigated)

- **The in-browser real-stack VM shipped** (supersedes the 2026-05-26 "/try v2 deferred" ruling): the owner
  re-opened it on 2026-05-27; the wasm32+Pulley blob now runs on `/vm` through milestone 2 (fiberless
  callback execution behind an off-by-default vendored feature, browser root providers, HTTP program store,
  JSPI suspension, retail-Chrome-verified). `/try` v1 remains alongside it. Milestone 3 (eosh in the
  browser) is queued. (plan/18)
- **No upstreaming until a compelling MVP** (owner ruling 2026-05-26), refined 2026-05-27: feasibility
  reports live in `docs/upstreaming/`, and three contribution packages are staged locally for owner review
  and push (wasmtime CM-async no_std; wit-parser no_std decoding; wasm-wave no_std `wit`). wit-component
  should start as a "make wasm-metadata no_std" design issue; wac needs an appetite-check issue.
- **On-target codegen forked cranelift rather than waiting for upstream** (owner ruling 2026-05-26) — done;
  the vendored compile-layer + algebra forks live under kernel/vendor, provenance-reviewed.
- **Unconfigured providers never trap** (owner ruling 2026-05-27, option C): standard stubs self-bind
  documented defaults; providers with no sensible default must export only their config interface so
  unconfigured composition is a compose-time mismatch (SPEC "export shape encodes whether configuration is
  required"). Implemented for the four configurable stubs; the export-shape diagnostic and `describe`
  marking of required config args are queued.
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

### Algebra correctness (new, from the PL study — next fix wave)
- **Configured middleware over a configured provider traps**: `time.frozen --… $ time.fuzzy --… $ hello`
  (and the `&` form) fails at runtime; each provider works alone and the middleware works over the host
  clock — a counterexample to the override law and a composition shape missing from the suite. (study 05 #1)
- **`fs.none $ <fs-consumer>` fails encode/validation** instead of cleanly dropping the unmatched export —
  violates the no-op-drop law; wanted obligation: "if the interface-level composition is defined, the
  encoded component validates; otherwise a typed refusal". (study 05 #2)
- **`rename` on a residual import produces an invalid artifact** (codegen rejects the import name); renaming
  both sides then composing works. (study 05 #3)
- **`≡` and instance identity are undefined** in the spec's laws (when do two importers share one provider
  instance; does the action law preserve it); the identity element `empty` has no concrete spelling.
  (study 05 #5)
- **No generative property-test suite** over component triples (resources, types-siblings, multi-slot,
  stateful configured providers) asserting encoder-validates-when-defined + the action law — would have
  caught the three bugs above. (study 05 #6)
- **The spec-promised "exports match nothing" warning never fires** — a dead outer provider is silently
  ignored. (study 05 #7)
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
- **Scheduling is the top gap**: no child fuel, no eo9-sched adoption, no preemption — a looping child takes
  the machine (embedded persona's #1 blocker). Idle is now interrupt-driven (~1% host CPU via GICv2 + timer
  IRQ + `wfi`); a PL011 RX interrupt would make it true event-driven 0%; the single-slot idle waker needs to
  become a queue when concurrent children land. (plan/12 D36)
- **Metal children receive text/time/entropy only** — no fs/io/exec wiring, so fs-needing children get a
  friendly refusal and nested eosh on metal is not yet possible (the kernel `drive_children` loop also needs
  rework before nested spawning is safe). W^X for JIT code pages still TODO; exceptions are fatal; on-target
  codegen determinism not bit-compared; on-target code measured ~25–35% slower than host AOT (verify
  opt-level parity); no instrumentation for peak compile heap / phase timings / cache-hit reasons; no
  writable storage or fused-artifact cache on metal; the kernel store image lacks the coreutils.
  (plan/12 D22–37, studies 01/03)
- **Wasmtime version bumps are not free**: re-verify the binder/executor ABI-constant blocks and re-AOT all
  artifacts on any bump off 45.
- riscv64/x86_64 ports and the QEMU test tier not started; real-board bring-up unscheduled (owner decision).

### Website / in-browser demos
- **Server hardening** (web-dev study): no response compression (the 1.21 MiB /vm blob would be ~290 KB with
  brotli), no security headers (CSP, HSTS, X-Content-Type-Options, COOP/COEP), max-age-only caching with no
  ETag/fingerprinted URLs.
- **/try v1**: ~570 KB of mostly duplicated jco glue (split shared intrinsics + minify); the friendly
  refusal is launcher JS (the real enforcement is the absent import — add the disclosure sentence);
  stub composition blocked by the upstream js-component-bindgen TDZ bug (issue text drafted, plan/15 D11).
- **/vm**: milestone 3 = fs + io providers in the blob, the exec/store surface for eosh-in-browser, a
  callback-ABI sleep/read demo guest; the stackful-lift `sleepy` canary is correctly refused on the
  fiberless host (page says so); add the "these components import nothing yet" disclosure; vm.js error path
  hard-codes one cause and lacks an instantiateStreaming fallback; the determinism claim should point at the
  native cross-check; blob-size watch (1.21 MiB raw). JSPI support outside Chromium still to re-check.
  (plan/18 D7–9, study 04)

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
