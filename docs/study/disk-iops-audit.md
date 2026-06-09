# Disk-IOPS audit (area/25-io-audit) — 2026-06-08

## Verdict

**No.** The usermode disk path delivers **~9,000–12,600 IOPS at every queue depth from 1 to 1024** on cache-cold 4 KiB random reads — queue depth buys *zero* overlap because every read completes synchronously on the drive thread. The same image through the same machine's native parallelism does **209k IOPS at 14 threads (still climbing)**, and even the project's own existing-but-unwired `BlockingPool` path does **130–153k**. The guest is 12–23× under the machinery the repo already contains, and "tens of thousands of concurrent IOPs" today means tens of thousands of *queued* ops draining at device QD1. The WIT header's claim that the API is "Designed to scale to millions of concurrent read/write ops" (`wit/disk/disk.wit:7-9`) is true of the API *shape* and false of every shipped backend.

## Part 1 — Audit findings (file:line)

**The bottleneck: the usermode disk adapter is eager-blocking.** `crates/eo9/src/providers.rs:188-279` — `HostDisk` implements the runtime's async `DiskProvider` trait by calling `read_blocking`/`write_blocking` (synchronous `pread`/`pwrite` on the calling thread) and returning an already-ready future (`ready_op`, line 259-267). The `BlockingPool` is opened (line 235) but **never used for disk I/O**. Documented as deliberate (lines 182-187): the eofs consumer's synchronous engine (plan/14 D15) required eager completion. Consequence: every disk read of every task serializes on the single drive thread; the device never sees more than one outstanding request.

**The pool path exists, works, and is dormant.** `crates/eo9-providers-unix/src/disk.rs:251-289` — `DiskHost::read/write` submit to a `BlockingPool` (`pool.rs:21-77`): 2–8 worker threads (`with_default_size`, clamp at `pool.rs:56-62` — 8 on this 14-core machine), one shared `mpsc` channel behind a `Mutex<Receiver>` (`pool.rs:96-104`), one `Box` allocation per submitted job. Single FIFO queue, no per-device queues, no priority.

**Per-read copy count (usermode, end-to-end):** ① device → kernel page cache (DMA); ② page cache → host `Vec` (`pread`, `disk.rs:259`); ③ host `Vec` → fresh `Vec` on guest access (`buffer.read` does `bytes[range].to_vec()`, `link.rs:472-482` / `buffer.rs:75-78`); ④ `list<u8>` → guest linear memory (canonical-ABI lower). **Three host-side copies per 4 KiB read**, plus a zero-fill memset at buffer creation (amortizable by reuse). The owned-buffer round-trip itself (`BufferTable::take/restore`, `task.rs:408-433`) is a pointer move — free.

**Per-call async machinery:** one in-flight read = one wasmtime-45 subtask via `func_wrap_concurrent` (`link.rs:884-906`), two `accessor.with` store-lock dances, buffer take/restore, and completion delivery through the store's event loop, woken by the per-task `Doorbell` (`task.rs:153-217`); the embedder drive loop donates fuel in 1M slices (`run.rs:23,182-208`, `FUEL_QUANTUM=10_000` at `task.rs:71`). Measured cost: **~0.7 µs/op on top of the 0.72 µs native pread** (1.42 µs total, see table). Serialization points: the single drive thread (everything), the store (all completions), the pool's one mutex'd queue (if it were used).

**Bounds on in-flight ops per task:** buffer budget, not table or executor — `MAX_TOTAL_BUFFER_BYTES = 64 MiB`, `MAX_BUFFER_BYTES = 16 MiB` (`task.rs:331-337`) → max 16,384 in-flight 4 KiB buffers. Empirically confirmed: depth 16384 × 4 KiB = exactly 64 MiB runs without trapping (the `>` check at `task.rs:366` admits the boundary). `BufferTable::alloc` is a linear free-slot scan (`task.rs:374-377`) — O(n), irrelevant at current scale. No wasmtime subtask-count limit was hit at 16,384.

**Batching: none.** `wit/disk/disk.wit:57-61` — `read`/`write` are strictly one-op-per-call. Same for `eo9:fs` (`wit/fs/fs.wit`). One WIT call = one subtask = the full per-call cost.

**The fs-over-disk path is worse than serial — concurrency is an *error*.** `guest/stubs/fs-eofs/src/lib.rs` ("Operations are serialized" bullet, ~line 44-48): the eofs engine is single-state; a concurrently delivered fs op fails typed-`io("the filesystem is busy")`. And each fs op is several serial disk ops (tree walk + commit), so fs IOPS over a disk image is a small fraction of the already-pinned disk IOPS.

**QEMU/metal path:** `kernel/eo9-kernel/src/virtio_blk.rs:1-80` — the kernel's storedisk driver is explicitly "polled, single-request" with `QUEUE_SIZE=16` declared but one 4 KiB bounce buffer (`REQ_BYTES=4096`, line 80); the guest `disk.virtio` stub mirrors the same shape. Device QD1 on the board too. (Not benchmarked this session — usermode only; noted as remaining work.)

