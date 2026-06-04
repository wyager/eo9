# 10 — eosh (`guest/eosh`)

## Scope
The Eo9 shell, written as an ordinary Eo9 program (a wasm component importing `eo9:exec`, `eo9:text`,
`eo9:fs`) — the shell has no private powers. (Confirmed: compiled from Rust to a wasm component, not an OS builtin.)

## Spec references
"Shell", "Programs as values" (type-directed arguments, grouping, top-level rule), "Composition and the `$`
operator" (precedence), "Environments and `&`", "The capability algebra" (`only`), "Capability slots,
`rename`, and `with`", Arguments-and-outcomes (WAVE).

## Deliverables
- Grammar + parser (hand-rolled recursive descent; parse keyword-first forms from the left):
  - atoms: names (dotted), literals, parenthesized expressions;
  - application binds tightest (flags `--name value`, values parsed by WAVE against the callee signature);
  - `&` next, then `$` (right-associative);
  - gate terms: `only <iface-list|world-name>`, `rename a b`, `with p as n[, …]` incl. tuple form;
  - `let name = <expr>` for session-local component/environment bindings.
- Evaluator:
  - name → component: resolve via the store-backed fs (`resolve` then `open-exec` then `load`);
  - build compositions via the imported `component-algebra` interface; type-directed argument handling via
    `describe` (component-typed params get program expressions, string-typed get literal text);
  - top level: compose with the shell's granted environment, `compile`, `spawn`, await outcome, print WAVE;
    non-zero-style failure = render the `err` variant.
- Builtins: `let`, `describe`/`imports`, `save`/`load`, `env` (show granted environment), `help`.
- Line editing: minimal (read line, history in memory). No job control in MVP.
- Tests: parser unit tests (precedence, associativity, the re-association example from the spec), golden
  transcript tests run under the usermode binary (plan 13).

## Dependencies
02, 07 (SDK), plus runtime/providers/store transitively at run time via plan 11. Consumed by 11 (I2) and 12
(boot-to-shell).

## Milestones
1. Parse + eval: run a bare binary with flags (I1-adjacent).
2. `$`, `&`, `let`, `only`, `with`; deterministic-environment demo (I2).
3. Builtins polish, error messages worth reading.

## Decisions

1. **Split: `eosh-core` (library) + `eosh` (component).** `guest/eosh/eosh-core` is a dependency-free
   `no_std + alloc` library holding the lexer, parser, evaluator, WAVE argument encoding, outcome/describe
   rendering, and the session (builtins, `let` bindings, the top-level rule), all behind a `Backend` trait
   (resolve, load, duplicate, describe, compose, extend, restrict, rename, compile, spawn, wait, print).
   `guest/eosh/eosh` is the thin component crate: it binds `Backend` to the real WIT imports and runs the
   read–eval loop. The runtime does not expose `eo9:exec` to guests yet, so the component cannot run end to
   end; everything that can be tested without it is unit-tested on the host against a mock backend
   (73 tests: grammar precedence/associativity incl. the spec's re-association example, `only`/`rename`/
   `with` incl. the tuple form, `let`, type-directed flags, the top-level plan, outcome rendering).
2. **World.** Package `eo9-eosh:eosh@0.1.0`, world `eosh`: imports `eo9:exec/{component-algebra, compile,
   task}`, `eo9:text/text`, `eo9:fs/fs`; exports `main: async func(command: option<string>)` — interactive
   REPL when absent, one-shot command when present (for scripts/golden transcripts). The exec bindings are
   generated in the eosh crate (the SDK world does not include exec); text/fs/io map onto `eo9_guest::api`.
3. **Grammar details.** `$ & ( ) , =` are always structural and must be quoted inside values; `#` comments;
   `let only rename with as` are reserved words; builtin names (`help`, `env`, `history`, `describe`,
   `imports`, `exit`/`quit`) are special only as the first word of a command. Gate terms must be followed by
   `$`. Flag tokens are WAVE-encoded by the declared parameter type: `string` is quoted/escaped by the shell,
   `option<…>` wraps in `some(…)` (bare `none` = absent, omitted optionals auto-fill `none`), everything else
   passes through as the user's own WAVE text; the host's `spawn` remains the type checker.
4. **Name resolution convention (interim).** A program name resolves to `/bin/<name>.wasm` (dotted name
   verbatim) on the shell's granted fs, opened with `open-exec`, read via the immutable handle, and `load`ed.
   Area 11's store-backed resolution replaces only `Backend::resolve` in the component crate.
