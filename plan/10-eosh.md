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
