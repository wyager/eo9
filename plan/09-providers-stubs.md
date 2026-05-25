# 09 — Standard stub providers (`guest/stubs/*`)

## Scope
The hand-written stub/virtual providers from the spec's "Standard stubs" lists — small wasm components, one
crate each, composable with `$`/`&`/`with`.

## Spec references
"The capability algebra" (none/deny/stubs table and rules), per-API "Standard stubs" lines, "Environments
and the `&` operator" (the deterministic-environment example), Security (time.fuzzy).

## Deliverables (priority order)
1. `*.none` for every API (exports the `-optional` flavor answering `none`) — tiny, mechanical to write by
   hand, needed by `only`'s story and the loader rule.
2. Deterministic set: `fs.memfs`, `time.frozen`, `time.monotonic-stub`, `entropy.seeded`, `disk.mem` —
   together these make the deterministic environment of integration milestone I2.
3. Attenuators/refusers: `net.deny`, `net.loopback`, `fs.readonly` (imports fs, re-exports it read-only —
   first real middleware provider), `text.null`, `time.fuzzy` (jittered/quantized).
4. Later (needs Message API): `text.capture`.
- Each stub: targets its stub world from `wit/` (plan 02), takes `configure` args where the spec implies
  config (e.g. `entropy.seeded --seed`, `fs.memfs` size), ships with a compose-and-run test against an
  example program.

## Dependencies
02, 07 (provider-authoring support). Consumed by 10, 13, and the I2 milestone.

## Milestones
Match the priority order above; (1)+(2) unblock I2.

## Decisions
(record here)