5. **`let` bindings are duplicated per use.** The WIT algebra consumes components, so bound values are copied
   (`save` + `load` in the component backend) each time a binding or the granted environment is used.
6. **Deferred / escalations.** (a) Provider `configure` arguments — resolved: the `eo9:exec`
   `component-algebra.configure` operation landed and the shell now uses it (see decision 8).
   (b) Component-typed arguments (`interpret (…)`) are classified correctly but rejected at argument-encoding
   time: `spawn` takes WAVE text only. (c) `only <world-name>` (named policy worlds) needs store resolution.
   (d) `save`/`load` builtins, unmatched-export warnings, and history recall/line editing beyond in-memory
   `history`. (e) eosh-core's host tests are run with `cargo test -p eosh-core --target <host-triple>` inside
   `guest/`; `xtask ci` does not run guest-workspace host tests — wiring that in (one line in xtask `test`)
   or moving eosh-core under `crates/` is a planner call.
7. **Mechanical update by area 02 (async operations, branch `area/02-async-operations`):** the eo9 ops the
   shell awaits (`fs.open-exec`/`exec-read`, `text.read-line`, `task.wait`) are now `async func` imports;
   call sites and eosh-core are unchanged except one owned-String argument in `Backend::resolve`
   (`open-exec` takes its path by value).
8. **Provider flags mean `configure`.** Flags applied to a provider term are its configure arguments: the
   evaluator WAVE-encodes them against the provider's config signature (from `describe`, the same
   type-directed rules as `main` flags), fills omitted `option<…>` arguments with `none`, errors on missing
   required or unknown ones, and calls `component-algebra.configure` to bake them in as compose-time
   constants — before the provider is used by `$`, `&`, `with … as`, or `let`. A provider with no flags is
   used as-is (left unconfigured). The configured value carries no run-time arguments, so it composes,
   extends, and binds exactly like any other provider. The old "configure not supported" error path is gone
   (`EvalError::ProviderArguments` removed; the specific flag errors — unknown flag, expression for a data
   parameter, missing required argument — surface instead).
9. **`env` shows the session's capability picture; `env <expr>` shows one expression's.** The shell has no
   private way to ask the runtime what its session holds (it is an ordinary program), so the embedder that
   builds the session writes a small plain-text **session manifest** where the shell can read it with a
   capability it already has — the session filesystem, at `/session` (`eosh-core::envinfo`, format
   `eo9-session 1` + `shell|child <capability> <description>` + `note …` lines; unknown record kinds are
   skipped so the format can grow). `env` renders it: capabilities granted to the shell, what programs
   started from the shell receive, embedder notes, then the granted environment (if an embedder passed one)
   and the `let` bindings as before. `env <expr>` evaluates the expression like `describe` (nothing is
   compiled or spawned) and marks every residual import with how this session would treat it: *satisfied by
   the session (cap)*, *always available* (types-only and `eo9:io/*` — no authority), *absent — observes
   absence* (optional), or *missing — would be refused at spawn* with the `cap.none $ …` hint (required).
   The manifest is informational only — the runtime's linking rules remain the authority — and a missing or
   malformed manifest degrades to "no session capability information available". Backend gains one method
   (`session_manifest`, async; the component backend reads the file, the mock returns a canned string).
   *Escalation (proper fix, needs planner/WIT):* a real introspection surface — e.g. an `eo9:exec/session`
   interface with `grants: func() -> list<grant-info>` describing the caller's own providers and its
   children's policy — would replace the file convention; the manifest format was chosen to be trivially
   replaceable by it.

10. **Friendly error rendering (2026-05-27).** The eosh backend renders `only`/`$`/`&`/`configure`/`spawn`
    failures as plain-language sentences instead of the generated error enums' debug form (the user studies
    flagged `RestrictError::RequiredOutsideAllowList([...])` and raw linker text). Spawn `internal` errors
    that mention an unsatisfied `eo9:*` import are translated into "the program requires the <capability>
    capability, which this session does not provide to it". `load`/`rename`/`compile` keep the generic
    rendering for now. Guest-SDK panic messages are still discarded by the panic handler (preserving them
    needs either a hidden import or a new diagnostic channel — owner design call, see GAPS). **Update
    (2026-05-27):** trap reasons are now cleaned (`crates/eo9-runtime/src/trap.rs`: trap kind + a
    symbol-only demangled backtrace, no addresses/hashes) so a guest panic reads as
    `abnormal(trapped("guest panicked — wasm \`unreachable\` …; guest backtrace: … ← panic_fmt ← main"))`
    instead of raw escaped text. The panic *message* + source line still need the per-world post-trap
    export proposed in plan/07 Decision 11 (an export, not an import — capability-clean), deferred behind
    the configure-sync WIT churn.
