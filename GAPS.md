# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-29 (master at 6f3c405, after the drivers / networking / persistence /
three-architecture wave: layered `eo9:net`, the virtio-blk and virtio-net wasm drivers over `eo9:pci`, the
smoltcp TCP/IP middleware and the metal sockets demo, eofs persistence in usermode and on a real virtio
disk, guest panic messages everywhere, and the riscv64 + x86_64 ports reaching parity with aarch64)._

## Decisions pending with the owner

- **Compose-time vs run-time provider parameters.** Changing a seed changes the composed artifact and forces
  a recompile. Owner parked the "late-bound parameter" idea until there is a clean design; revisit if
  deterministic sweeps start thrashing the compile cache.
- **In-kernel (Rust) drivers vs wasm-component drivers for boot-critical devices.** The working direction —
  proven by `disk.virtio`/`net.virtio` — is drivers as wasm components over `eo9:pci`, with in-kernel Rust
  reserved for whatever the kernel itself must have before it can run components (e.g. the disk a future
  on-metal store lives on). Formal owner ruling still open. (plan/12 D43/D50)

## Settled directions (recorded so they're not re-litigated)

- **Owner rulings 2026-05-27 (the open design calls) — all implemented:** (1) `configure` is synchronous and
  minimal — DONE (`af9cb34`). (2) Guest trap reasons are readable — DONE (`dc53e70`), and the panic
  *message* + location now arrive too via the write-once `eo9:rt/diagnostics.report-panic` sink
  (`f8dc070`; browser `9047c7f`; SPEC notes `only` always admits the rider, `c8e6695`). (3) `describe`
  composition tree — DONE on the CLI (`00bfaf7`) **and** at the eosh prompt via
  `eo9:exec/component-algebra.wiring` (`e7f198b`; kernel renders leaf-only for now). (4) Entropy stays in
  the default child set — no-op by decision. (5) Roadmap order depth → breadth → real hardware: depth is
  complete, and **breadth is now complete too** — riscv64 (`f651106`…`b6b7403`) and x86_64
  (`27d4edd`…`6f3c405`) are at functional parity with aarch64 and all three are CI-gated; real-board
  bring-up happens when the owner has hardware. `only` package shorthand, `-c` stderr outcomes, `-v` grant
  visibility — DONE; honest `-c` 0/1/2/3 exit codes — DONE (`e7f198b`).
- **Networking is layered (owner directive 2026-05-28)**: separate `eo9:net/l2`, `/l3`, `/l4` capabilities
  so each layer can be granted/mocked independently; higher-over-lower stacks are ordinary middleware.
  Implemented (`04c0eae`), in SPEC (`a6a4275`), and exercised end-to-end on metal
  (`net.virtio $ net.l4.over-l2 $ l4check`, `834f72e`).
- **The in-browser VM is fully self-hosted**: the `/vm` blob runs the real runtime + algebra + eosh, and the
  Cranelift→Pulley compiler runs **inside the blob** (`cbe8fe6`), so composition needs no server (the
  earlier `/vm/compile` endpoint was removed, `d844313`). `only` genuinely narrows via a restricted linker;
  `describe` shows the wiring tree; guest panics carry messages. The site is two pages — the front page and
  the auto-booting try-it shell — and the old jco `/try` page plus its build machinery are removed
  (`40a4116`, `94bc94e`). (plan/18, plan/15)
- **No upstreaming until a compelling MVP** (owner ruling 2026-05-26): feasibility reports live in
  `docs/upstreaming/`; three contribution packages are staged locally (wasmtime CM-async no_std; wit-parser
  no_std decoding; wasm-wave no_std `wit`) and are **on ice** until the owner reviews/pushes them.
- **On-target codegen forked cranelift rather than waiting for upstream** (owner ruling 2026-05-26) — done;
  the vendored compile-layer + algebra forks live under kernel/vendor, provenance-reviewed. The riscv64
  backend additionally needed a vendored copy of registry `cranelift-codegen 0.132.0` whose only delta is
  four `powi`→exact-power-of-two-division constants (provenance-diffed against the registry crate;
  kernel-workspace-only patch). (plan/12 D48)
- **Unconfigured providers never trap** (owner ruling 2026-05-27, option C): standard stubs self-bind
  documented defaults; config-only export shape signals "must configure"; the compose-time diagnostic
  landed. `describe` marking of required config args is still queued.
- **Root-handle resources live in the API interface** (owner ruling 2026-05-27, option 1): done for
  `eo9:fs`; disk/net/pci migrate by the same recipe when one gains a multi-instance consumer.

## Design decisions deliberately parked

