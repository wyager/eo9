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
(record here)
