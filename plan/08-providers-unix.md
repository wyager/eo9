# 08 — Unix root providers (`crates/eo9-providers-unix`)

## Scope
The usermode equivalent of drivers: host-side implementations of the OS APIs backed by the host OS, wired
into the runtime's linker as the root context. These are trusted host code, not wasm components.

## Spec references
Each API section under Deliverables; "Execution APIs" (environments are just data; hardware roots),
"Eo9 API design" (owned-buffer round-trip, high concurrency), kill/linearity contract.

## Deliverables
- `eo9-providers-unix` crate implementing host traits the runtime (plan 04) links against:
  - `text`: std{in,out,err}.
  - `time`: wall + monotonic.
  - `entropy`: OS RNG (`getrandom`).
  - `fs`: rooted at a host directory (configurable); open/read/write/list/stat; `open-exec` returns an
    immutable handle — MVP: copy-on-open or O_RDONLY + hash-on-open; document the immutability guarantee and
    its limits on non-COW host filesystems.
  - `disk`: file-backed block device (a plain file or block dev path), offset read/write with owned buffers.
  - `net`: TCP connect/listen/accept/send/recv + UDP. MVP correctness over performance.
  - `perf`: stub or minimal counters.
- Completion model: every potentially-blocking op completes asynchronously into the task's completion queue
  (plan 04/05 types). MVP implementation may use a small blocking-thread pool; keep the interface such that
  an io_uring backend can replace it later without touching callers. No tokio.
- Kill behavior per API documented (abort vs complete) per the spec's linearity contract.

## Dependencies
01, 02, 04 (host trait shapes — agree these with plan 04 early), 05 (queue types). Consumed by 11, 13.

## Milestones
1. text/time/entropy (enough for I1).
2. fs + disk with async completions; immutable open-exec.
3. net; concurrency soak test with `many-reads`/`netcat-lite` (I3).

## Decisions
(record here)
