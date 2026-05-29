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
12. **Interactive shell line editor (tab completion + history).** When `eo9 shell` is interactive — no `-c`
    and both stdin and stdout are terminals — the shell task's text provider is `InteractiveText`
    (`interactive.rs`): `write` goes to the real streams (tracking the trailing partial line so the editor
    knows the prompt to repaint), and `read-line` runs a small hand-rolled raw-mode line editor
    (`editor.rs`) on a dedicated thread, completing the runtime op through the same oneshot bridge the other
    adapters use. The editor implements emacs-style editing (cursor movement, kill, delete-word, Ctrl-L),
    in-memory ↑/↓ history, and readline-style tab completion: a unique candidate completes the word,
    ambiguity first extends to the longest common prefix, a no-progress tab lists alternatives and repaints.
    Candidates come from `complete.rs`: eosh builtins/keywords (lists mirror eosh-core's grammar — keep in
    sync), the session bin-view names `materialize_session` just placed (exactly what eosh can resolve), the
    standard `eo9:*` interface refs for words containing `:` (for `only`), and — only when `--fs-root` was
    given — paths under that root for words containing `/` or filling a flag value (a path-valued argument
    refers to the *child's* filesystem, so the host CWD is never offered). Flag names are not guessed.
    Raw mode is termios via `libc` (already in the tree through eo9-providers-unix; std has no termios),
    restored on drop; ^C cancels the line, ^D on an empty line ends the session, and if raw mode cannot be
    enabled the read falls back to a plain buffered line. Piped sessions, `-c` one-shots, and all children
    keep the plain stdio provider, so transcripts and tests are unchanged; the editor only changes how the
    interactive line is typed, never what is granted. Known cosmetic limit: output a child printed without a
    trailing newline is not part of the tracked prompt, so a mid-edit repaint redraws only the shell's own
    prompt. The editor and completer are unit-tested against in-memory streams (no TTY in CI).
    *Prompt ownership (fixed after the first merge):* eosh owns the prompt — it writes `eosh> ` like any
    other output and never knows whether an editor exists. The host editor owns only the **line repaint**,
    and the prompt prefix it repaints is single-sourced from `InteractiveText`'s tracker: "whatever stdout
    holds since the most recent newline". Both writers keep that tracker honest — guest writes update it in
    `write` (newline resets, partial line extends), and the editor, which always ends an edit by emitting
    its own newline, resets it when `read-line` completes. The original code missed the second half, so
    after any line with no stdout of its own (an empty Enter, a parse error on stderr) the next prompt was
    appended to the stale prefix and the repaint showed `eosh> eosh> …`, growing by one per Enter.
    Regression coverage: `shell_interactive_pty_prompt_is_not_duplicated` drives the real interactive path
    inside a pseudo-terminal via `script(1)` (macOS-only invocation, which is where local CI runs) with
    empty Enters, a command with output, and a parse error, and asserts no repaint ever contains
    `eosh> eosh>`.
13. **Session manifest for `env`.** Every shell start writes `<session>/session` (`providers::
    session_manifest`, format `eo9-session 1` — see plan/10 Decision 9): the shell's grants, what children
    receive (fs only when `--fs-root` was given, never exec), and notes. It is generated next to
    `shell_providers`/`child_root_providers` and must be kept in sync with them; writing it is best-effort
    (a failure degrades to a warning and `env` just has less to say). Children cannot read it — their
    filesystem, if any, is `--fs-root`, not the session directory.

14. **User-study fixes (2026-05-27).** (a) The typed outcome line is printed on **stderr** by default —
    program output owns stdout, the exit code already encodes the outcome — with `--outcome
    <stderr|stdout|quiet>` to override (the CLI transcripts assert the stderr form). (b) `--max-fuel <units>`
    caps the fuel donated by the drive loop; an exhausted budget kills the task (`abnormal(killed)`, exit 2);
    default unlimited. (c) Direct runs (`eo9 run <name>` / `eo9 <name>`) seed an empty store from the
    embedded components exactly like the shell path, so the first README example works on a fresh install.
    (d) Not done here, recorded as remaining: unifying eosh's `ok:`/`error:` per-command lines with `run`'s
    WAVE outcome format, and propagating the child's 0/1/2/3 exit code through `shell -c` (today a failed or
    trapped child both exit 1 via eosh's `command-failed`); needs a small eosh-world variant addition.

16. **`shell -c` ergonomics + child-grant visibility (2026-05-27 design-call batch).** (a) **One-shot outcome
    placement:** eosh routes its per-command outcome line (`ok:`/`error:`) to **stderr** in one-shot mode
    (`Session::route_outcome_to_stderr`, set by eosh's `main` when `--command` is given) so a `-c` invocation's
    stdout carries only the program's own output — matching `eo9 run`. Interactive mode keeps the outcome on
    stdout. (b) **No redundant wrapper:** `cmd_shell` no longer re-prints eosh's `failure(command-failed(…))`
    wrapper in one-shot mode (eosh already surfaced the command's outcome on stderr); the exit code carries
    it. An unexpected eosh trap/kill is still surfaced. (c) **Exit codes:** `shell -c` already returns 0
    success / 1 command-failed / 2 eosh-trapped / 3 eo9-error via `render_outcome`. The remaining gap — honest
    1-vs-2 for the *inner* command's failure-vs-abnormal, and a distinct 3 for "eosh couldn't run it" — is
    **blocked on a WIT change** to `eo9-eosh:eosh`'s `program-failure` (it must carry the inner command's
    three-way class instead of the single `command-failed(string)`); recorded as the precise next step, not
    done host-side because the class is unrecoverable from the rendered string. (d) **Child-grant visibility
    (synthesis #8):** `cmd_shell` logs, under `-v`, the capability set children inherit at spawn time; the full
    picture remains in the `env` manifest, and what children receive is unchanged (the entropy-opt-in question
    stays an owner decision). CLI transcripts updated for the stderr outcome and the new `-v` line.

15. **The layered session filesystem and the full child environment (Phase 2 of the overlay/recursive-eosh
    plan, 2026-05-27).** The session's filesystem is now an **overlay** (SPEC.md "Overlay filesystems"),
    assembled host-side in `providers::OverlayFs`: the *upper* layer is the session directory's read-only
    program view (`/bin/<name>.wasm` plus the `session` manifest), the *lower* layer is the user's writable
    `--fs-root` (absent → the overlay is read-only and mutations report `read-only`). Reads resolve
    upper-first and fall through to lower on not-found, listings union both layers (upper wins), every
    mutation routes to lower; handles are tagged with their serving layer. The shell **and every child**
    receive this same filesystem, and children now inherit the **full session environment** — text, time,
    entropy, the overlaid fs, and the entire `eo9:exec` surface (component algebra, compile, task) — via a
    recursive child policy (`shell_providers`' `make` factory), so a nested `eosh` is a full peer that can
    resolve `/bin`, compose, compile, spawn, and recurse; every generation gets the same environment.
    Restriction is composition: `only`/`$`/`&`/`configure` attenuate before spawn (covered by the
    `only_strips_the_whole_exec_surface_from_a_restricted_child` test), and the runtime still links only
    what a child imports. This supersedes the "children never receive exec" rule in Decisions 8/13 and the
    earlier held child-caps branch (its regression — children losing `--fs-root` — is exactly what the
    overlay fixes; the `coreutils_fs_tools_against_a_sandbox` and
    `shell_children_see_bin_programs_and_fs_root_data_at_once` tests pin both behaviors at once).
    *Why host-side rather than the guest `fs.overlay` component:* both of this phase's layers are root
    providers (the program view and `--fs-root`), which the OS core links directly like every other root
    capability; interposing the guest `fs.overlay` component instead requires the runtime to (a) satisfy a
    component's two named `eo9:fs` slots from two host providers and (b) compose the overlay onto every
    spawned child (with the compile-cost and cache-key consequences that implies) — recorded here as the
    follow-up that makes the session overlay itself algebraic; the guest component, its semantics, and its
    tests are already merged (plan/09 D11–12) and unchanged by this decision. Bare-metal recursion remains
    deferred (plan/12 D36: the kernel's child-drive lock); the kernel session still grants children
    text/time/entropy only. `env`'s manifest now describes the layered fs and the inherited exec, and
    `env <program>` marks `eo9:fs` imports as satisfied by the session (the read-only view exists even
    without `--fs-root`).

17. **`eo9 describe --wiring` (owner ruling 2026-05-27 — the full composition tree).** `describe` gains a
    `--wiring` mode: one reference renders the component as a leaf; several references compose a
    right-associative `$`-chain in-process (`describe --wiring A B C` ≡ `A $ B $ C`, last = consumer) and
    render the resulting `Component::wiring_tree()`, labelling each leaf with the reference it was resolved
    from. This is host-side and needs no WIT change — it builds the composition here, which is the only way
    to get the tree (provenance is in-memory, not in the bytes; see plan/03 D19). **eosh follow-up (not done,
    WIT kept stable this wave):** eosh's `describe` builtin goes through `eo9:exec`'s `describe`, which
    returns `component-info` with no wiring field, so eosh cannot show the tree for an expression it
    composed. Surfacing it there needs an `eo9:exec` addition — e.g. `component-info` gaining an optional
    wiring/provenance field (or a dedicated `wiring(component) -> string`) that the host fills from the
    algebra's `Wiring`. The CLI `describe --wiring` delivers the owner's full-tree ask now without that.
18. **Stale-store upgrade refresh + `eo9 store reseed` (the post-upgrade footgun).** A store seeded by an
    older eo9 broke under a newer binary (the old seeded eosh/components no longer match the runtime's WIT
    shapes — e.g. the fs-impl move — and spawning them died with a raw "resource implementation is missing"
    linker error; first hit on a fresh-laptop run, plan/01 D10). Seeding now leaves a **seed record** at the
    store root (`<root>/seed`: header `eo9-seed 1`, the embedded-set fingerprint = blake3 over the sorted
    `name hash` lines, then one `name hash` line per seed-managed binding; written by `crates/eo9`, ignored
    by `eo9-store` — see plan/06 D13). On every start (`shell` and store-name resolution), `seed::ensure_seeded`
    compares fingerprints and, on mismatch, re-binds exactly the names whose current binding is what seeding
    put there; user-added names and user re-bound names are never touched, objects are never deleted (gc
    reclaims), and one line announces it ("store: refreshed N bundled program(s) for this version of eo9").
    A store with no record is refreshed only if it was clearly seeded (an `eosh` binding exists — the
    legacy ~/.eo9 case); a store the user assembled by hand is left alone, preserving the original
    "seeding never clobbers user bindings" contract. `eo9 store reseed` forces the same refresh explicitly
    (recovery / scripting; on a record-less store it refreshes every bundled name, which is what the user
    asked for). As a backstop, a spawn failure for a store-resolved component that looks like a shape
    mismatch (missing interface instance / resource implementation) appends "this component may have been
    built for an older eo9 — try `eo9 store reseed`". Covered by four CLI tests (upgrade refresh, user
    bindings survive, legacy auto-refresh, reseed on a legacy store) plus seed-record/fingerprint unit tests.

19. **Installed binaries seed from the bundled component set.** `seed.rs` now routes every use
    of the embedded set through `carried_components()`: the build.rs-embedded
    `guest/target/components` set when the binary was built inside the repo (dev builds are
    unchanged), otherwise the prebuilt set from the `eo9-components` crate — which is what a
    `cargo install eo9` build carries, so a fresh install still seeds eosh, the examples, the
    coreutils, and the standard stubs (verified by building with the components directory absent:
    the binary seeded 36 bundled programs and ran `hello`). build.rs also falls back to the
    packaged `Cargo.lock` for the wasmtime-version half of the cache key when the workspace
    lockfile is absent (the published-crate case).
20. **Honest `-c` exit codes + the eosh-side wiring view (closes the D14(d)/D16(c)/D17 follow-ups,
    2026-05-28).** With eosh's `program-failure` now carrying the inner command's class (plan/10 D13),
    `cmd_shell` maps the typed failure's *case name* — the case of a typed WAVE value, not free-form text —
    onto `eo9 run`'s contract: `command-failed` → 1, `command-trapped`/`command-killed` → 2,
    `not-runnable`/`io` → 3; an unrecognised case keeps the plain failure code, so an older eosh still exits
    1. And eosh's `describe` builtin now shows the composition tree through `eo9:exec.wiring` (plan/02 D18),
    the in-shell counterpart of `eo9 describe --wiring`. CLI transcripts cover fail→1, trap→2,
    unresolvable→3, and the wiring tree for both a plain reference and a composed expression.
