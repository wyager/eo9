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

Deferred to later milestones: the provider component and `eofs.mkfs` (M2), usermode end-to-end and
store-on-eofs (M3), kernel adoption (M4), plus content-only node hashes, holes/sparse files, rename, and
persistent free-space maps if they turn out to be needed.
