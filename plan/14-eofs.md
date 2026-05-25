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
(record here)
