# User study 05 — programming-languages / type-systems researcher

## Session metadata

- **Participant persona:** programming-languages and type-systems researcher (capability systems,
  effect systems, ML-style module systems; has read the seL4 and MirageOS papers; cares about
  precise semantics, algebraic laws, and what is actually enforced versus convention); no prior
  exposure to Eo9; interacted only with what the facilitator showed — never read the repository or
  ran tools themselves.
- **Format:** conversational demo session focused on the capability algebra and its semantics. The
  facilitator ran every requested experiment live and pasted real (trimmed) output, and quoted
  SPEC.md verbatim whenever the participant asked for a definition.
- **Build under test:** `study/session-pl` worktree (master at c71ded7), `eo9` debug binary and the
  36 guest components rebuilt from that branch, wasmtime 45.0.0 (crates.io), macOS/aarch64 host.
- **Fixtures:** a study store at `/tmp/eo9-study-pl/store` seeded on first run with the 36 bundled
  components (examples, coreutils, stub providers, eosh); a sandbox directory
  `/tmp/eo9-study-pl/sandbox` containing `note.txt`, used for the fs demos.
- **Honesty rules:** all output below is from real runs against the build above, trimmed only for
  length (long wasm backtraces and repeated lines elided, marked `...`). Failures and unexpected
  behavior are shown as they happened; where the facilitator could not run or measure something,
  that was said instead of substituted.

---

## Round 1 — facilitator: pitch and anchor demos

**Pitch given:** every program/provider is a Component Model component; the import set is the
capability set; authority is decided by a composition algebra before run — `$` (right-associative
composition with sealing, residuals, kind preservation, an identity, and explicit
non-associativity), `&` (environment extension, right-biased shadowing, associative, action law
`(x & y) $ c ≡ x $ y $ c`), `only` (pre-run restriction gate, intersecting), `rename`/`with`
(slot relabeling/wiring), and compose-time `configure` bound by provider flags. Fused result
compiled as one artifact; do-nothing layers claimed to compile away.

**Demos shown (real output):** refusal before run (`only eo9:text/text,eo9:time/time $ hello`
runs; dropping time refuses pre-instantiation naming `eo9:time/time@0.1.0`); determinism by
substitution (`rng --count 3` differs run to run; `entropy.seeded --seed 9 $ rng --count 3` is
identical run to run); the decision visible in the artifact (`describe rng` lists the entropy
import; `describe entropy.seeded --seed 9 $ rng` no longer does).

## Round 1 — participant's reply (abridged)

