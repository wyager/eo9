# 14 — Native filesystem `eofs` (`crates/eofs-core`, guest provider)

## Scope
Eo9's bundled filesystem: ZFS-flavored copy-on-write, Merkle-hashed, append-only-update, snapshotting, with
block compression on by default — delivered as an ordinary provider over the Disk API so the same component
runs on bare metal and under usermode Eo9 (file-backed or in-memory disk).

## Spec references
SPEC.md "Filesystem API" (the native-filesystem paragraph is the contract), "Disk API" (raw block device),
"Eo9 API design" (owned-buffer round-trip, concurrency), "Loading is immutability-first" + "The module store
and compilation cache" (hash/immutability synergy), "Packaging and submodules" (`eofs.mkfs` as a sibling
binary world).

## Deliverables
- `crates/eofs-core` — `#![no_std]` + `alloc`, target-independent (usable from host tests, the guest
  provider, and the kernel): on-disk format + read/write engine over an abstract `BlockDevice` trait.
  - `FORMAT.md` in the crate: superblock/uberblock pair with atomic root flip, block pointers carrying
    (physical location, logical/physical size, codec tag, blake3 hash), file block trees, directory format,
    snapshot roots, allocator/space-map, versioning rules.
  - Semantics: never overwrite in place; transactions commit by root flip; crash consistency by construction;
    snapshots = retained roots; deferred reclamation of unreferenced blocks; per-node hashes all the way up
    (the spec's hash feature and `open-exec` immutability are structural).
  - Compression **on by default**: lz4 (pure-Rust `lz4_flex`, block format, no_std — pre-approved dependency;
    escalate if its no_std story doesn't hold) with store-raw fallback for incompressible blocks; codec
    tagged per block so fast-zstd can be added later without a format change. Hashing: `blake3` (pin table).
  - Fixed block size (default 4 KiB, recorded in the superblock). MVP non-goals: multi-device/RAID, dedup,
    quotas, encryption, online GC (a manual GC entry point is fine).
  - An in-memory `BlockDevice` for tests; a `verify()` walk (check every reachable block against its hash).
- Later milestones: the guest provider component (imports `eo9:disk` + time/entropy, exports `eo9:fs`
  including hash queries and `open-exec`), an `eofs.mkfs` sibling tool, usermode end-to-end, kernel adoption
  (replaces plan 12's packed read-only store image).

## Dependencies
01, 02 read-only; 07's provider-authoring support for milestone 2; 04/11 for milestone 3; 12 for milestone 4.
Milestone 1 depends on none of the in-flight work.

## Milestones
1. **Core library + format.** `FORMAT.md`, eofs-core with create/mount/read/write/mkdir/list/stat/remove,
   transactions, snapshots, compression, hashing, `verify()`; property tests plus simulated-power-cut
   crash-consistency tests over the in-memory device (cut at arbitrary block-write boundaries, remount, fsck
   must pass and committed data must be intact); `cargo check --target aarch64-unknown-none` documented and
   clean.
2. **Provider component + `eofs.mkfs`** (with plan 07's provider support).
3. **Usermode end-to-end** over `disk.mem` and the file-backed disk (with plans 04/11); store-on-eofs
   evaluation with plan 06.
4. **Kernel adoption** (plan 12): boot disk formatted as eofs, read-only first, then read-write.

## Notes / constraints
- Keep eofs-core free of wasm/wasmtime/OS types; all I/O goes through the `BlockDevice` trait (sync trait in
  milestone 1 is fine — the async wiring belongs to the provider layer).
- Determinism: given the same operation sequence and config, the produced image bytes should be identical
  (no wall-clock or RNG in the core path unless injected) — this keeps image-based tests and the compile-cache
  philosophy consistent.
- New Cargo manifests carry `license = "MIT"`; keep `cargo run -p xtask -- ci` green.

## Decisions

Milestone 1 (`crates/eofs-core`; the on-disk format is described in `crates/eofs-core/FORMAT.md`):

1. **`BlockDevice` is byte-addressed** (`read_at`/`write_at`/`flush` on byte offsets), the same shape as
   `eo9:disk`, so the milestone-2 provider is a thin bridge. eofs assumes no write atomicity at all — torn
   writes (including torn uberblocks) are handled by checksums, and commit ordering is the only requirement.
2. **Uberblock pair at fixed offsets 0 and 4096** (slot size fixed at 4 KiB regardless of the filesystem block
   size); data region starts at 8192. Commit alternates slots by `txg mod 2`; mount picks the valid slot with
   the highest txg. The live root and the snapshot-table reference live directly in the uberblock.
3. **Everything is a byte object** (file contents, serialized directories, the snapshot table): data blocks of
   `block_size` under indirect blocks of 56-byte block pointers. Directories are sorted entry lists (name,
   kind, child object reference); no inodes, no hard links. Snapshots are entries in the snapshot-table object
   holding a retained root.
4. **Block pointers carry (addr, logical size, physical size, codec tag, blake3-of-logical-bytes).** Hashing
   the logical bytes makes hashes codec-independent and lets every read verify what it returns; `verify()` is
   the same check over every reachable block. Node hashes (exposed via `stat`) are the Merkle roots; for
   multi-block nodes they depend on physical layout (see FORMAT.md "Hashing") — whether the milestone-2
   `eo9:fs` hash queries need a content-only hash (extra field, format v2) is an open question for the planner.
5. **Allocation** is append-at-frontier with allocation-unit granularity (default 512 B) so compressed blocks
   actually save space; `gc()` is the manual deferred-reclamation entry point (walks all retained roots,
   builds an in-memory free list that is consumed first-fit; the free list is not persisted). Writes rebuild
   the changed object's indirect tree rather than patching single pointers — simpler, same format, more write
   amplification; acceptable for the MVP.
6. **Compression defaults to lz4** (`lz4_flex`, block format, `default-features = false` + safe encode/decode;
   added to the root pin table). Blocks that do not shrink are stored raw with codec tag 0. The per-filesystem
   default codec is fixed at format time and recorded in the uberblock.
7. **blake3 in no_std mode**: the root pin was changed to `default-features = false` (the hashing API other
   crates use is unchanged); eofs-core additionally sets `no_neon` for `cfg(target_os = "none")` targets only,
   because blake3's aarch64 NEON kernels are C and need libc headers that bare-metal targets lack.
   `cargo check -p eofs-core --target aarch64-unknown-none` is clean and documented in the crate manifest.
8. **Transactions are explicit**: operations stage copy-on-write state in memory (new blocks are written
   immediately, the root flip is not), `commit()` is the only durability point, `unmount` discards uncommitted
   changes. Crash consistency is tested by a power-cut simulator (`CutDevice`) cutting at every write boundary
   of a multi-transaction scenario, with torn final writes, then remount + `verify()` + exact state comparison.
9. Test-support devices (`MemDevice`, `CutDevice`) live in the crate itself so the provider, tools, and other
   areas' tests can reuse them.
10. **Hostile-image hardening**: object references are validated before any allocation or walk (size bounded by
    the device, metadata objects capped at 16 MiB, tree level must match the canonical height, data-block count
    bounded during the walk), and the verify/GC directory walks are iterative with a visited set — so corrupted
    or adversarial images fail with `Corrupt` instead of unbounded allocation, fan-out, or recursion.

Deferred to later milestones: usermode end-to-end over a *persistent* (file-backed/host) disk and
store-on-eofs (M3), kernel adoption (M4), plus content-only node hashes, holes/sparse files, rename, and
persistent free-space maps if they turn out to be needed.

Milestone 2 (`guest/stubs/fs-eofs` → the `fs.eofs` component; tests in
`tests/eo9-integration/tests/eofs.rs`):

11. **The provider is a thin bridge, exactly as D1 intended.** `fs.eofs` targets a crate-local world
    (`eo9:fs-eofs/eofs`: `import eo9:disk/disk`, `export eo9:fs/fs`) and implements `BlockDevice` over the
    imported disk; the whole filesystem is the unmodified `eofs-core` engine (no eofs-core changes were
    needed). `eofs-core` is now in the guest workspace pin table and builds for wasm32 unchanged.
12. **mkfs = `Eofs::format` + format-on-first-mount; no separate `eofs.mkfs` tool yet.** The provider's
    documented default (it has no configure interface, per the option-C rule): first use mounts the disk if
    either uberblock slot carries the eofs magic, and formats a *blank* device (no magic anywhere) in place
    with the default options; a device that has the magic but fails to mount is never reformatted — the error
    surfaces instead of becoming data loss. Host-side tooling formats with `Eofs::format` directly. A
    standalone `eofs.mkfs` sibling tool (and an `eo9 mkfs.eofs <file>` CLI hook) is deferred until there is a
    persistent host disk to point it at (M3).
13. **disk.mem gained its documented default** (16 MiB, zero-filled) so the canonical chain
    `disk.mem $ fs.eofs $ program` runs with no `configure` anywhere — it previously trapped when
    unconfigured, which the default-configuration rule (plan/09 D14) already said it must not.
14. **Provider semantics** (documented in the crate): every mutating operation (`open` that creates or
    truncates, `write`, `create-directory`, `remove`) ends with an eofs commit — durable and crash-consistent,
    at the cost of write amplification (batching is a later refinement). Paths follow the same
    `/`-separated, `.`/`..`-normalizing rules as fs.memfs. Open files are *path references* (removing a file
    invalidates its open handles with `not-found`, unlike memfs's unlink semantics); `open-exec` snapshots
    contents at open time (honest immutability by copy; pinning the Merkle object is a recorded refinement).
    Truncate is remove + recreate (the engine has no truncate primitive yet). Snapshots, `verify`, `gc`, and
    node hashes are not reachable through `eo9:fs` yet — that needs the planner's hash-query/snapshot surface
    on the WIT side (the SPEC's open TODO).
15. **Async bridging is eager-only for now.** The disk imports are `async func`s but the engine is
    synchronous, so the provider polls each disk call to completion on the spot and fails with an `io` error
    if the disk would genuinely suspend. Every disk it can be wired to today (disk.mem, other compute-only
    backends) completes eagerly; the fully asynchronous bridge (or an async `BlockDevice`) is the follow-up
    for when a genuinely suspending disk provider exists.
16. **`eo9:disk` API gaps to raise with the planner** (not changed here — wit/ is owned elsewhere): no size
    query (the provider discovers the device size by probing with zero-length reads, ~120 probes once per
    mount) and no flush/sync operation (writes are treated as durable when they return). Both would make the
    bridge cleaner and are needed for honest durability on real hardware.
17. **Test coverage split.** The integration tests cover the provider layer: component shape, the
    `disk.mem $ fs.eofs` seal, the behavioral `readwrite` round-trip over the full chain on documented
    defaults, and cross-run determinism. Persistence across remounts, crash consistency, snapshots,
    compression, and hostile images remain covered by `eofs-core`'s own suite; a component-level
    remount/persistence test needs a host-backed persistent disk provider and lands with M3.

Milestone 3 (usermode persistence: `--disk`, `mkfs.eofs`, the file-backed device):

18. **The `eo9:disk` root-provider seam now exists host-side.** `eo9-runtime` gained a `DiskProvider`
    trait (owned-buffer read/write at byte offsets, one `DiskError` mapped onto the WIT's per-operation
    error variants by the linker) plus a `Providers.disk` slot and the `eo9:disk/disk` linker path
    (`disk-impl` lives in `eo9:disk/types`, registered unconditionally like the other types-only
    interfaces; the operations and the `disk-optional` "granted" answer require the grant) — the same
    shape as the fs seam. The unix file-backed `DiskProvider` (already in `eo9-providers-unix`) is wired
    behind a new global `--disk <image>` flag with the same opt-in posture as `--fs-root`: no flag, no
    block device, and a program/composition that hard-requires `eo9:disk` is refused up front
    (`eo9 run` names the flag; the shell surfaces the spawn refusal). Containment is structural — only
    the named image file is reachable. The grant flows into both `run` and the shell session (children
    inherit it; the session manifest lists it).
19. **Disk operations complete eagerly for the granted device.** `fs.eofs` drives its block device from
    a synchronous engine and requires every disk call to be complete when it returns (D15), but the unix
    provider's pool path completes on another thread — through the runtime that looks like a suspension
    and every mount died with `device i/o failure`. The unix provider gained `read_blocking`/
    `write_blocking` (same checks, same error mapping, calling-thread execution) and the `--disk`
    adapter uses them, returning already-resolved operations. The fully asynchronous bridge remains the
    follow-up for a device that genuinely must suspend; a positioned read/write on a host file is not
    that device.
20. **`eo9 mkfs.eofs <image> [--size <bytes[K|M|G]>] [--force]`.** Host-side formatting through the same
    `eofs-core` engine the provider uses, over a small `FileDevice` (read/write/flush on a host file).
    A missing image file is created at `--size` (default 16 MiB, matching disk.mem); an existing file
    keeps its size. An image whose uberblock slots carry the eofs magic is never reformatted without
    `--force` — surfacing the situation beats silent data loss — while a blank file formats without
    ceremony (the provider's own format-on-first-mount rule still covers the no-mkfs path). `eofs-core`
    is now `publish = true` (the `eo9` binary depends on it), which adds it to the crates.io publish
    chain — xtask's `PUBLISH_CRATES`/`PUBLISH_LEAF_CRATES` lists still need the one-line addition
    (xtask was outside this change's scope).
21. **Milestone-3 test coverage.** CLI end-to-end tests now cover: mkfs format/refuse/`--force`; the
    persistence story itself — `mkfs.eofs`, then `--disk img -c "fs.eofs $ readwrite /keep.txt …"` in
    one process and `fs.eofs $ cat /keep.txt` + `fs.eofs $ ls /` in *new* processes reading the data
    back (the image file is the only shared state); the no-`--disk` refusal; and a truncated image
    surfacing the program's typed `fs(…)` error rather than a panic. Crash consistency itself remains
    `eofs-core`'s suite (root-flip commits); the provider layer adds no caching that could weaken it.
22. **What milestone 4 (store-on-eofs, kernel adoption) still needs.** A kernel-side `eo9:disk` root
    provider (the virtio-blk wasm driver of plan/12 D43(e), or an in-kernel ramdisk for QEMU), the
    asynchronous disk bridge (or an async `BlockDevice`) once a genuinely suspending device exists,
    `eo9:disk` size/flush operations (D16) for honest durability, a guest-reachable verify/snapshot
    surface on `eo9:fs`, and the plan/06 evaluation of hosting the module store on an eofs image.
23. **The disk API gaps are closed: size query and commit-boundary durability (2026-05-29).**
    `DiskDevice` now reads the device size from `eo9:disk/disk.size` (the ~120 zero-length-read
    probe is gone) and its `BlockDevice::flush` calls `eo9:disk/disk.flush`, so the engine's
    root-flip commits (which flush before and after the uberblock write) reach fsync on a
    `--disk` file device and a virtio cache flush on `disk.virtio`. Behaviour over `disk.mem`
    is unchanged (no-op flush). The async-disk-bridge follow-up from D15 still stands.
24. **Study-07 hardening: the operational shell around the engine (2026-05-31, branch
    `area/14-eofs-integrity`).** The round-3 storage-engineer study (docs/user-studies/07) found
    that while the data path held up (corruption always detected, blast radius exact, portability
    real), every operational edge around it was a data-loss trap. All fix-now findings are closed:
    - **S7-4, atomic rewrites.** The provider's `open(TRUNCATE)` no longer commits a remove+recreate
      transaction of its own; truncation is recorded on the handle and applied by the first `write`
      as one engine transaction (remove + recreate + write + commit), with `Eofs::rollback()` (new)
      discarding the pending state of any failed multi-step operation. A failed rewrite leaves the
      previous contents on disk. Engine + CLI regression tests.
    - **S7-3, space reclamation.** Mutating provider operations run `Eofs::gc()` on `NoSpace` and
      retry once: rewrites reuse the space of the copies they replace, `rm` frees space, images no
      longer brick at a finite write count. The no-gc control test documents the old behaviour.
    - **S7-1, loud uberblock fallback.** New engine API: `Uberblock::classify_slot` (NoMagic /
      Invalid / Valid), `Eofs::mount_with_report`, and `probe()` → Eofs/Blank/Foreign/Unmountable.
      The CLI probes every `--disk` image before composing and warns when the mount will fall back
      past a damaged slot. (The wasm provider itself still cannot warn — that needs a diagnostics
      channel; the *policy* question of operator-gated rewind vs warn-and-mount is the owner
      decision recorded in the study triage.)
    - **S7-2, blank means all-zero.** Auto-format (provider and mkfs alike) only touches devices
      probed `Blank`: no eofs magic AND all-zero leading 64 KiB. Foreign data refuses with the
      explicit `mkfs.eofs --force` way out. The kernel storedisk path already had this rule.
    - **S7-5, integrity error fidelity.** ChecksumMismatch / Corrupt map to messages led by a fixed
      `integrity check failed:` marker. The complete fix is an `integrity(string)` case in the
      `eo9:fs` `fs-error` WIT variant, mapped from those two engine errors — **WIT addition needed,
      next WIT round** (also wanted by the kernel's metal error rendering).
    - **S7-11.** With S7-4 in place, the `readwrite` truncate-then-write pattern is safe (atomic);
      the corruption-test methodology note lives in the study report.
    Findings that remain open by design: S7-12 (rename/atomic-replace in WIT — owner decision),
    S7-8 (fsck/scrub/df surface), S7-9 (uberblock geometry), S7-19 (operator-side threat model
    SPEC paragraph) — all tracked in the study triage table.

25. **Blank-image probe spans widened to clear every common foreign format (2026-06-01, owner ruling
    on S7-2, branch `area/14-blank-and-rename`).** D24's "all-zero leading 64 KiB" rule had a hole:
    btrfs puts its primary superblock at exactly 64 KiB, so a btrfs volume whose first 64 KiB is
    legitimately zero would have been auto-formatted. `eofs_core::probe()` now judges a magic-less
    device blank only when **(a)** its leading 1 MiB is all zero (clears MBR/GPT/ext4/bcachefs/ZFS
    and btrfs with margin), **(b)** its trailing 64 KiB is all zero (clears backup GPT headers and
    ZFS end-of-device labels — a wiped start with surviving backups is damaged data, not blank), and
    **(c)** devices of 2 MiB or less are probed in full. Both auto-format paths (the fs.eofs
    provider and un-forced `mkfs.eofs`) share `probe()`, so one change covers both. The accepted
    residual: data hiding strictly between the spans of a large device still reads as blank —
    nothing common lives there, and probing whole multi-gigabyte devices on every mount is not worth
    it (test-pinned as documentation). New tests: engine-level btrfs-at-64 KiB / tail-backup /
    between-spans / small-device cases; CLI-level `zero_prefixed_foreign_volumes_are_refused_not_formatted`
    proves both victims are refused by provider and mkfs with bytes untouched.

25. **The async-first conversion: one engine, two device boundaries (2026-06-02, branch
    `area/14-async-storage`).** The D15/D22 follow-up landed, under the SPEC ruling "Boundaries are
    honestly async" (the owner's directive that nothing stays sync because it happens to complete
    eagerly). Shape:
    - **`eo9-eofs` is async at its core.** The engine (CoW, Merkle, transactions, snapshots, verify,
      gc) now runs over an `AsyncBlockDevice` (`size` sync; `read_at`/`write_at`/`flush` async) and
      awaits every device call; recursion sites (`apply_in_dir`, `collect_level`, `verify_ptr`,
      `mark_ptr`) box their futures, with depth still bounded by `check_object`'s validated tree
      height. The synchronous embedders — the kernel storedisk cache, `mkfs.eofs`, the whole test
      suite — keep the *unchanged* `BlockDevice` trait and `Eofs`/`probe`/`SnapshotView` API through a
      facade that adapts via `SyncDevice` and drives each core future with a single poll: over a sync
      device the core's only awaits are immediately ready, so `Pending` is unreachable by construction
      and the facade is behaviorally identical to the pre-async engine. **No CoW/Merkle/commit logic
      exists twice**; the kernel and `crates/eo9` compile against it without a single edit.
    - **`fs.eofs` genuinely awaits** its disk import (`AsyncEofs<DiskDevice>` where `DiskDevice:
      AsyncBlockDevice` awaits `eo9:disk` calls). The eager single-poll helper and its "device
      suspended" failure class are gone. Because operations now park mid-flight, the provider state
      moved from `ProviderState` (whose borrow must never cross an await) to a take/put slot:
      `Empty | Busy | Ready(engine)` — an operation takes the engine out, awaits freely, puts it
      back; a concurrently delivered operation observes `Busy` and fails with a typed `io`
      ("filesystem is busy") rather than trapping on a re-borrow. All shipped consumers issue fs
      calls sequentially; queueing instead of refusing is a recorded refinement.
    - **Deadlines live where the waiting lives.** `fs.eofs` adds no clock import; its awaits are
      bounded *transitively* by the device layer's own bounds (disk.virtio's interrupt-retry and
      poll-spin limits, the kernel's bounded `wait`, host providers' eager completion). A disk that
      exceeds its bounds surfaces a typed `io` error through the same await. This satisfies the
      SPEC's "everything that parks, parks bounded" without growing the provider's import surface —
      revisit if a genuinely unbounded device class appears.
    - **Lazy bring-up vs the sync `size` query.** `disk.virtio` can only bring its hardware up
      inside an awaited operation, so `size` (synchronous WIT) reports 0 until first use; the
      `fs.eofs` mount path issues one 1-byte read on size-0 (which both wakes the device and, on
      failure, surfaces the driver's real typed error) and then re-asks for the size.
    - **Verification.** Engine suite (41 tests) green through the sync facade; kernel aarch64
      full-feature build untouched and the two-boot storedisk QEMU smoke (format/miss → remount
      txg 2/hit) green; `cargo test -p eo9 --test cli` 60/60 green pre-stub-conversion (the engine
      facade) — the bundle refresh that re-exercises the *converted* fs.eofs in the CLI happens at
      merge from the main checkout per the build convention; the usermode integration suite is
      green including a new `filtered_chain_over_a_deferring_eofs_round_trips` pin (the deepest
      awaited guest chain in usermode: readwrite → fs.filtered → fs.eofs → disk.mem, with fs.eofs
      genuinely parking on every disk call).

26. **Metal acceptance: study 09's filtered-storage chain runs (2026-06-02, QEMU aarch64,
    `pci disk` boot, fresh scratch disk).** Transcript evidence, all through the *converted*
    awaiting stubs (paced-console driver; the modern policy-component form — `pci.filtered`'s
    own `--allow` moved to `pci.admit-address` in the policies-are-programs swap, which is what
    study 09's pre-swap transcript still showed):
    - Baseline, unfiltered: `disk.virtio $ fs.eofs $ readwrite --path "/keep.txt" …` →
      `disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx
      interrupt` / `pci: INTx delivery on line 3 served an interrupt wait (the cpu halted
      instead of polling)` / `ok: round-tripped(10)`.
    - **The flagship**: `pci.admit-address --allow "[{segment: 0, bus: 0, device: 3, function:
      0}]" $ pci.filtered $ disk.virtio $ fs.eofs $ ls /` → probe line, `keep.txt`,
      `ok: listed(1)` — the exact composition class study 09 pinned as failing with the typed
      suspension. The INTx line shows interrupt-paced completion *through the filter*: the
      interposed-interrupt residual (plan/09 D31) is empirically gone, not just by-construction.
    - Next boot (scratch disk kept): the same chain with `cat /keep.txt` served `asyncfirst` —
      power-cycle persistence through the filtered chain — and the wrong-device variant
      (allow-list naming the rng) failed with the driver's own actionable error ("no virtio-blk
      function is visible through the granted pci capability … check that an attenuator composed
      in front of this driver allows the disk's address") instead of study 09 finding 3's
      misleading "device too small" (the size-rewake path of D25).
    - Perf: usermode A/B of the eofs round-trip test (eager master vs awaited branch, 3 runs
      each): ~4.0s vs ~3.3s median — no measurable queueing tax (the bar was "analyze if >2x").
      No metal op-phase instrumentation exists to time the ms-scale run phase under the
      minutes-scale on-target compile, so the metal evidence is qualitative: completion moved
      from polled (the eager driver's single poll could never see the interrupt wait complete)
      to genuine halt-until-INTx, strictly less CPU burn.
    Residual: `gpu.virtio` (merged from `area/02-gfx` while this branch was in flight) still
    uses the eager `pci_call` convention — the next conversion in this series, deferred to the
    gfx lane.
