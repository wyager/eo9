# eofs on-disk format (version 1)

This document is the authoritative description of what `eofs-core` writes to a block device.
`src/format.rs` is its code twin; if the two ever disagree, one of them is a bug. The design
goals come from SPEC.md "Filesystem API": copy-on-write with never-overwrite-in-place, atomic
root flips, a blake3 Merkle tree over everything, snapshots as retained roots, per-block
compression with a codec tag, and deterministic images.

Integers are little-endian. Offsets and sizes are in bytes. Structures are fixed-layout byte
strings; there is no alignment padding beyond what is written out explicitly.

## Device model

eofs runs over a flat, byte-addressed device (the same shape as the `eo9:disk` API: reads and
writes of byte ranges at offsets). It assumes:

* reads observe previously completed writes;
* `flush` makes all completed writes durable before it returns;
* **nothing else** — in particular, no write atomicity. A torn write of any structure,
  including an uberblock, is detected by checksums and simply loses the transaction that was
  being written, never anything older.

## Layout

```
offset 0        uberblock slot 0   (4096 bytes, fixed regardless of block size)
offset 4096     uberblock slot 1   (4096 bytes)
offset 8192     data region        (everything else: data, indirect, directory, and
                                    snapshot-table blocks, allocated append-style)
```

The slot offsets and sizes are fixed so a mount can find them before knowing anything about
the filesystem; the filesystem's own block size and allocation unit are recorded inside the
uberblocks.

## Uberblock

The uberblock is the root of everything. Two slots exist; a commit writes the new uberblock
into the slot the previous commit did *not* use (slot = `txg mod 2`), so the most recent
valid uberblock is never overwritten. Mount reads both slots and adopts the one with a valid
magic + checksum and the highest `txg`.

| offset | size | field |
|-------:|-----:|-------|
| 0   | 8  | magic `"EOFS-UB\0"` |
| 8   | 4  | format version (1) |
| 12  | 4  | block size in bytes (power of two, 512 … 1 MiB; default 4096) |
| 16  | 4  | allocation unit in bytes (power of two, 64 … 4096, ≤ block size; default 512) |
| 20  | 1  | default codec for new blocks (0 = raw, 1 = lz4) |
| 21  | 3  | reserved (zero) |
| 24  | 8  | txg — transaction number (1 at format, +1 per commit) |
| 32  | 8  | allocation frontier (first never-allocated byte) at this commit |
| 40  | 8  | device size at format time |
| 48  | 72 | live root: object reference to the root directory |
| 120 | 72 | snapshot table: object reference to the serialized snapshot list |
| 192 | 32 | blake3 checksum of bytes 0..192 |
| 224 | …  | zero padding to the end of the 4096-byte slot |

A slot whose magic or checksum does not match is treated as absent (this is how torn commits
and freshly `format`-ted slot 0 are handled). `format` zeroes both slots first so stale
uberblocks from a previous filesystem can never win the election, then commits txg 1.

## Block pointers

Every stored block is addressed by a 56-byte **block pointer**:

| offset | size | field |
|-------:|-----:|-------|
| 0  | 8  | physical byte offset of the stored bytes (0 = null pointer) |
| 8  | 4  | logical size — the uncompressed length, 1 … block size |
| 12 | 4  | physical size — the stored length |
| 16 | 1  | codec: 0 = raw (physical == logical), 1 = lz4 block format |
| 17 | 7  | reserved (zero) |
| 24 | 32 | blake3 hash of the **logical** (uncompressed) bytes |

The all-zero pointer is "null" and only appears as the root of an empty object. Because the
hash is over logical bytes, it is independent of how (or where) the block happens to be
stored; every read decompresses and re-hashes the block and fails with a checksum error on
any mismatch, and `verify()` is simply this check applied to every reachable block.

### Compression

The filesystem-wide default codec is fixed at format time (lz4 by default). Each block is
compressed independently with the lz4 *block* format (`lz4_flex`); if compression does not
make it strictly smaller, the block is stored raw — so incompressible data costs nothing
extra and decompression is never run for it. The codec tag is per block pointer, so further
codecs can be added later by defining a new tag without changing anything else.

## Byte objects (block trees)

Files, serialized directories, and the snapshot table are all stored the same way: as a
**byte object** — a logical byte string of arbitrary length addressed by a 72-byte
**object reference**:

| offset | size | field |
|-------:|-----:|-------|
| 0  | 8  | logical size in bytes |
| 8  | 1  | level: number of indirect levels above the data blocks |
| 9  | 7  | reserved (zero) |
| 16 | 56 | root block pointer (null iff size is 0) |

The object's bytes are split into data blocks of `block_size` bytes (the final block may be
short; data blocks are never zero-length). With one data block, the root points at it
directly (`level` 0). Otherwise data-block pointers are packed into **indirect blocks** —
arrays of 56-byte block pointers, up to `block_size / 56` per block (73 at the default block
size) — and levels of indirect blocks are added until a single root remains. Indirect blocks
are ordinary blocks: compressed, hashed, and verified like everything else.

There are no holes: writing past the end of a file materialises zero-filled blocks for the
gap (they compress to almost nothing when compression is on).