- **Configure for resource-owning providers** (fs.memfs, disk.mem, …): parked until concrete consumers need
  real arguments; documented defaults cover today's uses. (plan/03 D13)
- **Content-only vs layout-dependent eofs node hashes** — eofs M2/M3 shipped on the existing engine design;
  revisit if a guest-visible hash/verify surface is added. (plan/14 D4)
- **Component-typed arguments** (`interpret (…)`) — revisit when something consumes it. (plan/10 D6b)
- **dma-buffer ↔ `eo9:io` buffer relationship** (eo9:pci) — both virtio drivers were comfortable with
  `alloc-dma` as-is; unify only if a future driver needs zero-copy paths into `eo9:io`. (plan/02 D14)
- **Exec-copy cleanup / crates.io name** — operational niceties, owner-facing.

## Functional gaps (implementation exists, coverage incomplete)

### Algebra correctness (from the PL study)
- **FIXED** — the drop-law failure, renamed-residual artifacts, the configured-middleware trap (bug 1), and
  the missing generative property suite are all fixed and regression-guarded (see plan/03, plan/13;
  merges `6438b22`, `82b2eeb`, `af9cb34`, `26ddc28`).
- **FIXED — `≡` / instance identity / `empty` were undefined** (study 05 #5): SPEC now defines observational
  equivalence, one-`$`-one-instantiation identity, and the `empty` provider (`489b3f5`, from plan/13 D17).
- **OPEN — The spec-promised "exports match nothing" warning never reaches the user**: `compose_checked`
  returns `ProviderExportsUnused`, but surfacing it in eosh/CLI is still queued. (study 05 #7)
- Binder caveats (unchanged): depends on wasmtime 45's CM-async ABI encodings (one constants block);
  suspended-subtask path not yet exercised end-to-end; cancellation of an in-flight forwarded call traps;
  >4 flat params / variant results / >16-flattened-param cases rejected with clear errors.
- Kernel algebra errors map to `Internal(String)` rather than the specific WIT variants; the kernel renders
  `wiring` as a leaf only (it stores fused bytes, not in-memory `Component` values); eosh `envinfo` still
  classifies authority by the `/types`-name heuristic.

### Runtime / providers (usermode)
- **Guest-facing `resume` unsupported (E5)**: children are fuel-sliced from the parent's donation; no
  guest-directed scheduling. (plan/04 D11/E5)
- **Fuel-quantum resume shim** (10k granularity) until wasmtime can park a fiber at fuel exhaustion.
- **Capability coverage**: `eo9:disk` is now linked host-side (the `--disk` file-backed device) and the
  layered net stack exists guest-side (`l4.loopback` for tests, `net.virtio`+`net.l4.over-l2` on metal),
  but there is still **no host net root provider** in usermode, `eo9:pci` exists only on aarch64 metal,
  perf is a placeholder, and the **Message API is unstarted** (blocks `text.capture`, pipes, parent↔child
  channels). `pci.deny`/`pci.filtered` stubs pending.
- **WIT round-out queued**: `eo9:disk` has no `size`/`flush` ops (fs.eofs probes size with zero-length
  reads; no durability barrier — FLUSH-on-commit for `disk.virtio` waits on it); the TCP/IP middleware has
  no config interface for address overrides (defaults to slirp's 10.0.2.15/24) and ships without
  DHCP/IPv6/an l3 export; TCP listen/accept coverage is shallow. (plan/09 D18, plan/14 D22)
- The shell-side missing-`--disk` refusal is raw linker text (the `run` path has the friendly message).
- **Codegen determinism not verified bit-for-bit**; cache keys carry `compiler_deterministic = false`; one
  observed fused-composition cache miss on re-run remains uninvestigated. (plan/04 D3)
- **fs path containment is canonicalize-then-operate** with post-open fd re-verification as the shipped
  interim; openat2/`RESOLVE_BENEATH`-style walks remain the real fix. (plan/08 D7/D13)
- **Store/cache integrity is blake3 but unauthenticated** — no signing/provenance story yet.
- Shell `env` reads a session-manifest file; `/bin` and `session` are reserved names that shadow same-named
  `--fs-root` entries; the session overlay is composed host-side rather than via the guest `fs.overlay`.
  (plan/10 D9, plan/11 D15)

### Bare metal
- **Parity reached**: all three QEMU targets (aarch64, riscv64, x86_64) boot to eosh, compile on-target
  from W^X pages, and match on preemption/Ctrl-C/idle. Remaining items below are residuals, not blockers.
- **PCI/drivers**: the `eo9:pci` provider exists only on aarch64 (ECAM bring-up is virt-specific) — riscv64
  and x86_64 cannot run the virtio drivers yet; the drivers are **polled** (no MSI/INTx delivery);
  `pci.filtered` (and filtering by vendor/device id rather than address) is queued; no machine-global
  device claiming until the interrupt work lands. (plan/12 D43/D50/D51)
- **Storage**: the artifact/compile cache and the store still live in the baked image — **store-on-eofs /
  store-on-disk is queued** (the virtio-blk + eofs stack now makes it possible). (plan/14 D22)
- **Kernel hardening residuals**: kernel-image-internal W^X (`.text`/`.rodata`/`.data` split) and guard
  regions; exceptions other than IRQs are fatal; the idle waker is single-slot; nested shells share the one
  serial console.
- **Diagnostics/runner gaps**: the headless `program=` runner ignores `program=eosh` and does not carry the
  guest panic message (the interactive path does); on-target codegen determinism is not bit-compared and
  measured ~25–35% slower than host AOT; no instrumentation for peak compile heap / phase timings /
  cache-hit reasons. (plan/12)
- **Scripted-console conventions** (not bugs in the kernel input path): on riscv64, OpenSBI consumes a byte
  that arrives before the kernel exists — scripted sessions must wait for the prompt before sending input;
  on every arch a full-speed pasted line can outrun the UART model — pace scripted input. (plan/12 D49)
- **Wasmtime version bumps are not free**: re-verify the binder/executor ABI-constant blocks and re-AOT all
  artifacts on any bump off 45.
- Real-board bring-up is unscheduled (waiting on hardware); the QEMU test tier is still scripted/manual
  rather than part of `cargo xtask ci`.

### Website / in-browser demo
- **Blob reproducibility**: `--remap-path-prefix` removed embedded checkout paths and same-path rebuilds are
  byte-identical, but a build from a *different* checkout directory still differs by ~410 bytes of cargo
  unit-metadata for the `[patch]` path deps — full cross-machine reproducibility needs cargo-side or
  workspace-restructuring work. (plan/18 D26)
- **Asset churn**: every guest-SDK change re-fingerprints all `/vm` store assets (~11 MB of binary churn per
  such merge); if repository weight becomes a problem, move the committed web assets out of git / build at
  deploy time.
- **Performance honesty**: browser programs are Pulley-interpreted — fine for the shell and coreutils,
  noticeably slower than native/metal for compute-heavy runs (the page says so).
- **Remaining polish**: a click-through on the live deployed site (after the owner's next push/redeploy);
  lazy-fetching `/bin` raw+cwasm pairs to trim the ~8.8 MiB raw blob; COEP/Permissions-Policy headers;
  JSPI support outside Chromium re-check.

## Tracked from the user studies (see docs/user-studies/00-synthesis.md for the full triage)

- Debugging: panic message + location now arrive everywhere (DONE); still open — full source-line
  backtraces, a documented debugger workflow, symbolized kernel exception dumps.
- Onboarding/authoring: `eo9 new` scaffold; per-package guest builds; auto-pickup (or a loud warning) for
  guest crates missing from `GUEST_COMPONENTS`; optional/defaulted `main` args; a beginner tutorial that
  defines store/component/provider vocabulary.
- Error-quality consistency: fs errors still render as `fs("FsError::…")` debug text; deleting on the
  read-only `/bin` layer reports NotFound for a visible file; shell-path refusals print twice and exit 1 vs
  3 on the direct path; `eo9 store --help` errors instead of printing help; the outcome line needs a leading
  newline guard when program output doesn't end in one.
- Security follow-ups: hostile-component CI suite + fuzzing of the fs provider and ABI boundary; signed
  stores/provenance; align the symlink Denied/NotFound oracle. (W^X is in place on all three arches.)
- Performance/instrumentation: compose/compile/run timing split, cache-hit reasons, peak compile heap;
  on-target vs host-AOT parity; the zero-cost-layer claim needs a benchmark or softer wording.
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
- Root host workspace manifest lacks a `license = "MIT"` field (guest/www have it; the published crates
  carry it individually).
- Full-feature kernel cargo builds emit two known warnings outside the clippy gate (`arch::NAME` dead code;
  one unnecessary-unsafe block in the x86_64 wasm config path) — cosmetic cleanup for a later kernel pass.
- kernel/vendor/README.md documents the cranelift-codegen copy but is still missing the algebra-crate
  section (wit-parser, wac-*, wit-component, wasm-wave) — documented only in plan/12 D30–35.
- The owner pushes master to GitHub (github.com:wyager/eo9); planner-side agents never push.