11. **`only` package shorthand (2026-05-27).** An `only` allow-list entry may name a whole package
    (`eo9:text`) as well as a single interface (`eo9:text/text`); a package entry admits every interface of
    that package the consumer imports. Every user-study persona tripped on the full-ref-only requirement.
    Implemented entirely in `eo9-component`'s `restrict` (allow-list validation now accepts a `namespace:package`
    entry with no `/interface`, and `admitted` matches by package prefix when the entry has no `/`); eosh's
    `parse_allow_entry` already passed a package-only word through unchanged, and full refs are unchanged.
    Covered by `tests/eo9-integration/tests/only_shorthand.rs`.
12. **Variadic tail in argument application (2026-05-28).** Positional application arguments already filled
    parameters in declared order; now, when the callee's **final** parameter is `list<string>`, the
    positionals left over once the other parameters are filled collect into it as one list argument
    (`cat a.txt b.txt`), a single bare value for a `list<string>` flag coerces to a one-element list
    (`cat --paths a.txt`), and `complete_args` fills an omitted final `list<string>` with `[]` (so bare `ls`
    runs and lists `/`). Mixing the flag and positional spellings for the same parameter is a duplicate-argument
    error. The convention itself is plan/04 D13; the coreutil signatures that use it are plan/17 D6.
13. **`describe` shows the wiring tree; `program-failure` carries the inner command's class (2026-05-28).**
    (a) The `describe` builtin now ends with a `wiring:` section rendered from the new
    `eo9:exec/component-algebra.wiring` (plan/02 D18): the composition tree of the described expression, so
    an interposed attenuator (`fs.readonly $ cat`) is visible from inside the shell, where plain `describe`
    shows only the residual surface. The `imports` builtin is unchanged. The `Backend` trait gains
    `wiring()`; the mock logs it. (b) The eosh world's `program-failure` now distinguishes
    `command-failed` / `command-trapped` / `command-killed` / `not-runnable` (was: a single
    `command-failed(string)` for every one-shot problem). `LineResult::ProgramFailed` carries a
    `CommandClass` (failed/trapped/killed) and `LineResult::Error` — nothing ran — maps to `not-runnable`,
    which is what lets the `eo9 shell -c` embedder report honest 0/1/2/3 exit codes (plan/11 D20).

14. **Discoverability: help teaches by example, the banner points at it (2026-05-29, owner feedback).**
    The owner's testing feedback: beyond `describe`, there was no good way for a new user to "explore the
    sandbox". (a) `help` now shows a one-line example under each composition operator (`hello --name you`,
    `entropy.seeded --seed 7 $ rng --count 2`, the `&` form, `only eo9:text,eo9:time $ hello`) and gained an
    "explore the sandbox" block — `ls /bin`, `describe <name or expr>`, `imports <expr>`, `env`,
    `env <expr>` — ahead of the builtins line; the two phrases the browser harness asserts on
    ("compose: satisfy the program's imports", "builtins: help, env") are kept. (b) The interactive banner
    is now "eosh — the Eo9 shell (type `help` to explore, `ls /bin` to see what's installed)" — the prefix
    the CLI banner-count test matches is unchanged. (c) Confirmed (no change needed): `describe` of a
    provider already lists its `configure` arguments (`describe entropy.seeded` → `--seed: u64`), because
    `eo9-component::describe` extracts the configure signature for providers; the browser harness now
    asserts it. (d) Deliberately not done: distinguishing providers from binaries in `ls /bin` — the listing
    is a plain fs read and the kind is only known after a `describe` per entry; a `bin`-style builtin that
    describes as it lists is the recorded follow-up if wanted.

