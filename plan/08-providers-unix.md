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
8. **open-exec immutability = clone-first snapshot to an anonymous file** (revised by the exec-copy-hardening
   follow-up). `open-exec` first attempts a zero-overhead COW clone of the source — `clonefile(2)` on
   macOS/APFS, the `FICLONE` ioctl (reflink) on Linux — into the provider's exec-copy directory, opens it,
   and immediately unlinks it, so the snapshot is reachable only through the handle's descriptor. What
   happens when the backend cannot clone is the provider's `ExecSnapshotPolicy`: `CloneOrRefuse` (default)
   fails `open-exec` with the existing `not-immutable` error — only filesystems that can promise a COW
   snapshot back execution (this also covers an exec-copy dir on a different volume than the source) — and
   `CloneOrCopy` (opt-in) falls back to a byte copy, which may capture a torn state if a writer races the
   open and costs O(file size). Guarantee in both cases: after `open-exec` completes, no modification,
   rename, truncation, or deletion of the original path by any process changes the bytes seen through the
   handle; a hostile host superuser is out of scope. The raw clone syscalls come from a direct `libc`
   dependency (pre-approved by the planner); the existing WIT `not-immutable` case fits the refusal exactly,
   so no WIT change is requested. The refusal path itself only triggers on non-COW hosts and is therefore
   not exercised by tests on this APFS machine.
8a. **Exec snapshot hardening (security-review follow-up).** Snapshot files are owner-only: created mode
    `0o600` atomically at open time on the copy and Linux-clone paths, and re-moded to `0o600` immediately
    after the macOS clone (the window is covered by the directory permissions). The default exec-copy
    directory is an unpredictably named `eo9-exec-<pid>-<random hex>` subdirectory of the system temp dir,
    created fresh, non-recursively, mode `0o700`, and construction fails if the path already exists (a
    squatted path is never adopted). Both the default and caller-supplied exec-copy directories are vetted
    via `lstat` before use: they must be real directories (not symlinks) owned by the current effective uid;
    the default must additionally be exactly `0o700`, while a caller-supplied directory keeps whatever mode
    its owner chose (documented in the constructor).
9. **disk.** File-backed block device (plain file or block-device node); size fixed at open (or supplied via
   `open_with_size` / `create`). Reads and writes must lie entirely within `[0, size)` or fail `out-of-range`
   before touching the file; `read_only` devices fail writes with `read-only`. I/O is `pread`/`pwrite`, so
   concurrent ops share no seek state.
10. **Kill/abort behavior (linearity contract).** No provider aborts an in-flight host op: it runs to
    completion on a provider thread, the completer receives the result (including the returned buffer), and
    a dead task's runtime drops it — a pre-kill write may still reach the backing file/device, a consumed
    stdin line is lost, a pending sleep fires and is discarded. Dropping a provider never cancels accepted
    work (the pool drains on drop; timer/reader threads finish what they accepted).
11. **Tests.** Unit tests per module (45 total) use a std-only tempdir helper rooted under `target/` — no
    network, no extra test deps. Coverage includes escape/symlink attempts, open-exec immutability against
    overwrite+delete (the COW-clone path on this APFS host), the copy fallback's content/mode/unlink
    behavior, snapshot and exec-dir permission checks, rejection of symlinked exec-copy dirs, read-only
    enforcement, out-of-range disk ops, and hundreds of concurrent completions.
12. **Deferred.** `net` (milestone 3), `perf` (surface still a placeholder in WIT), and the runtime/linker
    wiring (plan 04). Stub/attenuated flavors (`fs.memfs`, `time.frozen`, …) are guest-side providers
    (plan 09), not part of this crate.
