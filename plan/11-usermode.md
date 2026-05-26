# 11 — Usermode binary (`crates/eo9`)

## Scope
The `eo9` CLI: the embedder that assembles runtime + scheduler + store + unix root providers into a running
usermode Eo9 instance, per the spec's "Usermode binary" deliverable.

## Spec references
"Eo9-as-program", "Usermode binary" deliverable, "Execution APIs" (closed-before-compile; environments are
data), Implementation Details.

## Deliverables
- `eo9` binary:
  - `eo9 run <name-or-path> [--flag value …]` — resolve via store (or direct path), close against the root
    environment, compile (cache), spawn, print WAVE outcome, exit code = ok/err only (the real outcome is the
    printed value).
  - `eo9 shell` — spawn eosh with an environment granting the standard APIs; stdio wired to the terminal.
  - `eo9 store add|ls|gc`, `eo9 compile <name>` (warm the cache), `eo9 describe <name>`.
  - Configuration: store path, fs root for the fs provider, which APIs the root environment grants
    (a simple config file or flags; least surprise over cleverness for MVP).
  - Logging/diagnostics behind a `-v` flag.
- Integration-test host for plan 13 (the usermode suite drives this binary).

## Dependencies
03, 04, 05, 06, 08 (and 10 for `eo9 shell`). This area is mostly glue — expect to start after Phase 1 lands
its first milestones, and to be the place where cross-area seams get found.

## Milestones
1. `eo9 run guest/examples/hello.wasm` (I1).
2. Store-resolved names + compile cache + `eo9 shell` (I2).
3. Concurrency/limits demos wired as tests (I3).

## Decisions

1. **CLI surface & exit codes.** `eo9 [options] <command>` with `run`, `describe`, `compile` (cache warm),
   `store add|ls|gc`, `shell` (stub), `help`. Options (hand-rolled std::env parsing, no CLI dependency):
   `-v/--verbose`, `--store <path>`, `--fs-root <dir>`, `--exec-snapshot <clone-or-refuse|clone-or-copy>`,
   `--max-memory <bytes>`, `--debug-info`; they are accepted before the command and between the command and
   the program reference — everything after the reference belongs to the program as `--<flag> <value>` pairs.
   `run` exit codes mirror the three-way outcome: 0 success, 1 failure, 2 abnormal (trap/kill); 3 means eo9
   itself failed before an outcome existed (usage, resolution, compile, or spawn errors). Configuration is
   flags + `$EO9_STORE` only; no config file for the MVP (least surprise over cleverness).
2. **Name-or-path rule.** A reference containing `/`, starting with `.`, or ending in `.wasm` is a host path;
   everything else must parse as a bare dotted store `Name` and resolves through the store's default profile.
   The rule is purely syntactic so behaviour never depends on what happens to exist on disk; `./x` forces the
   path route.
3. **Immutable loading.** Store names are read through the store's `ObjectHandle` and re-hashed against the
   resolved hash. Paths are opened through the unix fs provider's `open-exec` (snapshot provider rooted at
   the program file's own directory — `--fs-root` is the *program's* capability root, not where programs may
   be loaded from) under the default `CloneOrRefuse` policy: on a volume that cannot COW-clone (or when the
   exec-copy temp dir is on a different volume) the run fails with a message pointing at
   `--exec-snapshot clone-or-copy`, rather than silently copying.
4. **Arguments and outcomes.** Flag handling is type-directed per the spec: the component's `describe`d arg
   signature is consulted and a flag filling a `string`-typed parameter is taken literally (WAVE-quoted by the
   CLI); every other value is passed through as WAVE text and type-checked by the runtime at spawn. The
   outcome is printed to stdout as the spec's three-way variant in WAVE — `success(…)`, `failure(…)`,
   `abnormal(trapped("…"))` / `abnormal(killed)` — with the payload type shown under `-v`.
