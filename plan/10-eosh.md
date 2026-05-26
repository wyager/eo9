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
