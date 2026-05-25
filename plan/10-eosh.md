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
(record here)