5. **Providers.** The runtime's provider traits are implemented in this crate as thin adapters over
   `eo9-providers-unix` (text→stdio, time→host clocks, entropy→OS RNG, fs→host directory tree), bridging its
   completion callbacks into the runtime's `BoxOp` futures with a one-shot cell; the waker that reaches the
   provider is the task's doorbell. The fs adapter (`HostFs`) wraps the unix fs provider and owns the handle
   tables mapping the runtime's `u32` handles to the unix provider's open-file / immutable-handle objects;
   containment is the unix provider's guarantee (guest paths can never escape the root) and nothing in the
   adapter widens it, so `--fs-root` *is* the program's filesystem capability. **The filesystem is granted
   only when `--fs-root` is given explicitly — there is no ambient default root.** Without the flag
   `Providers.fs` stays `None`: a program with a *required* `eo9:fs` import is refused before it runs with a
   hint to pass `--fs-root <dir>`, and optional fs imports simply observe absence (runtime auto-seal).
   Text/time/entropy are handed to every spawn — the runtime links only what the component imports, so this
   never widens a capability set. Disk and net are still not linked by the runtime, so programs importing
   those fail at spawn with the loader rule.
6. **Drive loop.** `run` uses the simple built-in loop from milestone 1: donate fuel in fixed 100-quantum
   slices, park the thread on the task's `runnable()` future when it blocks on I/O, stop at `Done`. Adopting
   `eo9-sched` run queues is deferred until there is more than one task to schedule.
7. **Compile cache integration** *(escalation resolved by the area-04-m2 merge — `Image::serialize` /
   `Image::deserialize` / `engine::compatibility_hash`)*. Cache keys follow plan 06: single module hash (no
   composition yet), empty configure constants, a canonical compile-opts text (`eo9-compile-opts 1` +
   `debug-info`), the host target triple, `compiler_deterministic = false`, and an engine-identity string
   that combines the human-readable wasmtime pin captured at build time (build.rs reads the workspace
   lockfile — kept for auditable cache metadata) with the engine's runtime `compatibility_hash` fingerprint
   (`… compat-<16 hex>`), which covers the wasmtime build, target, and compile-relevant settings. Caveat per
   plan 04: the fingerprint is stable for a given toolchain build but not across Rust/wasmtime upgrades, so
   an upgrade invalidates old entries (spurious misses, never false hits). The cached artifact is
   `Image::serialize` output wrapped in a one-line envelope recording its own blake3
   (`eo9-cached-image 1 <hash>` + payload): on a hit the envelope is verified against that recorded content
   hash before the bytes are handed to `unsafe Image::deserialize` (the deserialize trust contract), and the
   run launches with **no codegen**; a miss compiles exactly once and caches the very image it runs. An entry
   that fails the integrity or engine-compatibility check is ignored with a warning and the source is
   recompiled — it is never trusted with native code. More generally the cache is an optimization only:
   lookup and insert failures (a broken, unreadable, or unwritable cache — including the use-count bump on a
   read-only entry) degrade to warnings and the component is compiled from source, so a run can only fail on
   genuine resolution/compile/spawn errors or the program's own outcome. `-v` distinguishes "compile cache
   miss … compiling / cached image" from "launched from cached image". `eo9 compile` now warms the cache with
   the same path (and, since it goes through `Image::compile`, rejects providers as not-a-binary — the cache
   holds closed binaries per the spec); when the artifact could not actually be cached it says so instead of
   claiming "cached".
