# Known Gaps, Limitations, and Deferred Decisions

Tracked by the planner so nothing gets lost. Each item notes where it is recorded and what unblocks it.
Items are removed when closed; design questions move to SPEC.md when resolved.

_Last updated: 2026-05-30 (master at ca255c8, after the polish / persistence / configure-baking wave: the
type-in terminal and explorable shell, purge-free caching, the riscv64 PCI provider + `pci.filtered`,
`eo9:disk` size/flush durability, the MAC-verified persistent compile cache on metal, and compound
configure-argument baking)._

## Decisions pending with the owner

- **The `bind` entrypoint for configuring resource-owning providers (plan/03 D21).** Compound argument
  values now bake (D20), but any config interface on a provider whose API owns resources (`eo9:pci`,
  `eo9:net/l4`, `eo9:fs`, …) still refuses: the binder gates configuration by re-exporting the API through
  forwarders, and forwarding resource-owning interfaces would mean hand-rolling wit-component's adapter
  generator. Recommended way out: re-export the provider's API directly (plain aliases, no proxying) and
  have the binder expose a parameterless `bind` entrypoint that every executor calls once after
  instantiation, before first entry. Touches wit/ + the three executors. **This is what keeps
  `pci.filtered --allow […]` and `l4-over-l2-config` unusable today** (both ship with only their
  unconfigured defaults working). The eosh tokenizer's unquoted-`,`-in-record-literals issue rides with
  whichever design is chosen.
- **Compose-time vs run-time provider parameters.** Changing a seed changes the composed artifact and forces
  a recompile. Owner parked the "late-bound parameter" idea until there is a clean design; revisit if
  deterministic sweeps start thrashing the compile cache.
- **In-kernel (Rust) drivers vs wasm-component drivers for boot-critical devices.** The working direction —
  proven by `disk.virtio`/`net.virtio`, and by the in-kernel `storedisk` virtio-blk driver existing only
  because the cache is kernel infrastructure — is drivers as wasm components over `eo9:pci`, with in-kernel
  Rust reserved for what the kernel needs before it can run components. Formal owner ruling still open.
  (plan/12 D43/D50/D58)

## Settled directions (recorded so they're not re-litigated)

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
- **OPEN — resource-owning configure** (the D21 decision above): `pci.filtered --allow`,
  `l4-over-l2-config`, and any future config on a resource-owning API refuse with a typed error.
- **OPEN — The spec-promised "exports match nothing" warning never reaches the user**: `compose_checked`
  returns `ProviderExportsUnused`, but surfacing it in eosh/CLI is still queued. (study 05 #7)
- Binder caveats (narrowed): depends on wasmtime 45's CM-async ABI encodings (one constants block);
  suspended-subtask path not yet exercised end-to-end; cancellation of an in-flight forwarded call traps;
  variant/result/flags/handle-typed configure values still refuse (with clear messages); the
  unbakeable-shape refusal is reported under the `Internal` error variant (cosmetic tidy-up queued).
- Kernel algebra errors map to `Internal(String)` rather than the specific WIT variants; the kernel renders
  `wiring` as a leaf only; eosh `envinfo` still classifies authority by the `/types`-name heuristic.

### Runtime / providers (usermode)
- **Guest-facing `resume` unsupported (E5)**: children are fuel-sliced from the parent's donation; no
  guest-directed scheduling. (plan/04 D11/E5)
- **Fuel-quantum resume shim** (10k granularity) until wasmtime can park a fiber at fuel exhaustion.
- **Capability coverage**: still **no host net root provider** in usermode (the layered guest stack covers
  metal); perf is a placeholder; the **Message API is unstarted** (blocks `text.capture`, pipes,
  parent↔child channels).
- **TCP/IP middleware depth**: ships without DHCP / IPv6 / an l3 export; TCP listen/accept coverage is
  shallow; address overrides await D21. (plan/09 D18/D20)
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
  its QEMU arm doesn't wire the `pci` grant yet**. The drivers are **polled** — the INTx/MSI interrupt
  delivery design is recorded (plan/12 D57) and in flight; no machine-global device claiming until it
  lands (the `storedisk` vs `pci`/`disk` don't-combine rule stands meanwhile). Filtering by vendor/device
  id (vs address) is a recorded `pci.filtered` follow-up.
- **Storage**: the `storedisk` compile cache is the first store-on-eofs rung; **writable /bin on disk**
  (shell-visible `store add` persisting across reboots) is in flight; cache eviction/space management and
  VIRTIO_BLK_F_FLUSH-on-commit for the cache's own writes are queued. (plan/12 D58)
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
- Error-quality consistency: fs errors still render as `fs("FsError::…")` debug text; deleting on the
  read-only `/bin` layer reports NotFound for a visible file; shell-path refusals print twice and exit 1 vs
  3 on the direct path; `eo9 store --help` errors instead of printing help; the outcome line needs a leading
  newline guard when program output doesn't end in one.
- Security follow-ups: hostile-component CI suite + fuzzing of the fs provider and ABI boundary; signed
  stores/provenance for the usermode store (the metal disk cache is MAC-verified); align the symlink
  Denied/NotFound oracle.
- Performance/instrumentation: compose/compile/run timing split, cache-hit reasons, peak compile heap;
  on-target vs host-AOT parity; the zero-cost-layer claim needs a benchmark or softer wording.
- A third round of user studies (over drivers/networking/persistence/the new try-it page) is in flight.

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
