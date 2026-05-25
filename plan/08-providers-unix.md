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

1. **Runtime-agnostic core + thin host-trait layer.** The crate contains no wasmtime types. Each provider is a
   plain struct whose methods mirror the WIT functions, fronted by a small trait the runtime (plan 04) can
   link against or mock: `text::TextHost`, `time::TimeHost`, `entropy::EntropyHost`, `fs::FsHost` (plus
   `fs::FileHost` for the `file` resource and `fs::ImmutableHost` for `immutable-handle`), `disk::DiskHost`.
   The provider struct corresponds to the WIT `*-impl` root-handle resource; the `default()` export and all
   resource-table plumbing are the runtime's business. A small adaptation pass is expected when plan 04 fixes
   its final host-trait shapes.
2. **Completion model.** Every potentially-blocking op takes a caller-supplied `Completer<T>`
   (`Box<dyn FnOnce(T) + Send>`) and returns immediately; the provider invokes it exactly once from a
   provider-owned thread, on success and error alike. The MVP backend is a shared `BlockingPool` (2–8
   threads, drains its queue on drop) plus a dedicated timer thread for sleeps and a dedicated, detached
   stdin reader thread. The backend is private to each provider, so an io_uring-style submission backend can
   replace the pool without changing any caller.
3. **Owned buffers.** `OwnedBuffer` is the host-side value behind `eo9:io/buffers.buffer`: moved into the
   provider for the life of an op and returned inside the completion value on both success and error, per the
   spec's owned-buffer round-trip. Out-of-range accessor calls return an error for the runtime to turn into
   the WIT-specified trap.
4. **text.** `write` is synchronous (WIT shape) and flushes per call; `read-line` strips `\n`/`\r\n`, reports
   EOF as `Ok(None)`, and maps broken pipes to `closed`. Generic `from_streams` constructor for tests and
   output redirection; `stdio()` for the real streams.
5. **time.** Wall clock from `SystemTime` (negative seconds before the epoch), monotonic from `std::time::
   Instant` anchored at provider construction (per-boot arbitrary epoch per the WIT). `resolution` reports
   1 ns — the API granularity, not a claim about hardware; degradation is `time.fuzzy`'s job. `sleep` uses a
   deadline min-heap + condvar on one timer thread; pending sleeps still fire after the provider is dropped,
   so the "at least duration-ns elapsed" contract always holds.
6. **entropy.** Backed by the `getrandom` crate (0.3) — the standard, tiny binding to `getentropy(2)`/
   `getrandom(2)`; 0.3 rather than 0.4 because 0.4's wasm support drags a large wit-bindgen tree into the
   lockfile. The WIT surface has no error path, so an OS RNG failure panics rather than degrading silently.
7. **fs rooting and containment.** The provider root is canonicalized at construction. Guest paths are
   normalized lexically (`..` and prefixes → `denied`; leading `/` = provider root), then the existing path —
   or, for creation targets and `remove`, its parent — is canonicalized and must stay under the root, so
   in-tree symlinks work but cannot escape. `remove` never follows the final component (it removes a symlink,
   not its target). Known limit (documented in the module): canonicalize-then-operate is not atomic, so a
   racing host-side actor could still swap a path component; the proper fix is a per-component `O_NOFOLLOW`
   walk or `openat2(RESOLVE_BENEATH)`, deferred past the MVP since the usermode root is chosen by the same
   trusted host user.
8. **open-exec immutability = copy-on-open to an anonymous file.** The source is copied into a uniquely named
   file in a provider-configured exec-copy directory (system temp dir by default) which is immediately
   unlinked, so the snapshot is reachable only through the handle's descriptor. Guarantee: after `open-exec`
   completes, no modification, rename, truncation, or deletion of the original path by any process changes
   the bytes seen through the handle. Limits on a non-COW host fs: the copy is not atomic against a writer
   racing `open-exec` itself (the handle is immutable afterwards but may capture a torn state), it costs
   O(file size) per open, and a hostile host superuser is out of scope. `not-immutable` is therefore never
   returned; a COW/content-addressed backend (APFS `clonefile`, Linux reflink, the store) can later make the
   snapshot O(1) without changing the guarantee.
9. **disk.** File-backed block device (plain file or block-device node); size fixed at open (or supplied via
   `open_with_size` / `create`). Reads and writes must lie entirely within `[0, size)` or fail `out-of-range`
   before touching the file; `read_only` devices fail writes with `read-only`. I/O is `pread`/`pwrite`, so
   concurrent ops share no seek state.
10. **Kill/abort behavior (linearity contract).** No provider aborts an in-flight host op: it runs to
    completion on a provider thread, the completer receives the result (including the returned buffer), and
    a dead task's runtime drops it — a pre-kill write may still reach the backing file/device, a consumed
    stdin line is lost, a pending sleep fires and is discarded. Dropping a provider never cancels accepted
    work (the pool drains on drop; timer/reader threads finish what they accepted).
11. **Tests.** Unit tests per module (40 total) use a std-only tempdir helper rooted under `target/` — no
    network, no extra test deps. Coverage includes escape/symlink attempts, open-exec immutability against
    overwrite+delete, read-only enforcement, out-of-range disk ops, and hundreds of concurrent completions.
12. **Deferred.** `net` (milestone 3), `perf` (surface still a placeholder in WIT), and the runtime/linker
    wiring (plan 04). Stub/attenuated flavors (`fs.memfs`, `time.frozen`, …) are guest-side providers
    (plan 09), not part of this crate.
