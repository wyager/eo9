# Incremental parser-combinator REPL for eosh — study + design

(Study of github.com/wyager/audio2 code/repl @ 4402578, 2026-06-08. Study clone at
.claude/study/audio2, read-only, never built. Full report follows.)

## Verdict
Reimplement the combinator layer on alloc; recycle the theory and the small
invariant-bearing pieces verbatim: charset.rs (u128 ASCII bitset — the red-highlight
primitive), the step/admissible trait shape with the hard_required ontology, the
forced-prefix TAB walk (~25 lines), and the exhaustive admissibility sanity check as
a property test. The bulk of the original is no_alloc contortion (monomorphized sums,
const capacities, defmt plumbing) + an optional display compressor — with alloc the
rewrite is SMALLER than the original. License note: audio2's LICENSE.txt is a joke
non-license; verbatim carries need Will's one-line relicensing note at commit time.

## The soundness rule (the one invariant)
The incremental grammar is an editor aid; its language must be a SUPERSET of what
parse_command accepts. Red only when NO viable parse continues; false green is
tolerable, false red never. Execution keeps the battle-tested lex/parse path
unchanged. Enforced by a differential host property test over the parser corpus.

## Structural findings
- eosh today is line-at-a-time on every transport; the kernel read-line provider owns
  echo/backspace/history. Per-keystroke needs a new `read-key` WIT (variant key:
  char|enter|backspace|tab|up|down|left|right|ctrl|eof) with read-line fallback —
  zero regression on dumb transports.
- The editor moves INTO eosh-core (it alone sees /bin, let-bindings, describe);
  the usermode host editor + hand-synced completer (790 lines, "must be kept in
  sync") retire at parity — one editor for every transport.
- WIT-aware argument completion is CHEAP to reach: describe() already returns
  ArgSpec{name, ty}; the monadic Bind continuation swaps in the arg grammar after a
  name resolves (lazily, memoized). This is the genuinely novel payoff.
- Dynamic vocabulary = builtins ∪ reserved ∪ session bindings ∪ /bin listing,
  snapshotted per prompt.
- Backspace = reparse-from-start (microseconds at ≤4096 chars); audio2's
  snapshot-per-char stack is the fallback if profiling demands.
- Policy delta vs audio2: accept-and-mark-red (SGR 31), not beep-refuse.
- fbcon: use INVERSE VIDEO (SGR 7/27) as the inadmissible marker on the fb path —
  red specifically can render wrong-hued under the boot-state-dependent HDMI chroma
  issue; inverse is colorspace-proof. Serial/telnet keep SGR 31.
- Telnet per-key needs net.text WILL ECHO+SGA character mode (~150-250 lines,
  optional follow-on).
- Board per-key feedback is gated on the UART RX-IRQ kernel GAP (input currently
  arrives via idle-backstop scavenges) — M2 board acceptance deferred behind it;
  QEMU/usermode unaffected.

## Milestones
- M1: alloc parser core + v1 eosh grammar + host tests (differential superset +
  admissibility check). Lands independently, no behavior change.
- M2: read-key WIT + kernel/usermode providers; eosh-core editor (red mark,
  backspace-reparse, TAB completion, capped recall unified with session history —
  fixes the unbounded-history GAP); QEMU byte-level probes; retire host editor.
- M3: WIT-aware argument completion (ArgSpec-derived flag/value grammars).
- Follow-ons: net.text character mode, fbcon SGR subset, MikroTik-style full
  prediction line.

## Sizing
Core ~600-900 lines + ~400 tests; v1 grammar ~300-500; editor ~300-400; WIT+providers
~250; M3 ~300-500. eosh surgery proper is one function swap with capability check.

(Full file-by-file recycle table, risks, and the audio2 architecture notes live in
the study agent's report — transcript 2026-06-08; key risk: Bind::admissible is a
documented approximation in the source — port the exhaustive checker, prefer
hard_required LHSs at value-dependent binds.)