Demo 3 (authority visible in the artifact's interface) noted as the kind of evidence they like.
Before laws, they wanted the objects and the equality pinned down:

1. **Sorts.** One syntactic category or two? Is `p $ q` defined when `q` is a provider — i.e. is
   the non-associativity claim even non-vacuous? Quote the spec's definition of `$` with operand
   sorts.
2. **What does `≡` mean** in the action law and the identity law — bit-identical artifacts,
   interface equality, observational equivalence, or "we property-test it"? Quote the exact
   statements.
3. **Sealing** — quote the definition; confirm it is the absence of the import in the artifact (a
   corollary of linking) and not a runtime flag.
4. **Experiments:** `describe empty $ rng` next to `describe rng` plus an artifact digest if one
   exists; nested `only` gates in both orders (intersection law); and the sort check
   `entropy.seeded $ entropy.seeded $ rng --count 1` — rejected, no-op, or something else?

---

## Round 2 — facilitator: definitions verbatim, then the runs

Quoted verbatim: the binary-vs-provider kind rule, the operator types
(`$ : component, component -> component`, `& : provider, provider -> provider`,
`run/interpret/exec : component -> execution`), the defining sentence for `$`, and the full law
list (Sealing, Residuals, Kind preservation & layering, Identity, Non-associativity).

Sorts answered: one category with a kind attribute; left operand of `$` must be provider-kinded
(or a gate term), right operand either kind, result takes the rightmost kind, so `(p $ q) $ c` is
well-formed and the non-associativity claim is not vacuous. The implementation enforces the sort
restriction with a typed refusal:

```
$ eo9 -c 'hello $ rng --count 1'
error: `$` refused: the left operand is not a provider (only providers can satisfy imports)
$ eo9 -c 'describe (hello & time.frozen)'
error: `&` refused: the left operand is not a provider (only providers can satisfy imports)
```

**`≡` answered honestly:** the spec writes `≡` and never defines the relation. What is checked
today is example-based — law tests compare import/export surfaces and run both sides of specific
laws on specific programs requiring identical stdout + typed outcome; no bit-identical-artifact
claim; codegen determinism across machines explicitly unverified in the gap list.

**Sealing confirmed** as the absence of the import from the composed artifact (plus the
"composition is early context-override" paragraph quoted); no runtime flag.

**Experiments run:**

- `empty` has no shell spelling and is not in the store (`error: cannot resolve `empty``). Closest
  approximation shown: `net.none $ rng` — `describe` identical to bare `rng`, runs normally.
  Admitted: no user-facing digest for a composed expression exists, so identity can be shown up to
  interface and behavior only. Also admitted: the spec's promised "shell warns when a composed
  provider's exports match nothing" did not fire.
- Nested gates, both orders, both refused before run naming `eo9:time/time@0.1.0` (intersection
  holds; cannot widen from outside).
- `entropy.seeded $ entropy.seeded $ rng --count 1`: accepted by the algebra (right-associativity
  drops the outer occurrence), then **trapped at run time** — the seeded stub's "used before
  configure" panic surfacing as a contained trap, because no `--seed` was bound and nothing calls
  `configure` for you. Flagged proactively as a known sharp edge with an open project decision
  (defaults vs refuse-before-run vs hybrid).

## Round 2 — participant's reply (abridged)

Banked four findings: (a) `≡` undefined — and whatever it is, it must include interface equality,
not just I/O behavior, because contexts observe interfaces (an outer `only` distinguishes
same-behavior terms with different residual imports); (b) the identity law quantifies over a unit
that cannot be written; (c) the missing unmatched-exports warning is a spec/impl gap; (d)
unconfigured-but-configurable providers compose fine and trap at run time — "the static story
leaks"; the algebra should treat an unconfigured configurable provider as a different sort or
refuse at compose time.

Next: the **override law** ("running `p $ c` in Γ behaves like running `c` in `exports(p)` over
Γ") presupposes an ambient Γ that silently closes residual imports — so absence of a provider is
not absence of authority. Wanted the spec's words on Γ (what populates it, per-what, inspectable
how, and whether `only` is the only way to prevent ambient closure). Wanted the overlap
experiment: `entropy.seeded --seed 1 $ entropy.seeded --seed 2 $ rng` vs
`(entropy.seeded --seed 1 & entropy.seeded --seed 2) $ rng` vs both single-seed references, plus
`describe` of both forms, plus the verbatim `&` definition.

---

## Round 3 — facilitator: Γ, the `&` definition, and the overlap test

Quoted verbatim: programs-as-values (running is a separate operation; top level of a shell command
implicitly runs against the shell's context), the loader rules (missing required import rejected
before execution; missing optional auto-sealed with `X.none`), "Closed before compile / the shell
has no private powers", "Environments are just data", and the `only` section's "a gate at the far
left bounds what the shell's ambient context may inject".

Honest framing of Γ on this build: the session's root providers (text, time, entropy, the layered
session fs — read-only `/bin` programs view over the optional `--fs-root` — and exec), implemented
host-side in the CLI process and governed by session flags; per-session, not global. Inspection is
`env` / `env <program>` (output shown), currently rendered from a session manifest (an interim
stand-in recorded in the gap list). Stated plainly: the spec's "the shell composes its environment
value onto the command" is aspirational in usermode — the implicit close is a separate loader path
that links residual imports against host-side providers, observationally the layering the override
law describes but not literally the same `$` code path. `only` is not the only way to prevent
ambient closure (sealing with any provider, `X.deny`, `X.none` for optionals, narrowing a child's
environment), but for a required import the session holds and you did not seal, the shell will
close it; deny-by-default refers to what the session does not hold.

The `&` definition and action law quoted verbatim. The overlap experiment run:

```
$ eo9 -c 'entropy.seeded --seed 1 $ entropy.seeded --seed 2 $ rng --count 3'
10905525725756348110 / 13819372491320860226 / 10987583248141275951        # (three lines)
$ eo9 -c '(entropy.seeded --seed 1 & entropy.seeded --seed 2) $ rng --count 3'
10905525725756348110 / 13819372491320860226 / 10987583248141275951
$ eo9 -c 'entropy.seeded --seed 2 $ rng --count 3'      # reference: seed 2 — identical to both
$ eo9 -c 'entropy.seeded --seed 1 $ rng --count 3'      # reference: seed 1 — different stream
```