Readers validate an object reference before walking or allocating for it: the declared size
must fit on the device, `level` must be exactly the height the writer would produce for that
size, the walk must not meet more data blocks than the size allows, and *metadata* objects
(serialized directories and the snapshot table, which are read into memory whole) are capped
at 16 MiB — so a corrupted or hostile reference fails cleanly instead of driving unbounded
allocation or fan-out.

## Directories

A directory is a byte object containing its entries, each:

| offset | size | field |
|-------:|-----:|-------|
| 0 | 2 | name length in bytes (1 … 255) |
| 2 | 1 | kind: 1 = file, 2 = directory |
| 3 | 1 | reserved (zero) |
| 4 | 72 | object reference to the child (file content or child directory) |
| 76 | … | name (UTF-8, no `/`, no NUL, not `.` or `..`) |

Entries are stored sorted by name bytes, so a given set of entries always serialises — and
therefore hashes — identically. An empty directory is the empty object (size 0, null root).
There are no inode numbers and no hard links: the namespace is a tree, and a child is
identified by the object reference stored in its parent.

## Snapshot table

The snapshot table is a byte object referenced by the uberblock, containing one entry per
snapshot in creation order:

| offset | size | field |
|-------:|-----:|-------|
| 0  | 8  | txg of the commit that made (or will make) the snapshot durable |
| 8  | 2  | name length in bytes (1 … 255) |
| 10 | 6  | reserved (zero) |
| 16 | 72 | object reference: the live root directory as it was when the snapshot was taken |
| 88 | …  | name (same rules as directory entry names) |

A snapshot is nothing but a retained root: creating one appends an entry holding the current
root; the blocks it references stay live for as long as the entry exists. Snapshots share all
unmodified blocks with the live tree and with each other.

## Transactions, commit, crash consistency

Nothing is ever overwritten in place. Every operation (write, mkdir, remove, …) writes new
blocks for whatever changed — the data blocks it touched, the file's indirect blocks, and
every directory from there up to the root — and updates the *in-memory* root reference only.
The on-disk filesystem does not change until `commit()`:

1. `flush` — everything the new root references becomes durable;
2. write the new uberblock (txg + 1) into the *other* slot;
3. `flush` again.

A crash before step 2 leaves the previous uberblock as the newest valid one: the filesystem
remounts exactly at the previous transaction. A crash during step 2 leaves a torn uberblock
whose checksum fails, with the same result. A crash after step 2 lands on the new
transaction. Committed transactions are therefore always intact and uncommitted ones are
always absent, with no journal, no replay, and no fsck; `verify()` checks integrity, it never
repairs. Uncommitted changes are also discarded by `unmount`/drop — the transaction boundary
is explicit.

## Allocation, space reuse, GC

Allocation is append-style: blocks are placed at the **allocation frontier**, rounded up to
the allocation unit; the frontier as of each commit is recorded in its uberblock (anything
written beyond it by an uncommitted transaction is simply dead bytes after a crash).

Reclamation is deferred. The manual `gc()` entry point walks every retained root — the
committed root, the pending root, and every snapshot — and hands the unreferenced gaps below
the frontier to the in-memory allocator as a free list, which is consumed first-fit before
the frontier grows again. The free list is not persisted (it is recomputed by running `gc()`
again after a remount), and nothing reachable from any retained root is ever handed out, so
"never overwrite in place" holds for every byte any root can reach.

## Hashing and the Merkle tree

Every block pointer carries the blake3 hash of the block it points to, indirect blocks and
directories are blocks full of block pointers / object references, and the uberblock holds
the root object references — so every node's hash transitively covers its entire subtree,
and the uberblock's references cover the whole filesystem. `stat` exposes a node's Merkle
root (the hash in its root block pointer, all-zeros for an empty node); a node whose subtree
did not change keeps its hash across commits, which is what makes cheap change detection and
hash-guided incremental walks work.

One consequence to be aware of: for a node bigger than one block, the Merkle root is the hash
of its root *indirect* block, whose bytes include physical addresses and codec tags — so it
is deterministic for a given operation history but it is **not** a pure function of logical
content (a byte-identical file written elsewhere can have a different root hash). Single-block
nodes hash to exactly `blake3(content)`. If the `eo9:fs` hash queries (milestone 2) need
content-only identities for arbitrary sizes, a canonical content-hash field can be added to
directory entries in a later format version; this is recorded as an open question in
plan/14-eofs.md.

## Determinism

The engine uses no clock and no randomness, allocation order is a pure function of the
operation sequence, directory serialisation is canonical (sorted), and compression
(`lz4_flex`) and hashing (blake3) are deterministic — so the same operation sequence over the
same format options and device size produces bit-identical images (over a zero-initialised
device; eofs never needs to read bytes it has not written, so pre-existing junk outside
written extents is harmless but would show through in a byte-for-byte image comparison).

## Versioning

The format version lives in every uberblock. Version 1 readers reject other versions.
Additive changes that reuse reserved fields or new codec tags still bump the version; old
readers must never silently misread a newer image. The reserved bytes in block pointers,
object references, directory entries, and snapshot entries are written as zero and ignored on
read, giving later versions room without relayout.

## MVP non-goals

Multi-device/RAID, deduplication, quotas, encryption, hard links, sparse files (holes), and
online/automatic GC are out of scope for this format version. Concurrency is the embedder's
business: `eofs-core` is a single-writer library; the provider layer above it serialises
access.
