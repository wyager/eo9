# 06 — Module store & compilation cache (`crates/eo9-store`)

## Scope
The Nix-inspired content-addressed module store, name resolution, and the deterministic hash-keyed
compilation cache.

## Spec references
"The module store and compilation cache", "Loading is immutability-first" (Execution APIs), Filesystem API
immutable-handles note, Shell (name resolution).

## Deliverables
- `eo9-store` crate:
  - Content-addressed store: objects keyed by blake3 hash; usermode layout under `$EO9_STORE`
    (default `~/.eo9/store`): `objects/<hash>`, immutable once written.
  - Name resolution: `manifests/` mapping bare names (`browser`, `virtualfs.create`, `fs.memfs`) → object
    hashes, with the dotted package.world convention from the spec. Profiles (a stack of manifests) kept
    simple for MVP. Exact file format is this area's call; document it.
  - Compile cache: key = hash(ordered module hashes of the fused composition, configure constants,
    compile-opts, target triple, compiler version string); value = compiled image bytes + metadata;
    LRU/MFU eviction with a size budget; `gc` command support.
  - A small `store` API for the usermode binary and eosh: `add`, `resolve(name) -> hash + immutable handle`,
    `lookup-image(key)`, `insert-image(key, bytes)`.
  - Read-only store image builder: pack a set of objects + manifest into a single flat image the kernel can
    map (format: simplest thing that works — e.g. length-prefixed index; coordinate with plan 12).
- Determinism note: cache correctness depends on deterministic compilation (plan 04 verifies); until
  verified, include the compiler version + a "non-deterministic" flag in keys so nothing is silently wrong.

## Dependencies
01; 03 only if composition-DAG hashing helpers end up here (fine either way). Consumed by 11, 10 (via the
usermode binary's fs mapping), 12 (store image).

## Milestones
1. Object store + manifests + resolve; used by `eo9 run <name>`.
2. Compile cache integrated with plan 04 (`I2`: second launch is a cache hit).
3. Store image builder for the kernel.

## Decisions
(record here)