8. **`eo9 shell` runs eosh against a session.** `eo9 shell [-c <command>]` spawns the eosh component as an
   ordinary Eo9 program and drives it with the same built-in loop as `run`; interactive when `-c` is absent
   (the REPL's blocking `read-line` goes through the terminal text provider, so piped stdin works too), one
   command line when present; exit codes follow the shell's own outcome (clean exit 0, `command-failed`/io 1,
   abnormal 2), and a clean exit prints nothing beyond what eosh already printed.
   *eosh lookup order:* the store-bound name `eosh` first (first-run seeding normally provides it), then the
   dev-tree artifact `guest/target/components/eosh.wasm` relative to the current directory, then the copy
   embedded in the binary; none present ⇒ a clear error telling the user to `store add … --name eosh` or
   `cargo xtask build-guest`.
   *Session layout:* `<store-root>/shell/` is the session directory granted to eosh as its fs root;
   `shell/bin/<name>.wasm` is rebuilt on every shell start from (a) every bound store name (hard-linked to
   the store object, copied if linking fails) and (b) the dev-tree components under their shell names
   (`eo9-example-hello`→`hello`, `eo9-stub-entropy-seeded`→`entropy.seeded`, `eosh` verbatim), with store
   bindings winning on collision — because eosh resolves program names as `/bin/<name>.wasm` on its granted
   fs (plan 10 D4).
   *Grants:* eosh gets terminal stdio, host clocks, OS RNG, the session fs, and the exec capability
   (`ExecProvider` over the image's engine); its `ChildPolicy` hands children exactly the session root
   providers a direct `eo9 run` would get (text/time/entropy, fs only when `--fs-root` was given) and never
   exec itself. Known limitations, documented not solved: children execute inside the shell's own fuel
   donations (runtime escalation E5), so a long-running child throttles the shell; the shell and its
   children share the raw stdin/stdout streams (no multiplexing); and configured-provider transcripts
   (`fs.memfs $ readwrite`, `time.frozen $ hello`) currently trap inside the unconfigured stub — they need
   eosh's compose-time `configure` support (area 10, in flight), so the composed-stub test uses an
   unconfigured compose (`entropy.seeded $ cruncher`) and a configured transcript is a follow-up.
9. **Tests.** Unit tests cover the argv parser, cache-key construction, WAVE string quoting/arg binding, the
   outcome→exit-code mapping, the oneshot bridge, the fs-grant check, the component shell-name mapping, and
   the embedded component set. Integration tests (`crates/eo9/tests/cli.rs`) drive the real binary against
   the built example components: hello/outcomes (all arms incl. trap→abnormal)/cruncher end to end,
   second-run launch from the cached image (stderr + use-count evidence, and no codegen diagnostics on the
   hit), a tampered cache entry being refused and recompiled, a read-only cache never failing a run
   (cold-cache insert failure and use-count-bump failure both degrade to warnings), memory-limit enforcement,
   store add/ls/gc + run-by-name, describe, compile warm, `readwrite` end to end through the unix fs provider
   (write + read-back against a temp `--fs-root`, fs failures staying in the program's own vocabulary, escape
   attempts denied inside the root, and a run *without* `--fs-root` being refused with the grant hint), shell
   transcripts (bare-name run, `describe`, an unconfigured compose checked against `eo9 run`'s digest, child
   failure / unknown name ⇒ exit 1, a piped interactive `let` session, and store-bound names incl. eosh
   itself), and the demo defaults (bare `eo9 -c …` through the default-to-shell path, implicit `run` by path
   and by seeded bare name, first-run seeding being idempotent and never clobbering user bindings). The test
   harness builds the components via `cargo xtask build-guest` only when they are missing — and then rebuilds
   the eo9 binary so its embedded set picks them up — so stale pre-existing components must be rebuilt by
   hand after guest-facing WIT changes.
10. **xtask touch (authorized follow-up).** `xtask build` (and therefore `ci`) now also runs
    `cargo check -p eo9-sched --target aarch64-unknown-none`, after the kernel build so the pinned toolchain
    already has that target installed.
11. **Demo defaults, embedded components, first-run seeding.** Bare `eo9` is the shell; `eo9 -c "<line>"` is
    the shell's one-shot form; a first argument that reads as a program reference (a path, or a bare dotted
    name per decision 2) is an implicit `eo9 run`; explicit subcommands are unchanged and anything else is
    still a usage error. `build.rs` embeds every component present under `guest/target/components/` at build
    time (eosh, examples, stubs — ~1 MiB total); when the directory is absent the set is empty and the
    dev-tree fallbacks still work, so **packaged/release builds must run `cargo xtask build-guest` before
    building `crates/eo9`** (escalation: area 01 may want `xtask ci` to run build-guest before build/test so
    fresh checkouts embed the set on the first pass; the test harness compensates by rebuilding eo9 after it
    builds missing components). On a shell start against a store with **no name bindings at all**, the
    embedded components are added to the object store and bound under their shell names (`hello`, `cruncher`,
    `eosh`, `entropy.seeded`, `fs.memfs`, …) with a one-line notice; a store with any existing binding is
    never touched, so seeding is idempotent and user bindings are never clobbered. Seeding failures degrade
    to a warning, and the embedded eosh also serves as the last-resort shell component when neither the store
    nor a dev tree provides one.