Both composed forms produce the seed-2 stream (`$`'s innermost-wins and `&`'s rightmost-wins agree
on overlap — the action law at the point aimed at), and `describe` of both forms is identical
(entropy sealed, text residual). Volunteered: the dead outer `--seed 1` again produced no
"exports match nothing" warning — a user expecting seed 1 gets silence.

## Round 3 — participant's reply (abridged)

Credit given for the overlap test passing on both sides with `describe` agreeing. Logged: (1) the
override law is currently an **empirical claim about two code paths agreeing**, which is what
drifts — implement it as stated or restate it as a loader-correctness obligation; (2) the policy
default — children inherit the full session environment, least privilege is opt-in — is the part a
capability person pushes back on; the silent dead-outer-provider has now bitten twice. New probe:
**instance identity** — if `x` exports I and both a provider `y` and the binary `c` consume I, is
`x` instantiated once (shared state) or twice, and does the action law preserve the answer? Asked
for (a) the spec's words on instantiation/instance identity and (b) a stock provider+binary pair
to test it behaviorally. Then: the `rename`/`with` definitions and the trailed type-identity
wrinkle, and one sentence on how optional imports are encoded.

---

## Round 4 — facilitator: instance identity, the middleware trap, slots and type identity

**Instance identity:** quoted the only sharing statement in the spec (the `&` worked example's
comment — "one instance, shared" — plus the "Fusion shares implementation, never state" bullet,
which is about separate spawns). Stated honestly that there is no general statement of instance
identity under composition and no test pinning shared-vs-duplicated state; the implementation's
intent (one syntactic occurrence = one instance) was described as intent, not a guarantee.

**The behavioral test could not be built from stock parts — and the attempt found a new bug.** The
store has no entropy middleware and no provider+binary pair consuming one stateful interface by
two routes. The closest shape is the time middleware (`time.fuzzy` imports and re-exports
`eo9:time/time`), which is the spec's own worked-example shape. Run live:

```
$ eo9 -c 'time.frozen --now-seconds 50 --monotonic-ns 123456789 $ hello --name ref --excited false'
[50.000000000] Hello, ref.
ok: greeted                                        # configured guest source directly: works

$ eo9 -c 'time.fuzzy --granularity-ns 1000000000 $ hello --name ambientfuzz --excited false'
[1779929761.000000000] Hello, ambientfuzz.
ok: greeted                                        # middleware over the host clock: works (ns quantized)

$ eo9 -c 'time.frozen --now-seconds 50 --monotonic-ns 123456789 $ time.fuzzy --granularity-ns 1000000000 $ hello ...'
abnormal: trapped: error while executing at wasm backtrace:
    0:  0x19361 - <unknown>!<wasm function 15>
    ...
    3:  eo9_example_hello.wasm!eo9_guest::time::now
    4:  eo9_example_hello.wasm!main: wasm trap: wasm `unreachable` executed

$ eo9 -c '(time.frozen ... & time.fuzzy ...) $ hello ...'
abnormal: trapped: ... (same shape)

$ eo9 -c 'time.monotonic-stub --start-ns 5000000000 --step-ns 1000000000 $ hello ...'
[5.000000000] Hello, direct.                       # the other guest source directly: works
$ eo9 -c 'time.monotonic-stub --start-ns ... $ time.fuzzy --granularity-ns ... $ hello ...'
abnormal: trapped: ... (same shape)
```

Stated plainly: a configured guest middleware composed over a configured guest provider of the
same interface traps at run time with an opaque backtrace (inner frames lose their names through
the configure/fusion pipeline); the same middleware over the host-side session provider works;
each guest provider works alone; `describe` of the trapping chains is exactly what the algebra
predicts; both the `$` and `&` forms trap identically. The facilitator did not diagnose the root
cause live and said so; also stated that this provider-over-provider-with-both-configured shape
does not appear in the integration suite, so it is a coverage hole as well as a bug. The
unconfigured variants of the same chains fail differently (the named "used before configure"
panic in the respective stub).

**Slots, `rename`, `with`, type identity:** definitions quoted verbatim, including the
recently-added "Multi-instance imports and type identity" paragraph (root-handle resources are to
be declared in the API interface itself so each named import mints its own type; `fs.overlay` is
the canonical multi-instance consumer). Live state shown:

- `describe fs.overlay` shows real named slots (`upper:`/`lower:` of `eo9:fs/fs@0.1.0`);
  `describe rename upper top $ fs.overlay` shows the slot genuinely relabeled.
- The `with` sugar refuses every shipped fs provider: `with fs.memfs as upper, … $ fs.overlay …` →
  ``error: `with … as lower`: the provider must export exactly one interface (it exports 3); use
  `rename` explicitly instead`` — the precondition is per spec, but every shipped provider exports
  API + types + config, so the sugar is unusable against the project's own house style.
- The deeper wrinkle stated honestly: in the shipped WIT, `fs-impl` still lives in the types-only
  `eo9:fs/types`, both overlay slots `use` the same imported types instance (so their handle types
  are forced equal), while every standalone fs provider mints its own — wiring two independent
  leaves into the slots is ill-typed and the failure surfaces as a raw encoder error. The same
  root cause shown live in a one-provider shape:

```
$ eo9 -c 'fs.none $ cat --path /bin/hello.wasm'
error: `$` failed: encoding produced a component that failed validation
```

  (per the spec this should be a no-op drop of an unmatched optional export; instead fs.none's
  *types* export seals cat's types import while the fs import stays residual, the nominal identity
  splits, and encoding fails validation.)
- The just-adopted convention (root handles in the API interface) is in the spec but **not yet in
  the shipped WIT** (a migration branch exists, unmerged); `fs.overlay` therefore ships as a
  built, validated, describable component that cannot yet be wired from independent leaves; the
  session's `/bin`-over-`--fs-root` filesystem is real overlay semantics assembled host-side by
  the runtime, with the algebraic version recorded as the follow-up.
- One more volunteered bug: `rename eo9:time/time wallclock $ hello …` (renamed import left
  residual) fails at compile with
  `CompileError::Codegen("…invalid leading byte (0x2) for import name…")`, while renaming both
  sides and composing works (`(rename … $ time.frozen …) $ rename … $ hello …` runs and prints
  `[7.000000000]`).

**Optional imports** answered in one breath: derived `X-optional` interface flavor whose accessor
returns `option<x-impl>`; `import optional` is sugar; an export of X satisfies X-optional via a
derived adapter; the loader auto-seals missing optionals with `X.none`. Admitted: no seeded
program has an optional import, so absence-observation could not be shown live (integration
fixture only).

## Round 4 — participant's reply (abridged)

Called it the most informative exchange of the session. Findings as they wanted them logged:

1. **The middleware trap is a counterexample to a stated law, not just a bug.** With
   p = `time.frozen --…` and c = `time.fuzzy … $ hello`, the override law's right-hand side runs
   and the left-hand side traps — and the trapping shape is morally the spec's own worked example
   and the system's core pitch (attenuation by interposition). The algebra demonstrably supports
   substitution, refusal, and arguably dropping; interposition is unverified-and-currently-broken
   against algebra-composed sources, working only against the host side — exactly the
   two-code-path drift flagged earlier.
2. **The abstraction leak is the structural finding.** The laws are stated over name-sets; the
   objects are component types with per-exporter nominal resource identity. `fs.none $ cat` shows
   the interface-level algebra saying "no-op" while the component level produces an ill-typed
   artifact and a raw encoder error (credit: it fails closed). The algebra needs a soundness
   obligation — *if the interface-level composition is defined, the encoded component validates* —
   plus a typed refusal naming the split identity where that cannot yet hold. Same disease behind
   the two-slot overlay situation.
