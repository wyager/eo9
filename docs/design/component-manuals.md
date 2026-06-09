# Component manuals and completion hints — design

Status: proposed (owner feature request, 2026-06-08). Companion to
docs/study/incremental-repl-for-eosh.md (its M3 is the consumption point for the
completion half).

## Summary

One primitive: a component may carry a **self-described manual** as a wasm custom
section named `eo9-manual` — versioned, line-oriented UTF-8, embedded by a guest-SDK
macro at compile time. Two consumers: (1) `man <name>` in eosh renders it (synopsis,
description, per-arg docs, examples, see-also) with graceful fallback to `describe`;
(2) the incremental REPL's M3 argument grammars consume the per-arg value hints
(literal enums, kind tags, doc lines) — strictly additively, under the superset rule.
No new WIT, no host/kernel change, no instantiation, no compile. v1 = guest SDK macro
+ eosh-core reader + xtask validation + authored manuals.

## Repo context this builds on

- `describe` is already decode-only (crates/eo9-component/src/describe.rs,
  Meta::from_bytes via wit_parser — no execution) but runs host-side behind
  eo9:exec/component-algebra.load, so it costs a resolve. ComponentInfo/ArgSpec
  mirrors in eosh-core/backend.rs; rendered by render.rs.
- eosh already holds raw bytes: Backend::resolve_with_bytes + the session BytesCache
  (cache.rs, 16 entries / 4 MiB) — a manual reader needs ZERO new OS surface.
- eosh-core is dependency-free by policy; the section walker is hand-rolled (~100
  lines of LEB128 framing — a container parser, never a validator).
- Doc-card precedent: builtins.rs cards + the "no builtin ships undescribed" test;
  apidocs.rs WIT-doc extraction + coverage test. Same voice, same discipline.
- Build pipeline: cargo build (wasm32) → wasm-tools component new → validate, with
  content-fingerprint stamps — a manual in the core module invalidates stamps for free.
- Composition pipeline: wac-graph nests operand bytes; rename/restrict copy unknown
  sections verbatim; the FUSED artifact has no top-level manual and "which nested one
  wins" is ill-defined → sidestepped by the naming rule below.
- Kind classification REJECTS extra exports on binaries (describe.rs) — a callable
  manual export would need a carve-out; real cost, counts against that option.

## 1. Transport: custom section (decided)

Custom section `eo9-manual`, alone, for v1. Vs a callable eo9:meta/manual export:
the section is a byte scan of already-cached bytes (microseconds; man must be instant
on the board where instantiation = seconds of codegen), works on sealed/denied/only-
restricted compositions, touches no WIT/kind-classification/spec. The export's only
advantage is dynamic docs, which nothing needs; it can be added later without breaking
the section path. Hybrid rejected for v1.

Physical location: `#[used] #[unsafe(link_section = "eo9-manual")] static` emitted by
the macro → custom section of the CORE module; wasm-tools component new preserves
unknown sections (it consumes only component-type*). The reader scans two levels:
outer component sections first, then depth-1 core-module sections; first hit wins.

Fused-artifact rule (one sentence, deterministic): `man <name>` reads the manual of
the NAMED artifact in /bin only — no expression evaluation, no part-chasing, no
let-bindings in v1. A saved composition has no top-level manual → falls back to
describe, which is honest (a composition's behavior is the algebra's, not one part's
prose).

## 2. Schema and authoring

Format: line-oriented UTF-8 (not CBOR/TLV — const-concat!-able in macro_rules with
zero deps, greppable over serial; at 1-3 KiB the size argument is noise).

Schema v1 (first line = magic + major version; readers accept major 1):

```
eo9-manual 1
name: telnetd
synopsis: serve eosh sessions over telnet, one fused task per session
description:
  Composes net.virtio $ net.l4.over-l2 $ net.text $ eosh, compiles it once,
  and serves sessions sequentially. SECURITY: cleartext, unauthenticated;
  trusted LAN / dev use only.
arg port u16 optional
  doc: TCP port to listen on (default 23)
arg nic string optional
  doc: the NIC provider to compose at the bottom of the stack
  kind: component-name
arg address string optional
  doc: IPv4 acquisition mode
  values: dhcp, static
example: telnetd --port 2323
  doc: serve on a non-privileged port under QEMU user networking
see-also: net.l4.over-l2, net.text, eosh
end
```

Rules: `arg <name> <type-text> <required|optional>` headers with indented doc:/
values:/kind: lines (at most one of values|kind); known kinds v1 = url, path,
component-name, interface-name, port (unknown kinds = display text only); type-text
is advisory — WIT ArgSpec.ty stays the mechanical truth and the renderer FLAGS
mismatches rather than trusting either; unknown keys skipped (fwd compat); `end`
terminates; second header before end = parse error (lld concat defense). Hard caps:
16 KiB section, 64 args, 16 examples, 120-byte lines; over-budget → "manual
malformed; showing describe".