15. **`&` refusals name the offending operand (2026-05-29, owner-reported).** `entropy.seeded & echo` used
    to be refused with "the left operand is not a provider" — wrong, since the left operand *is* a provider.
    Root cause: `eo9-component`'s `extend` correctly checks both operands but its `ComposeError::NotAProvider`
    carries no side, and the eosh backend rendered every such refusal with the `$` wording (which genuinely is
    about the left operand). The check itself was never wrong; only the attribution was. Fix: the evaluator
    now checks both operand kinds (one `describe` per side) before calling `extend`, where the operands'
    source spellings are still known, and refuses with a message that names the operand at fault and — when
    both operands are bare names — suggests the `$` spelling instead (`to run it with that provider use
    `entropy.seeded $ echo``); when both operands are programs it says so plainly. The backend's rendering of
    a raw `NotAProvider` from `&` (now only a backstop) no longer claims a side either; `$` keeps its
    accurate left-operand wording. Cost: two metadata-only `describe` calls per `&` evaluation. Covered by
    eosh-core unit tests (right/left/both/configured-operand cases) and a CLI transcript; the
    eo9-component-level behaviour was already pinned by `algebra_properties`.

16. **The `save` builtin: persist a program or composition to the session's store (2026-05-30, branch
    `area/12-writable-bin`).** `save <name> = <expr>` parses exactly like `let` (same `<name> = <expr>`
    shape), evaluates the expression with compose-time arguments only (run-time arguments are refused, as
    for `let`), and asks the backend to persist the component value as `/bin/<name>.wasm`. The Backend trait
    gains one method — `persist(name, &component)` — implemented by the WIT backend as algebra `save` (the
    bytes) + an ordinary `eo9:fs` `open(create|write|truncate)` + `write`: **no new WIT**. Where the
    embedder's store is writable (the kernel's `storedisk` boot) the program lands on disk and resolves like
    any installed name, including for children and after reboots; on a read-only store (usermode, the
    browser page, metal without the disk) the embedder's `read-only` refusal is reported with pointers at
    the alternatives (`eo9 store add` in usermode, the `storedisk` boot on metal). Names are validated to
    the dotted shell-name shape *before* anything is evaluated. `help` lists the builtin; eosh-core covers
    parse/persist/refusal/bad-name in unit tests; a CLI transcript pins the usermode refusal text. The
    browser page ships an older eosh until its next asset rebuild; once it picks this up, whether a saved
    name is resolvable there (the blob's MemFs serves /bin and accepts writes, but nothing persists across a
    reload) should be verified and the page copy adjusted — recorded as a web follow-up, not a kernel
    concern.

17. **Study-10 fix batch: the shell's own surfaces must teach (2026-06-01, branch `area/10-ux-fixes`).**
    Round-3 user study 10 (the returning novice) found the shell's polish cracking exactly where its own
    materials point users: (a) the help text's `&` example failed when typed (partial `time.frozen`
    configuration is refused) — the example is now the unconfigured-defaults form
    (`time.frozen & entropy.seeded --seed 7`), and a new eosh-core test extracts every `e.g.` line from
    `help_lines()` and evaluates it against mocks mirroring the real argument signatures, so an example the
    shell itself refuses can never ship again; (b) `entropy.seeded & echo --text hi` got the blunt
    "arguments cannot be applied" error instead of the teaching program-not-provider refusal — the
    operand-kind check now runs first, and the suggested `$` spelling preserves the user's arguments on
    either operand (`expr_spelling`); (c) `let` succeeded silently and a failed `let` cascaded into
    "cannot resolve `det` (/bin/det.wasm): FsError::NotFound" — `let` now confirms the name, kind, and (for
    providers) exports, and the WIT backend's resolve renders not-found as "no such binding or program
    `<name>`" with pointers at `ls /bin` and `let`, never as enum text (a `fs_error_text` helper covers the
    other resolution-path errors too); (d) `eo9:rt/*` imports in `describe`/`imports` carry an inline
    " — carries no authority; always admitted by `only`" annotation, sized to stay within the try-it page
    terminal's column budget; (e) the "passed open, not run" grouping line is reworded to "the inner
    expression is passed as a value, not run". Browser-side checks for all of these live in
    verify-eosh.mjs (plan/18 D37). Still open from the same study (tracked, not this branch): the
    `ok:`/`success(…)` rendering split, `eo9 store --help`, fs errors leaking enum text from the *coreutils'*
    own failure variants, and the owner-decision items (front-page voice, boot banner, save-vs-ls).

### Executor v1: `detach` and `svc` builtins (2026-06-01, area 10)

eosh is the first client of `eo9:svc` (executor v1, docs/design/executor-model.md + owner rulings):