3. Smaller items: the `with` precondition contradicts the project's own provider house style
   (unusable on every shipped provider); `rename` on a residual binary import yields an invalid
   artifact; `only` matching by type cannot distinguish two slots of one type ("allow lower fs but
   not upper" is inexpressible at the gate) — defensible but should be stated as a limitation.
4. Two short questions before wrap-up: how is `configure` typed (is "configured" visible in the
   provider's type or only the cache key)? Has the zero-cost-layer claim ever been measured on
   this build?

---

## Round 5 — facilitator: configure typing, zero-cost honesty, last demos, wrap-up questions

**`configure` typing:** spec quoted (`configure : provider × args → provider`, bakes constants,
re-exports only the API; "an unconfigured provider has no handle to give, so 'used before
configured' is unrepresentable rather than a convention"). Reality shown: `configure` is a real
operation and the configured provider is a different artifact whose surface shows it —

```
$ eo9 -c 'describe entropy.seeded'              # args: --seed: u64; exports include entropy/seeded-config
$ eo9 -c 'describe (entropy.seeded --seed 9)'   # args: (none);    the config export is gone
```

— but the unrepresentability sentence is not realized: the shipped stubs keep state in an option
that `configure` fills and the accessor panics when unset (the trap seen three times). Configured
is enforced at the artifact level, convention at the handle level; the open project decision is
about closing exactly that.

**Zero-cost layer claim:** not measured. No identity middleware exists to measure with, no
benchmark compares a fused composition with and without a do-nothing layer, and codegen
determinism is also unverified. Logged as unevidenced at the participant's request.

**Last demos shown:** `only`'s position-vs-providers semantics works as specified
(`only eo9:text/text $ time.frozen --… $ hello` runs with `[0.000000000]`; moving the provider
left of the gate gets the pre-run refusal). The README-level deterministic environment with
`fs.memfs` traps from the shell (resource-owning providers cannot be configured via the binder
yet — recorded, parked decision), while the frozen+seeded two-thirds is the part that held up. The
audit blind spot: `describe fs.readonly $ cat` is import-for-import identical to `describe cat`,
so an interposed attenuator is invisible from the artifact's interface even though the attenuation
is behaviorally real.

Wrap-up questions asked: most under-specified (ranked); what to prove/test and in what form;
contradictions of the spec as written; what impressed; the single experiment/proof they would
require as a referee.

## Round 6 — wrap-up (participant's closing assessment, abridged)

Asked first that the audit blind spot be logged as a finding: "the reduced-authority story is
visible in the interface only for *dropped* authority, not for *attenuated* authority."

**Most under-specified, ranked:**
1. The equivalence relation behind every `≡` (must be at least interface equality plus behavioral
   equivalence, since contexts observe interfaces).
2. Instance identity under composition — one instance or two when one export serves two importers,
   and whether the action law preserves instance count; nothing but a comment in a worked example.
3. The relationship between the interface-level algebra (name-sets) and the component-level type
   system (nominal per-exporter resources) — i.e. when composition is actually *defined*.
4. The status of Γ and "modulo fusion" in the override law.
5. What "configured" means in a provider's type, and the sort of an unconfigured configurable
   provider.

**What to prove/test, and first:** first, a property test over *generated* component triples
(x, y, c) — generators covering resource types, types-sibling interfaces, multi-slot consumers,
and stateful configured providers — asserting (a) whenever the interface-level composition is
defined, the encoded artifact validates (else a typed refusal), and (b)
`(x & y) $ c ≈ x $ y $ c` observationally. Second, longer-term: a small mechanized core calculus
in which the action law, the sealing/residual equations, and the override law are theorems over a
defined equivalence.

**Contradictions of the spec as written (their list):** (a) the override law —
`time.frozen $ (time.fuzzy $ hello)` traps while the right-hand side runs; (b) the `X.none` law —
`fs.none $ cat` should be a no-op drop, instead fails validation; (c) "used before configured is
unrepresentable" — observed three times as a trap; (d) "the shell warns when a composed provider's
exports match nothing" — no warning, twice; (e) `rename` "applies to imports and exports alike" —
renaming a residual binary import yields an invalid artifact. The empty-provider identity and the
`with` precondition were classed as unimplemented/incoherent rather than contradicted.

**Genuinely good (their words, abridged):** sealing as absence of wiring — denial with no runtime
check to bypass; authority visible in `describe`; `only` refusing pre-instantiation with named
offenders and its gate-position semantics matching the spec; slots as real (name, type) pairs and
`rename` as a real re-encoding; `configure` visibly changing the export surface; the seed/overlap
test passing on both sides of the action law; and the project's gap-list culture.

**The one thing they would require as a referee:** the generative soundness-plus-action-law test
(2a/2b above) with stateful and resource-bearing providers in the generator — it would have caught
the middleware trap, the `fs.none` failure, and the rename bug, and it is the minimum evidence
that the algebra's laws are about the system rather than about its interface summary. "And either
measure or delete the zero-cost-layer claim."

---

## Findings

### Laws and claims exercised — what was run, and what actually happened

| Claim / law | Experiment | Result |
|---|---|---|
| Refusal before run (`only`) | drop a required interface from the allow-list | Refused pre-instantiation, names the offender; friendly message |
| `only` intersection / narrow-only | nested gates, both orders | Both refused; cannot widen from outside |
| `only` position-vs-providers | provider inside vs outside the gate | Matches spec (inside: satisfied+gated; outside: refused) |
| `$` sealing / innermost wins | `seed 1 $ seed 2 $ rng` vs references | Seed-2 stream; outer occurrence inert |
| `&` shadowing / rightmost wins | `(seed 1 & seed 2) $ rng` | Seed-2 stream |
| Action law on overlap | the two forms above + `describe` of both | Agree behaviorally and at the interface — law holds where tested |
| `$` non-associativity | `(frozen $ seeded) $ hello` vs the right chain | Observably different (wallclock vs frozen time), as the spec predicts |
| Sort restrictions | binary as left operand of `$` and `&` | Typed refusal ("left operand is not a provider") |
| Identity element | `empty $ rng` | Cannot be written: no `empty` in the store or the grammar |
| `X.none $ c ≡ c` (X not imported) | `net.none $ rng`, `fs.none $ hello` | Holds (interface + behavior); no unmatched-exports warning though |
| `X.none $ c` no-op when X is *required* | `fs.none $ cat` | **Fails**: raw "encoding produced a component that failed validation" |
| Override law (interposition over a composed source) | `time.frozen --… $ time.fuzzy --… $ hello` (and the `&` form) | **Fails**: traps with an opaque backtrace; same middleware over the host clock works |
| Configure as algebra | `describe entropy.seeded` vs `describe (entropy.seeded --seed 9)` | Configured artifact differs (config export gone, args consumed) |
| "Used before configure is unrepresentable" | unconfigured `time.frozen`/`entropy.seeded`/`fs.memfs` compositions | **Not realized**: guest panic → contained trap, raw backtrace |
| Slots are (name, type) pairs; `rename` is real | `describe fs.overlay`, `describe rename upper top $ fs.overlay` | Real named slots; rename genuinely relabels the artifact |
| `rename` on a residual binary import | `rename eo9:time/time wallclock $ hello …` | **Fails**: `CompileError::Codegen(… invalid leading byte … import name …)`; rename-both-sides+compose works |
| `with p as slot` | `with fs.memfs as upper, …` | Refused: "must export exactly one interface (it exports 3)" — true of every shipped fs provider |
| Two-slot wiring of independent providers | fs.overlay upper/lower | Not expressible today (nominal root-handle identity; WIT migration pending) |
| Deterministic environment (frozen+seeded) | repeat runs | Byte-identical, sealed against ambient |
| Deterministic environment incl. `fs.memfs` | shell composition onto `readwrite` | **Traps** (resource-owning configure unsupported; parked decision) |
| Zero-cost do-nothing layer | requested measurement | Not measurable on this build (no identity middleware, no benchmark); logged unevidenced |

### Spec-vs-implementation contradictions identified by the participant

1. Override law: interposition over an algebra-composed source traps; over the host context it
   works (round 4).
2. `X.none` no-op-drop law: `fs.none $ cat` fails component validation with a raw internal error.
3. "Used before configured is unrepresentable rather than a convention": it is a runtime panic
   today (three different providers observed).
4. "The shell warns whenever a composed provider's exports match nothing": no warning is emitted
   (observed twice, including the seed-1-shadowed case where a user would genuinely want it).
5. `rename` "applies to imports and exports alike": renaming a binary's import and leaving it
   residual produces an artifact the compiler rejects.
6. (Classed as incoherent/unimplemented rather than contradicted:) the identity law's `empty` has
   no spelling anywhere; the `with` sugar's precondition excludes every provider the project
   ships.

### Under-specified areas (participant's ranking)

1. The equivalence relation `≡` (must include interface equality, since contexts observe
   interfaces via `only`).
2. Instance identity / sharing under composition, and whether the action law preserves it.
3. When interface-level composition is *defined*, given nominal per-exporter resource types — the
   soundness obligation "interface-level defined ⇒ encoded component validates, else a typed
   refusal naming the split identity".
4. Γ and "modulo fusion" in the override law (currently an empirical claim that two code paths
   agree — the host-side close vs the algebra).
5. The type-level meaning of "configured"; the sort of an unconfigured configurable provider.

### "Prove it" / testing requests (participant)

1. **A generative property suite over component triples** (x, y, c) with resource types,
   types-sibling interfaces, multi-slot consumers, and stateful configured providers, asserting
   (a) interface-level-defined ⇒ encoder validates (else typed refusal) and (b) the action law
   observationally. Named as the single thing they would require as a referee; would have caught
   the middleware trap, the `fs.none` failure, and the rename bug.
2. A mechanized core calculus (longer-term) where the action law, sealing/residual equations, and
   override law are theorems over a defined equivalence.
3. An instance-identity discriminating experiment (needs an entropy-style middleware or a
   two-route stateful consumer in the store).
4. Measure the zero-cost-layer claim against a do-nothing middleware, or remove the claim.
5. State the equivalence relation in the spec; restate the override law as a loader-correctness
   obligation if it is to remain host-implemented.

### Criticisms / rough edges

- Interposition — the system's core pitch — currently only works against host-side providers; the
  guest-middleware-over-guest-provider chain traps with an opaque, name-stripped backtrace, and
  the shape is absent from the test suite.
- Failures of the algebra surface as raw internal strings (`encoding produced a component that
  failed validation`, `CompileError::Codegen(…)`) rather than typed diagnoses naming the cause.
- Default child policy is inherit-everything; least privilege is opt-in via `only` (policy
  observation, not an algebra defect).
- Silent dead providers: no warning when a composed provider's exports match nothing, which has
  real misuse potential (the seed-1-shadowed-by-seed-2 case).
- The `with` sugar is unusable against every shipped provider; multi-slot wiring requires explicit
  `rename` gymnastics and is then blocked by the type-identity issue anyway.
- `only` cannot express per-slot policy (by-type matching).
- No user-facing digest for a composed expression, so artifact-level identity claims cannot be
  checked from the shell.
- The README/spec deterministic-environment one-liner including `fs.memfs` does not run from the
  shell against stock programs.

### What landed well

- Sealing really is the absence of wiring; refusals happen before instantiation and name the
  offending imports; gate-position semantics match the spec exactly.
- The action law held at its most fragile observable point (overlapping stateful providers), with
  interfaces agreeing via `describe`.
- Non-associativity of `$` is real and was demonstrated observably, and the typed sort refusals
  ("left operand is not a provider") match the spec's kind discipline.
- Slots are genuine (name, type) pairs in the encoded artifact; `rename` is a real re-encoding;
  `configure` visibly changes the artifact's surface (config export gone once bound).
- Determinism by substitution (seeded entropy, frozen time) is real, repeatable, and sealed
  against the ambient session.
- `describe` / `env` / `env <program>` give a usable inspection story for granted-vs-refused
  authority, with types-only imports correctly marked as carrying no authority.
- The facilitator showing failures unprompted (the middleware trap, `fs.none $ cat`, the rename
  bug, memfs) and quoting the spec verbatim rather than paraphrasing was explicitly noted as
  increasing trust in what did work.

## Facilitator observations (gaps admitted / discovered during the session)

- Three previously unrecorded breakages were found live by following the participant's experiment
  designs: the configured-middleware-over-configured-provider trap (`time.fuzzy` over
  `time.frozen`/`time.monotonic-stub`, both `$` and `&` forms), the `fs.none $ cat` encoder
  validation failure (nominal types split), and the `rename`-residual-import codegen failure.
  None of these shapes appear in the integration suite.
- Had to admit: the `≡` relation is undefined; the identity element has no spelling; instance
  identity under composition is unspecified and untested; the override law is implemented as a
  separate host-side close rather than as composition; the unmatched-exports warning promised by
  the spec does not exist; the unconfigured-provider trap and the resource-owning-configure gap
  remain open decisions; the zero-cost-layer claim is unmeasured; codegen determinism is
  unverified; no expression-level digest surface exists.
- Had to admit the shipped WIT has not yet been migrated to the spec's just-adopted root-handle
  convention, so `fs.overlay` cannot be wired from independent leaves and the session filesystem's
  overlay semantics are host-assembled rather than algebraic.
- The participant's wrap-up framing — laws should be "about the system rather than about its
  interface summary" — is a fair summary of where the algebra's evidence stands today: strong on
  the interface-level laws that are tested, weak exactly where component-level type identity,
  state, and configuration interact.