Authoring: `eo9_guest::manual!` macro_rules macro beside bindings!/main! — authors
write structured fields; the macro concat!s canonical text + emits the static.
Rejected: doc.toml sidecar (splits the interface, xtask-coupled), rustdoc extraction
(proc-macro against the zero-dep discipline; copy-editing into manual! also forces
the user-facing register).

Validation: xtask, at componentize time — scan + parse with the same rules; malformed
fails the build. MANUALED_COMPONENTS list asserts presence for the retrofit set
(grows, never shrinks).

## 3. Trust and soundness (hard rules)

Self-reported, unverified. Two consumers, two rules:
- man is DISPLAY-ONLY: never parsed into resolution/composition/grants/caching/
  execution. Renderer strips all control bytes (no escape injection), wraps to the
  109-col budget, enforces caps. The WIT-mismatch flag keeps a lying manual from
  silently contradicting describe.
- Completion hints are ADDITIVE, NEVER RESTRICTIVE: the M3 grammar builder constructs
  value slots as union(wit_grammar(ArgSpec.ty), words(hint_literals)) — the WIT branch
  is unconditionally present; hints contribute only extra alternatives, TAB
  candidates, ordering, and description columns. No code path narrows an admissible
  set: a lying values: list can produce a false green (tolerable), never a false red.
  Test: the differential superset property gains a manual-fuzzing arm (adversarial
  manuals injected into the memo; admissibility never shrinks vs no-manual grammar).
- Kind tags map to candidate SOURCES, not constraints: component-name → the per-prompt
  dynamic vocabulary; url/path/port → labels + canned prefixes only. No fs walking
  (that's a follow-on with its own capability question).

## 4. eosh integration

man builtin (eosh-core only): parse.rs `man <word>` (bare word; expressions → "use
describe"); session.rs run_man dispatch ladder: builtin/operator card → eo9: API card
→ resolve bytes → extract/parse/render → absent/malformed → "no manual for <name>;
showing describe" + render_info → unresolvable → existing error. New
eosh-core/src/manual.rs: scanner (~120) + parser (~180) + renderer (~80), no_std +
alloc, no deps. help_lines gains man; man gets a builtins card (coverage test
enforces both).

Completion consumption: the M3 lazy memo becomes {ComponentInfo, Option<Manual>} —
manual parse is one extra pass over the same cached bytes. Invalidation rides the
existing BytesCache structural rules.

## 5. Retrofit plan (order; ~20-50 lines manual! each, mostly copy-editing)

telnetd (security warning + component-name kind) → net.l4.over-l2 (the flagship
values: dhcp, static) → net.rtl8125 → l2check/l4check (example lines are the payload)
→ draw → usb.ohci(+pci) → curl when its branch lands.

## 6. Milestones

- v1 (independent of REPL, lands first): schema (this doc), manual! macro (~100-150),
  eosh-core manual.rs (~350-400 + ~200 tests incl. fixture bytes, malformed cases,
  control stripping), man builtin (~120-160 + tests), xtask validation +
  MANUALED_COMPONENTS (~150-250; share a no-dep crates/eo9-manual parser if a fourth
  consumer appears), 5 authored manuals. Acceptance: man telnetd instant on QEMU +
  www/vm; saved-composition and manual-less fallbacks clean. ~900-1200 lines total.
  No WIT/host/kernel change; no board risk.
- v2 (with repl M3): memo extension + grammar-builder union + candidate descriptions
  (~150-300 in M3) + the manual-fuzzing superset arm (~100-200). Acceptance: TAB after
  `net.l4.over-l2 --address ` offers dhcp/static with doc lines; a lying fixture
  manual never reds a parse_command-accepted line.
- Deferred: callable manual interface, fs-walking completion, fused-artifact manual
  aggregation, localization.

## Workarounds / assumptions

1. link_section on wasm32 + component-new preservation asserted from ecosystem
  behavior, NOT yet proven here — the first manual-carrying component is the spike;
  fallback = ~30-line xtask byte-append of an OUTER custom section (reader already
  scans that level).
2. wac nesting claim pinned by a small host test (compose two fixtures, scan) so a
  wac upgrade can't silently change man's fallback.
3. curl not in tree yet (area/16 pending) — planned-for, not planned-in.
4. eosh-core gains a container parser, not a validator — framing only; malformed
  bytes degrade to "no manual".
5. Parser drift between xtask and eosh-core mitigated by a differential fixture test
  (or the shared crate).