* **Grammar.** `detach <name> = <program-expr> restart <policy-expr>` — the program side is a full
  expression (compositions, args, `only`, …); the policy side too (so `restart.backoff
  --max-restarts 5 --base-delay-ms 200` configures). The split is at the *last* top-level `restart`
  word, so a program itself named `restart` still parses; a missing clause is the typed
  `DetachNeedsRestart` parse error (the policy is required — owner ruling C). `svc` / `svc list` /
  `svc log|stop|clear <name>` are the inspection builtins.
* **Backend trait.** Six new methods (`svc_grants`, `svc_detach`, `svc_list`, `svc_log`, `svc_stop`,
  `svc_clear`) + the `ServiceInfo` record. The component backend reads the `-optional` imports for
  `svc_grants` and calls the full interfaces only when they answered `some` — sessions without the
  grant get a friendly refusal naming `eo9 --svc`, never a trap.
* **Top-level rule parity.** `detach` evaluates its program exactly like a foreground run (argument
  completion against the signature, provider refusal, granted-environment composition) before the
  handoff — a detached service runs with what *this session* could have given a foreground run.
* **World.** eosh imports `eo9:svc/detach`, `detach-optional`, `services`, `services-optional`
  (wit/svc dep symlink added). Executors must register all four (the runtime and the kernel both
  do; the kernel's registration answers "absent" until executor v2).
* eosh-core: 117 unit tests (16 new — parsing, grant refusals, lifecycle, soundness-at-the-shell);
  end-to-end: tests/eo9-integration/tests/svc_shell.rs (7 subprocess session tests).

18. **`describe` works on the shell's own words (2026-06-02, branch `area/18-prompt-explain`).** The owner
    asked for an `explain` builtin, then corrected: `describe` is the canonical spelling and should cover
    builtins — including itself. `describe <word>` now renders a hand-written plain-language card when the
    word is a builtin (`help`, `describe`, `imports`, `env`, `history`, `let`, `save`, `detach`, `svc`,
    `exit`/`quit`) or an operator (`$`/`compose`, `&`/`extend`, `only`, `rename`, `with`): kind line,
    one-paragraph summary, usage with a concrete example, and a `related:` pointer, all within the try-it
    page's ~109-column budget (`eosh-core/src/builtins.rs`). Precedence is deliberately narrow: only a
    *single* trailing word with a card takes the builtin path; anything longer — and any parenthesized
    expression, the escape hatch — keeps the expression path (resolution, backend describe, wiring).
    No `explain` builtin exists (nothing to alias). Coverage is test-pinned the same way the help examples
    are: `every_builtin_and_operator_has_a_card` enumerates the parser's dispatch list (a new builtin
    without a card fails the build), `every_builtin_the_help_text_lists_has_a_card` walks help's
    `builtins:` line, and the cards' column budget is asserted per line. `help`'s explore section now
    points at it (`e.g. describe describe`, which the help-examples test therefore also runs end-to-end).