## Part 2 — Benchmarks

Machine: 14-core Apple Silicon, 36 GiB RAM, APFS on internal NVMe, macOS. 4 KiB random reads over a 2 GiB non-sparse image, splitmix64 offset streams. Release builds (eo9 binary, hostbench, wasm32 release guest). **Cold** = fresh seed per run (cold fraction ≈ 88% at first run, decaying ~2-4%/run as runs accumulate — stated, not hidden). **Hot** = re-run of a just-warmed offset set (macOS evicts under pressure; every hot row was warmed immediately beforehand — earlier unwarmed "hot" runs were quietly half-cold and have been discarded; `sample` profiling confirmed the discarded runs' time was in `pread`).

**Guest (`diskbench` component via `eo9 --disk … run`), 4 KiB random reads:**

| depth | COLD IOPS | COLD p50/p99 | HOT IOPS | HOT p50/p99 |
|---|---|---|---|---|
| 1 | 8,991 | 92 µs / 553 µs | 688,336 | 1.25 µs / 3.8 µs |
| 8 | 8,135 | 93 µs / 1.05 ms | 716,927 | 1.25 µs / 1.7 µs |
| 32/64 | 11,136 | 91 µs / 565 µs | 716,784 | 1.25 µs / 1.8 µs |
| 256 | 12,592 | 91 µs / 401 µs | 392k–752k* | 1.4 µs / 3.2 µs |
| 1024 | 12,387 | 90 µs / 548 µs | 703,796 | 1.25 µs / 1.8 µs |
| 4096 | — | — | 591,079 | 1.4 µs / 2.9 µs |
| 16384 | — | — | (17,426)† | (84 µs)† |

\* run-to-run variance from background eviction; the clean pre-warmed sweep is the 688–717k band. † depth 16384 survived the machinery (buffer budget exactly at the 64 MiB cap) but the number measures my bench's O(depth) join combinator (~5 ns × 16384 ≈ 82 µs/op), not the host — reported as a machinery-limit data point, not a host ceiling.

**Host-side (same offsets, no wasm):**

| path | config | COLD IOPS | COLD p50 | HOT IOPS | HOT p50 |
|---|---|---|---|---|---|
| `eager` (today's adapter path) | 1 thread | 13,347 | 90 µs | 1,317,747–1,379,972 | 0.7 µs |
| `pool` (existing, unwired) | 8 thr, d=8 | 130,041 | 88 µs | 340,790 | 22 µs |
| `pool` | 8 thr, d=32 | — | — | **780,830** | 40 µs |
| `pool` | 8 thr, d=256 | **153,292** | 1.56 ms | 752,430 | 334 µs |
| `pool` | 8 thr, d=1024 | — | — | 818,033 | 1.24 ms |
| `native` pread | 1 thread | 14,725 | 86 µs | 1,289,868 | 0.7 µs |
| `native` | 4 threads | 60,982 | 86 µs | — | — |
| `native` | 8 threads | 127,753 | 86 µs | 1,758,087 | 1.7 µs |
| `native` | 14 threads | **208,801** | 94 µs | 892,790 | 9 µs |

CPU: guest cold runs ≈ 0.1 s CPU per 1.1 s wall (drive thread idle in `pread` waits); guest hot 20k ops ≈ 0.04 s total. Pool cold: ~70–110 ms sys per 10k ops (thread handoff ≈ 7 µs/op round trip at depth 1).

**Named bottlenecks, in order of bite:**
1. **Eager adapter** (`providers.rs:259`) — pins device QD to 1: cold ceiling 9–13k IOPS at any guest depth. *This single seam is the whole answer to the owner's question.*
2. **Pool thread count + single mutex'd queue** (`pool.rs:56,96`) — next ceiling once wired: cold ~150k (8 threads × ~90 µs, NVMe coalescing helps), hot ~800k; queue latency grows linearly with depth past 32 (Little's law on one queue).
3. **Per-call component-async cost** — ~0.7 µs/op over native (subtask + store accessor + take/restore): caps any single task at ~700k hot IOPS/core regardless of backend; this is what batching amortizes.
4. **Guest-access copies** — `buffer.read` `to_vec` + ABI lower = 2 copies per consumed read (my bench excludes them; real consumers pay them).
5. **fs.eofs serialization** — concurrent fs ops fail typed-busy; the fs surface cannot express concurrency at all today.
6. **virtio QD1** on the QEMU/board path.

## Part 3 — Gap analysis and proposal sketch (design notes, no code)

**Batched WIT extension.** Add to `eo9:disk` (and mirror in `eo9:fs` later):

```wit
record read-req { offset: u64, dst: buffer }       // owned buffers in
record completion { index: u32, dst: buffer, result: result<read-result, read-error> }
submit: async func(dev: borrow<disk-impl>, reqs: list<read-req>) -> list<completion>;
// or split-phase for pipelining:
submit: func(dev: borrow<disk-impl>, reqs: list<read-req>) -> batch-id;
reap:   async func(dev: borrow<disk-impl>, batch: batch-id, min: u32) -> list<completion>;
```

What it amortizes, with measured coefficients: one subtask + one lower/lift per *batch* instead of per op — at batch 32 the 0.7 µs/op machinery cost drops to ~22 ns/op, moving the hot single-task ceiling from ~0.7M toward the native 1.3M; and it hands the backend a whole submission window at once, so the device sees QD=batch instead of QD=1 without 16k live guest futures (the depth-16384 run shows guest-side scheduling of huge future sets is its own O(depth) tax — batching replaces that with one array). The split-phase form additionally lets the guest overlap building batch N+1 with reaping batch N. Semantics to pin: per-op independent results (no all-or-nothing), completion order unspecified (`index` correlates), buffer ownership transfers per-op exactly as today — the owned-buffer round-trip generalizes cleanly to lists since `list<buffer>` is just a list of host-table moves.

**Composition with call-gate sharing** (`docs/design/shared-resources.md`): a gate call costs two host-call boundary crossings + queue/translate bookkeeping + up to one drive-loop pass of latency (§3.2, §8) — *per call*. A batched `submit` through a gate pays that once per batch: at batch 32 the gate's µs-range per-call cost lands at ~30 ns/op, which is what makes a *shared* eofs/disk owner (the §5.3 "eofs once multiple writers exist" candidate) viable at high IOPS rather than only for interactive traffic. Buffers already cross the gate as host-table entry moves, not byte copies (§3.2 "honest cost accounting"), so a batch of 256 owned buffers is 256 table moves — no new copy class. The batch is also the natural unit for the §3.4 lock domains: one whole-instance lock admission per batch, not per op.

**Host backend ladder (honest sizing per rung):**
- **Rung 0 — wire the pool that exists** (`HostDisk::read` → `DiskHost::read` + the `oneshot` bridge already used by `HostFs`): cold 9–13k → **130–153k IOPS** (measured, this machine). Cost: small adapter change *plus* the real work — eofs D15: the sync engine path must keep eager semantics or eofs must finish moving to `AsyncEofs` everywhere; that constraint is why the adapter is eager, and un-eagering it without that is a regression for eofs mounts. Days, dominated by the eofs story.
- **Rung 1 — batched WIT + deeper pool with a submission queue** (per-device queue, more threads for rotational-free media, completion coalescing: one doorbell ring per drain, not per op): hot per-op machinery ~0.7 µs → tens of ns; cold scales with threads toward the native curve (61k@4t → 128k@8t → 209k@14t, unsaturated). Sizing: WIT + linker + provider ≈ 1–2 weeks including the guest SDK helpers.
- **Rung 2 — io_uring on Linux hosts**: true async submission, QD256+ without 256 threads; macOS has no equivalent (POSIX AIO on darwin is a kernel thread pool — the rung-1 pool *is* the macOS endgame, stated plainly). Only worth building when a Linux usermode deployment exists. ~1 week behind the rung-1 interface (the pool already hides behind `Completer`, `pool.rs:1-12` says exactly this).
- **Rung 3 — virtio-blk multi-queue on the board**: replace the single-request polled driver with a real ring (the declared `QUEUE_SIZE=16` is already allocated) + interrupt-driven completion + the batched WIT mapping ~1:1 onto descriptor chains. This is where `submit(list)` stops being an optimization and becomes the device's native shape. Sizing: the largest rung; board-lane margins apply.

**What I did *not* find:** no wake storms (resume donations stay at ~7 per 5k ops at every depth — the doorbell batches well); no allocation churn visible at current scale; no wasmtime table/subtask limit below 16,384 in-flight.

## Workarounds / assumptions (per the standing rule)
1. **Cache-cold discipline without root:** no `sudo purge`; cold = fresh-seed offsets over a 2 GiB image, cold fraction decaying ~88%→~65% across the session — cold IOPS figures are therefore slight *over*estimates, which only strengthens the verdict.
2. **Hot-number eviction hazard:** macOS evicted supposedly-warm pages between runs (caught by `sample` showing `pread` dominating a "hot" run); all reported hot rows were re-warmed immediately before measurement. Two early anomalous tables were discarded.
3. **depth=16384 guest row measures my bench combinator** (O(depth) poll-all join), not the host — flagged inline.
4. **hostbench `pool` latency** includes completion-channel delivery to the issuing thread (µs-scale; immaterial).
5. **QEMU/virtio benchmark not run** (usermode-first per the brief; time went to controlling cache state). The virtio QD1 claim is from code reading (`virtio_blk.rs:4-7,74-80`), not measurement.
6. **`guest/Cargo.lock` churns when diskbench builds** (the `examples/*` workspace glob picks it up); I reverted the lockfile and committed only new files — noted in `bench/README.md`.
7. **Native baseline is a thread pool, not io_uring** — macOS limitation, stated in the table and the ladder.

Committed: `f34c82b` on `area/25-io-audit` (worktree `.claude/worktrees/io-audit`) — `bench/` (hostbench + README + .gitignore; the 2 GiB image and target/ are ignored, not committed) and `guest/examples/diskbench/`. No existing code touched; `/dev/cu.usbserial*` untouched.
