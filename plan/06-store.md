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

1. **Store root & layout.** Root is `$EO9_STORE`, else `~/.eo9/store` (`Store::open_default`). Layout:
   `version` (marker line `eo9-store 1`, checked on open), `objects/<blake3-hex>`, `manifests/<stem>.manifest`,
   `profiles/<stem>.profile`, `cache/<key-hex>/{image,meta}`. Everything is written via temp-file/dir + rename,
   so readers never see partial writes; objects are chmod'd read-only before they become visible.
2. **Hashes.** blake3, 64-char lowercase hex everywhere (object file names, manifest entries, cache dirs).
   `ObjectHash` (content) and `CacheKey` (derived) are distinct types.
3. **Names.** `Name` = dot-separated kebab-case segments (`browser`, `virtualfs.create`, `fs.memfs`); the store
   treats the dotted name as a flat key, with `package()`/`world()` accessors for the spec's package.world
   convention. Manifest/profile file stems are single segments (no dots, no path separators).
4. **Manifest format (v1).** Line-oriented text: header `eo9-manifest 1`, then `<name> <hash>` lines, `#`
   comments and blank lines ignored, duplicate names rejected, serialized sorted by name.
5. **Profile format (v1).** Header `eo9-profile 1`, then manifest stems one per line, base first; **later
   manifests shadow earlier ones** (same override direction as `&`). If `profiles/<p>.profile` is absent, the
   profile is implicitly the single manifest `<p>` (absent ⇒ empty), so a fresh store works with just
   `add` + `bind` + `resolve` and no profile file. An explicit profile naming a missing manifest is an error.
   Default manifest and profile are both named `default`.
6. **Binding rule.** `bind` requires the target object to already be in the store — manifests never point at
   objects that don't exist.
7. **Resolution result.** `resolve(name)` returns hash + `ObjectHandle` (hash, store path, open read-only
   file) — the usermode realization of the spec's immutable handle; `verify()` re-checks content against hash.
8. **Cache key (v1).** blake3 `derive_key` with context `"eo9-store compile-cache key v1"` over length-prefixed
   fields: ordered module hashes, configure constants (stable-sorted by name, WAVE-encoded values), compile-opts
   text, target triple, compiler version string, and a `compiler_deterministic` flag. The flag is `false` until
   plan 04 verifies deterministic codegen, keying unverified entries separately per the determinism note.
9. **Cache entry format (v1).** `cache/<key>/image` (bytes) + `cache/<key>/meta` (text: header `eo9-image-meta 1`,
   `created`, `last-used`, `use-count`, `image-size`, `target`, `compiler`, `deterministic`, repeated `module`
   lines). Lookups bump `use-count`/`last-used` by atomically rewriting `meta`.
10. **Eviction.** `gc(CachePolicy { max_bytes })` (default budget 4 GiB, provisional) evicts in ascending
    `(use-count, last-used, key)` order until image bytes fit the budget — the simplest deterministic LRU/MFU
    blend; the policy is a pure, separately testable `CachePolicy::plan`. `gc` also sweeps `.tmp-*` leftovers
    older than one hour.
11. **Dependencies.** Only `blake3` (from the root pin table) plus std; temp dirs in tests are hand-rolled to
    avoid a `tempfile` dev-dependency.
12. **Deferred.** Milestone 3 (read-only store-image builder for the kernel) and object-level GC (objects
    unreachable from any manifest/cache entry) are not implemented yet; runtime/CLI integration is consumed by
    plans 04 and 11 against the API above.
13. **The CLI's `seed` marker file at the store root.** `crates/eo9` (not this crate) writes a small
    line-oriented marker `<root>/seed` recording what first-run seeding last did (header `eo9-seed 1`, an
    embedded-set fingerprint, and one `name hash` line per seed-managed binding) so an upgraded binary can
    refresh exactly the bundled-program bindings it owns (plan/11 D18). The store crate neither reads nor
    writes it and the store layout/version marker are unchanged; the file sits beside `version` at the root
    and is ignored by `objects()`/`gc`/name resolution. If the store ever gains first-class metadata, this
    is the first candidate to migrate.
14. **Kernel-side persistent compile cache (store-on-eofs, first rung — 2026-05-29).** The bare-metal kernel
    now keeps a disk-backed cache of on-target compile results on an eofs-formatted virtio disk behind the
    `storedisk` boot token (plan/12 D56). It deliberately does not reuse this crate (the kernel needs no
    names/bindings/GC, just content-addressed artifacts), but it follows the same content-addressing
    convention: blake3 of the exact bytes handed to the compiler. If/when the metal store grows shell-visible
    bindings (a real writable /bin), revisiting whether `eo9-store`'s manifest format should be shared is the
    first design question.