19. **Coreutil failure text speaks the typed vocabulary (2026-06-02, branch
    `area/09-switch-convert`).** The browser-catchup review caught `cat` rendering a denied
    read as `fs("FsError::Denied")` — Rust debug formatting in failure text, the recurring
    R2-18 enum-leak class, this time in the programs rather than the shell. Every coreutil
    (cat, cp, find, head, ls, mkdir, rm, stat, touch, wc; echo and rng for output errors)
    now maps `FsError`/`TextError` through a human-vocabulary helper with the path in
    front: `fs("/inside.txt: denied")`, `fs("/no-such-file.txt: not found")`,
    `io("output closed")` — never the enum's debug form. Verified live at a usermode
    prompt (the subtree-policy denial and the not-found case), CLI suite 60/60,
    fs_filtered/capabilities/overlay suites green, full `cargo xtask ci` green. The
    harness pins were already case-insensitive (`/denied/i`), so only a stale comment in
    verify-eosh.mjs needed updating; the assets themselves are the merger's rebuild, per
    convention. Remaining instance of the class, deliberately untouched (example, not a
    coreutil, and pinned by cli.rs's "Denied" assertion): `readwrite`'s `{err:?}` mapper —
    a one-line follow-up for whoever next touches guest/examples.

## D20 — `describe` on the OS APIs themselves (2026-06-04)

Owner TODO: `describe eo9:pci` and `describe eo9:pci/pci` should explain the APIs the
way `describe describe` explains the shell. Decisions:

- **The WIT docs are the single source.** `eosh-core/build.rs` parses the repository's
  `wit/` tree at build time (line-oriented; our one-decl-per-line style) and bakes
  package + interface cards into a generated table (`apidocs.rs` includes it). No
  hand-maintained duplicate to rot; enriching a WIT doc comment enriches the card on
  the next build. Nine main interfaces (disk, entropy, fs, gfx, io/buffers, pci, perf,
  text, time) had no interface-level docs — given one-paragraph docs (additive,
  doc-only WIT change; `wit/check.sh` green).
- **Precedence:** in `describe <word>`, a single trailing word containing `:` routes to
  the API cards (`Command::DescribeApi`). Store names cannot contain `:` (the lexer
  reserves the spelling for interface references), so the route is unambiguous;
  parentheses still force the expression path, exactly as for builtins. `@version`
  suffixes are tolerated (`describe eo9:fs/fs@0.1.0` — the spelling import lists use).
  Unknown API names render the package inventory, not a resolution error.
- **The live section.** After the static card the session scans `/bin` (new
  `Backend::list_bin`, fs `list-directory` in the component; the registered names in
  the mock) and prints who exports / imports the described surface in *this* store —
  the card answers "what is this" and "who here speaks it" together. Broken store
  entries are skipped; an empty or unlistable store just omits the section.
- **Coverage discipline** mirrors the builtin cards:
  `every_wit_package_and_interface_has_a_card` re-scans `wit/` independently of the
  build-script parser and asserts every package and interface renders within the
  109-column budget — a new API cannot ship undescribed.

Follow-ups (not done here): the package inventory error line is one long line (matches
the long `builtins:` help line precedent); `describe` of a *world* (`describe
pci.filtered` already works through the store — the world spelling `eo9:pci/filtered`
is not a thing users meet).

## D21 — the session resolve cache (owner TODO: warm spawn, the eosh lane; 2026-06-04)

The spawn-fast spike (docs/spikes/spawn-latency.md) left ~98% of warm external latency
guest-side: per prompt line, eosh re-read every component from `/bin` through fs host
calls, re-ran the algebra, and re-passed every byte through the canonical ABI. The
session now carries a two-layer resolve cache (`eosh-core/src/cache.rs`):

- **Bytes cache** (name → component bytes, LRU 16 entries / 4 MiB): a repeated name
  `load`s from the session's copy instead of re-reading `/bin/<name>.wasm`
  (`Backend::resolve_with_bytes`, a default-method extension — byte-less backends are
  untouched). One canonical-ABI pass instead of read + load.
- **Image cache** (canonical run key → compiled image + bound args, LRU 8): a
  structurally identical line skips resolution, the algebra, `compile` — straight to
  `spawn` (images are many-spawn by the SPEC's own design). The key is the
  **fusion-graph identity at eosh's granularity** (the owner's ruling, mirroring the
  kernel's graph hash): canonicalized expression structure with netstring atoms,
  `/bin` leaves generation-tagged, `let` bindings substituted by sub-keys **frozen at
  bind time** — spelling, whitespace, parens, and binding names vanish; `let e = X`
  then `e $ hello` hits the same entry as inline `X $ hello`.

Invalidation is structural and conservative (a stale resolve is a correctness bug; when
in doubt, re-resolve):

| event | effect |
|---|---|
| `save <name>` | that name's generation bumps + its bytes drop; nothing else moves |
| a run whose program imports `eo9:fs` completes | global generation bumps + bytes clear (it *could* have rewritten `/bin`; the fs API exposes no content identity to check — node-stat is kind+size only) |
| `let` rebind | the frozen sub-key is replaced; unrelated entries untouched |
| `detach` of an fs-importing child | both caches disabled for the session (a concurrent writer defeats point-in-time invalidation) |

Store-immutability assumptions, per embedding: usermode sessions materialize a private
session dir (the session is its only writer); metal storedisk's only writers are this
console's `save` and fs-granted programs (both intercepted); the browser store is
read-only. External writers inside a session don't exist today on any target.

Deliberate v1 limits, recorded: argument values are part of the key (`hello --name a`
vs `--name b` rebuilds — argument-stripped image sharing via cached arg-specs is the
follow-up); fs-importing programs invalidate even when composed behind a read-only
attenuation (the import name is all eosh can see); a future `eo9:fs` content-hash stat
(the wit TODO at fs.wit:4) would turn the global bump into precise revalidation.
