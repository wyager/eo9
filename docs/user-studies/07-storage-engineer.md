# User study 07 — storage / database engineer (the persistence story)

## Session metadata

- **Date:** 2026-05-31
- **Branch / worktree:** `docs/study-07` (worktree of master at `5985249`)
- **Participant persona:** a storage/database engineer, ~10 years of experience (filesystems,
  embedded KV stores, a ZFS deployment scar or two; thinks in terms of write paths, fsync,
  torn writes, and what happens when the disk lies to you).
- **Methodology:** same as studies 01–06. The participant was a role-played persona run as a
  separate session with no access to the repository, its documentation, or any tools — it saw
  only the demo transcript pasted into its prompt and replied conversationally. Every command
  shown was actually executed by the facilitator; outputs are verbatim, trimmed only for
  length. Failures and surprises were shown as they happened, not cleaned up.
- **Environment:** `eo9` built from this checkout (master `5985249`) on an Apple Silicon macOS
  host, rustc 1.98.0-nightly; a throwaway store (`EO9_STORE`, seeded with the 50 bundled
  components on first run); eofs disk images under `/tmp`; for the bare-metal segment, the
  aarch64 kernel from the same checkout under `qemu-system-aarch64 -M virt` with the
  `storedisk` / `pci disk` xtask grants.
- **Focus:** Eo9's persistence story — `eofs` (the native CoW/Merkle filesystem), `mkfs.eofs`,
  the `--disk` grant, `fs.eofs`, corruption behavior, durability, space accounting, and on
  metal the power-cycle-surviving compile cache + saved programs and the wasm virtio-blk
  driver. A previous (lost) study session claimed that *"a corrupted file read back
  silently"* — this session re-investigated that claim carefully; the answer is in finding 2
  and the corruption section.

## Demo round 1 — usermode persistence

### 1.1 Build, format, write, read back across processes

`cargo build -p eo9` (debug) finished in 3.5 s against a warm target dir;
`cargo build -p eo9 --release` in 5.8 s. **Note:** all timings below are from the release
binary. The debug binary is functionally identical but pays ~2 s per disk-composition run
where release pays ~0.16 s (first-touch experience matters: a user who follows
"`cargo build`" rather than "`cargo install`" gets the slow one).

```
$ eo9 mkfs.eofs /tmp/s7.img --size 8M
formatted /tmp/s7.img: 8388608 bytes, eofs (block size 4096, lz4 compression on)        # exit 0

$ time eo9 --disk /tmp/s7.img -c "fs.eofs $ readwrite /keep.txt hello"
eo9: first run: seeded 50 bundled programs into the module store at <store>
ok: round-tripped(5)                                                # exit 0; 0.83 s (seed + cold compile)

$ time eo9 --disk /tmp/s7.img -c "fs.eofs $ cat /keep.txt"          # a NEW process
hellook: printed(5)                                                 # exit 0; 0.16 s
$ time eo9 --disk /tmp/s7.img -c "fs.eofs $ ls /"                   # a NEW process
keep.txt
ok: listed(1)                                                       # exit 0; 0.16 s
```

Persistence across processes is real: the only state shared between the runs is the image
file. Verified stdout purity with a redirect: stdout carries exactly the 5 file bytes
(`hello`), the outcome line goes to stderr — pipes stay clean. (The glued `hellook:` above is
terminal interleaving because the file content has no trailing newline; this is the known
round-2 finding R2-19, still unfixed.) The image layout after this write: uberblock slots at
0 and 4096 (live = highest valid txg), data region from 8192, the file bytes stored raw
(30-byte test payloads don't compress) at a 512-byte-aligned allocation unit.

`fsync` is wired for real: the `--disk` file device's `flush` calls `File::sync_all`
(`crates/eo9-providers-unix/src/disk.rs:209`), and eofs flushes before and after every
uberblock write (commit boundary). Every mutating fs operation ends in a commit.

### 1.2 The two refusals

```
$ eo9 -c "fs.eofs $ cat /keep.txt"               # same command, NO --disk grant
error: spawn failed: component imports instance `eo9:disk/disk@0.1.0`, but a matching
implementation was not found in the linker: instance export `default` has the wrong type:
function implementation is missing (the program requires the eo9:disk block-device
capability, which this session does not grant: relaunch with `--disk <image>` (create and
format one with `eo9 mkfs.eofs <image>`))                           # exit 3

$ eo9 mkfs.eofs /tmp/s7.img                      # reformat attempt on a live image
eo9: error: /tmp/s7.img already contains an eofs filesystem (or the remains of one); pass
--force to reformat it and lose its contents                        # exit 3

$ eo9 mkfs.eofs /tmp/s7.img --force
formatted /tmp/s7.img: 8388608 bytes, eofs (block size 4096, lz4 compression on)        # exit 0
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ ls /"
ok: listed(0)                                                       # data gone, as asked
```

Both refusals exist and both are pre-execution with exit 3. The mkfs refusal is clean and
names the remedy. The no-`--disk` refusal *contains* the right remedy (`--disk`,
`mkfs.eofs`) but buries it in parentheses **after** four clauses of raw linker internals —
the friendly sentence the missing-`--fs-root` case leads with is here a footnote to
`instance export 'default' has the wrong type`.

### 1.3 Corruption testing (the heart of the session)

Method: write a 30-byte canary (`hello-eofs-corruption-canary-7`), copy the image as a
pristine reference, then for each test restore the pristine copy, flip bytes with `dd` at a
chosen offset, and read back **with `cat` from a fresh process**. The image was mapped first
(uberblock slots at 0/4096; the canary occurs at exactly one offset; two directory-object
versions exist because of copy-on-write — one stale, one live).

**Live data block** (flip 1 byte of the canary, two different offsets tried):

```
$ printf 'X' | dd of=/tmp/s7.img bs=1 seek=8704 count=1 conv=notrunc
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ cat /keep.txt"
error: fs("FsError::Io(\"block checksum mismatch\")")               # exit 1
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ ls /"
keep.txt
ok: listed(1)                                                       # exit 0 (directory block untouched)
```

**Live directory block** (flip 4 bytes):

```
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ ls /"
error: fs("FsError::Io(\"block checksum mismatch\")")               # exit 1
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ cat /keep.txt"
error: fs("FsError::Io(\"block checksum mismatch\")")               # exit 1
```

**Verdict on detection: corrupted live blocks are detected, every time, and corrupted data
is never returned.** Every read path goes through a blake3 check against the block pointer
(`crates/eofs-core/src/tree.rs`, `read_block`). The earlier lost-study claim of a silent
corrupted read-back **does not reproduce** for data or metadata blocks.

**But three "silent" paths do exist, and one of them is real data loss:**

**(a) The live uberblock — silent rollback.** The uberblock pair alternates slots per
transaction; mount picks the valid slot with the highest txg. Corrupt the *newest* slot and
mount silently falls back to the *previous* transaction:

```
# /keep.txt contains the 30-byte canary; live uberblock = slot 1 (txg 3), slot 0 = txg 2
# (txg 2 = the moment the file had been created but not yet written)
$ printf '\xde\xad\xbe\xef' | dd of=/tmp/s7.img bs=1 seek=4136 count=4 conv=notrunc
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ ls /"
keep.txt
ok: listed(1)                                                       # exit 0 — no error, no warning
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ cat /keep.txt"
ok: printed(0)                                                      # exit 0 — file is silently EMPTY
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ stat /keep.txt"
file 0 bytes
ok: described                                                       # exit 0
```

The 30 bytes are gone, the exit code is 0, and **nothing anywhere tells the user the
filesystem just time-traveled.** This is the torn-write recovery path doing its job on the
wrong event: for a torn uberblock write during a crash, falling back to the previous txg is
exactly right; for *corruption of an already-committed uberblock* (bit rot, a bad sector, a
tamperer), it silently serves stale state as current. ZFS makes the same fallback but
reports it (`zpool status` checksum-error counters); eofs has no equivalent surface — no
mount-time warning, no error counter, nothing. This is the most plausible reconstruction of
the lost study's "corrupted file read back silently" claim.

Corrupting **both** uberblock slots, by contrast, is a typed error:

```
error: fs("FsError::Io(\"corrupt filesystem: no valid uberblock\")")    # exit 1
```

**(b) Stale copy-on-write blocks — harmless, but they make naive corruption tests lie.**
Because nothing is overwritten in place, the image accumulates stale copies of data and
directories. Corrupting a stale block changes nothing (correct!), but anyone probing "does
eofs detect corruption?" by grepping the image for their filename/payload and flipping bytes
at the first hit has a good chance of hitting a stale copy and concluding "not detected."
Demonstrated live: corrupting the *stale* directory copy at offset 8192 → `ls` and `cat`
both still clean; corrupting the *live* one at 9216 → both fail loudly.

**(c) The read-back-with-`readwrite` methodology trap.** `readwrite` opens with
`CREATE|TRUNCATE`, writes, then reads its own write back. Using it to "verify" data after
corrupting the image overwrites the corruption before the read:

```
$ printf 'X' | dd of=/tmp/s7.img bs=1 seek=8704 count=1 conv=notrunc    # corrupt the data block
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ readwrite /keep.txt hello-eofs-corruption-canary-7"
ok: round-tripped(30)                                               # exit 0 — "verified"!
$ eo9 --disk /tmp/s7.img -c "fs.eofs $ cat /keep.txt"
hello-eofs-corruption-canary-7                                      # clean again; corruption overwritten
```

If the lost study used `readwrite` (the canonical fs demo program) to check the data, it
never actually read the corrupted block.

**Free-space corruption** (1 MiB into an image whose data fits in the first ~16 KiB): no
effect on anything, as expected.

**Error fidelity finding:** eofs-core has *typed* `ChecksumMismatch` and `Corrupt(...)`
errors, but the provider's error mapping (`guest/stubs/fs-eofs/src/lib.rs`, `map_error`)
flattens both into `FsError::Io(<display string>)` because the `eo9:fs` WIT error type has
no corruption/integrity variant. By the time it reaches a program (or the user), corruption
is indistinguishable from a flaky cable except by string-matching
`"block checksum mismatch"` inside a debug-formatted `fs("FsError::Io(\"…\")")` blob.

### 1.4 What `--disk` does to a file that is not an eofs image

The provider's documented rule is "a *blank* device (no magic in either slot) is formatted
in place; a device that has the magic but fails to mount is never reformatted." The
implementation's definition of "blank" is **"no eofs magic at offsets 0 and 4096"** — which
includes *any file full of someone else's data*:

```
$ dd if=/dev/urandom of=/tmp/s7-garbage.img bs=1m count=8       # 8 MiB of not-eofs (stand-in for
                                                                # an ext4 image, a tarball, anything)
$ eo9 --disk /tmp/s7-garbage.img -c "fs.eofs $ ls /"
ok: listed(0)                                                   # exit 0 — it FORMATTED it
$ xxd -s 4096 -l 16 /tmp/s7-garbage.img
00001000: 454f 4653 2d55 4200 0100 0000 0010 0000  EOFS-UB.........    # eofs lives here now
```

Point `--disk` at the wrong path once — a typo, a different VM's image, any file without
eofs magic at two specific offsets — compose `fs.eofs`, and that file is now an empty eofs
filesystem. No prompt, no refusal, exit 0. (`mkfs.eofs` without `--force` has the same
shaped hole: it only refuses when eofs magic is present; it happily formats a file full of
non-eofs data. But mkfs is at least an explicit "format this" command; the provider does it
as a side effect of *mounting*.) A blank file auto-formatting on first mount is a
documented convenience; "blank" meaning "anything that isn't already eofs" is a data-loss
trap.

### 1.5 Space accounting: every image has a finite write budget

Copy-on-write + commit-per-mutating-operation + **no reclamation reachable from anywhere**
means deleted/superseded blocks are never freed. eofs-core has a `gc()` entry point and a
`verify()` walk; neither is exposed through `eo9:fs`, the CLI, or anything else.

```
$ eo9 mkfs.eofs /tmp/s7-tiny.img --size 256K
$ for i in $(seq 1 200); do eo9 --disk /tmp/s7-tiny.img -c "fs.eofs $ readwrite /same.txt rewrite-number-$i-xxxxxxxxxxxxxxxxxxxx" || break; done
...
FAILED at rewrite #165: error: fs("FsError::NoSpace")              # exit 1
```

One ~50-byte file. 165 rewrites. The 256 KiB image is now permanently full of garbage
copies. And it gets worse:

```
$ eo9 --disk /tmp/s7-tiny.img -c "fs.eofs $ stat /same.txt"
file 0 bytes                                # the FAILED rewrite also destroyed the old content
$ eo9 --disk /tmp/s7-tiny.img -c "fs.eofs $ rm /same.txt"
ok: removed                                 # rm works...
$ eo9 --disk /tmp/s7-tiny.img -c "fs.eofs $ ls /"
ok: listed(0)                               # ...the filesystem is now EMPTY...
$ eo9 --disk /tmp/s7-tiny.img -c "fs.eofs $ readwrite /same.txt after-rm"
error: fs("FsError::NoSpace")               # ...and still cannot store 8 bytes. Forever.
```

Three distinct findings in that transcript:

1. **The write-budget brick.** An eofs image has a hard total-bytes-ever-written budget
   (roughly its size, in ~1.5 KiB commit increments for small files); when it's spent the
   image is dead — `rm` doesn't help (removed blocks aren't reclaimed either), the only
   recovery is `mkfs.eofs --force` and total data loss. The 16 MiB default image holds
   roughly ten thousand small-file commits before it bricks. Nothing warns as the frontier
   approaches the end; there is no `df`; the failure arrives as a generic `NoSpace` on an
   image that may be 99% garbage and 1% live data.
2. **A failed rewrite destroys the previous content.** The fs.eofs provider implements
   `TRUNCATE` as remove + recreate (its own committed transaction), then the write is a
   second transaction. When the write's commit fails (`NoSpace`), the truncation has already
   committed: the old content is gone and the new content never landed — in a copy-on-write
   filesystem whose entire design point is that updates are atomic. (The engine is fine;
   the provider's multi-transaction rewrite squanders the atomicity.)
3. **Space exhaustion is reported only at the moment of failure**, with no way to see it
   coming and no way back from it.

### 1.6 Concurrency: two processes, one machine

There is no locking anywhere in the stack — not on the store, not on the session, not on
the disk image (no `flock`, nothing in `crates/eo9-providers-unix/src/disk.rs`). Two
separate races fall out, and both were demonstrated:

**(a) The shell session race (no disk involved at all).** Two concurrent `eo9 -c "echo …"`
loops sharing one store:

```
eo9: error: cannot place <store>/objects/a95fe9… into the session bin view: Permission denied (os error 13)
eo9: error: cannot refresh the session bin view <store>/shell/bin: Directory not empty (os error 66)
error: cannot resolve `echo` (/bin/echo.wasm): FsError::NotFound
```

Roughly a third of the 20 invocations failed. Every `-c`/shell run rebuilds
`<store>/shell/bin` (the session's view of the store) and concurrent rebuilds race. Any
parallel use of `eo9 -c` on one machine — a Makefile with `-j`, CI, two terminals — hits
this.

**(b) The image-level race.** With the session race removed (separate stores per writer),
two writers rewriting *different* files on the *same* image, 10 iterations each:

```
--- writer A errors ---                          --- writer B errors ---
error: fs("FsError::Io(\"block checksum mismatch\"))   error: fs("FsError::Io(\"block checksum mismatch\")")
error: fs("FsError::Io(\"block checksum mismatch\"))   error: fs("FsError::Io(\"block checksum mismatch\")")
error: fs("FsError::Io(\"block checksum mismatch\"))
```

5 of 20 writes failed: each writer mounts the image independently, allocates from the same
frontier, and commits uberblocks over each other; a writer whose in-memory tree references
blocks the other writer just overwrote gets checksum mismatches. In this run the surviving
image happened to end up consistent (both files present with their last-written content);
that is luck, not design — lost updates are structural (each commit publishes only that
writer's view). The good news: the checksums turned a silent cross-writer corruption into
loud errors. The bad news: nothing prevents, documents, or even mentions the
single-writer-only constraint, and the image file is never locked.

## Demo round 2 — bare metal (QEMU aarch64)

*(to be added: `storedisk` power-cycle persistence — `save` a composition, run it,
power-cycle, the saved program + compile cache surviving; and `pci disk` — the wasm
virtio-blk driver + fs.eofs round-trip.)*

## Participant reactions

*(to be added after the demo transcript is complete.)*

## Findings (running list, to be finalized with the participant)

### Verified during the usermode session

1. **Cross-process persistence works as documented.** mkfs → `--disk` + `fs.eofs $ write` →
   new-process read-back, with fsync at commit boundaries. The opt-in grant posture
   (`--disk`, never ambient) held everywhere it was probed.
2. **Live-block corruption is always detected** (blake3 on every read path); corrupted data
   is never returned; detection arrives as a typed `fs(…)` failure with exit 1. The lost
   study's "silent corrupted read" does not reproduce against live data/metadata blocks.
3. **Newest-uberblock corruption = silent state rollback** (exit 0, no warning). Detection
   exists (the checksum is what triggers the fallback) but the *event* is invisible to the
   user. Real data loss can present as "file is fine, just empty/stale."
4. **`fs.eofs` reformats any non-eofs file it is pointed at** ("blank" = "no eofs magic"),
   silently, on mount. `mkfs.eofs` without `--force` formats non-eofs data too (it only
   protects eofs images).
5. **Corruption errors are flattened to strings** (`ChecksumMismatch` → `Io("block checksum
   mismatch")` → `fs("FsError::Io(\"…\")")` debug text) because `eo9:fs` has no integrity
   error variant. Programs cannot react to corruption distinctly; users get debug noise.
6. **Every image has a finite write budget, then it is permanently dead** — CoW garbage is
   never reclaimed, `gc()` exists but is unreachable, `rm` frees nothing, there is no `df`,
   no warning, no recovery short of reformatting.
7. **A failed rewrite destroys the file's previous content** (truncate commits before the
   write fails) — non-atomic update semantics layered on an atomic-by-design engine.
8. **No fsck / verify / health surface exists** anywhere a user can reach: eofs-core's
   `verify()` is not exposed via `eo9:fs`, the CLI, or the shell. After any corruption
   event, there is no tool to assess the damage.
9. **Concurrent `eo9 -c` invocations corrupt each other's session view** (store-level race,
   no disk needed); **concurrent writers on one image produce checksum errors and lost
   updates** (no locking, no documented single-writer rule).
10. **The missing-`--disk` refusal buries its remedy** after four clauses of linker
    internals (the friendly sentence exists but trails the jargon; the missing-`--fs-root`
    refusal leads with it).
11. **The README has no persistence story at all** — `mkfs.eofs`, `--disk`, and `fs.eofs`
    appear in `eo9 --help` and STATUS.md but not in README.md, despite being (per STATUS)
    one of the headline features of the last two waves.
12. R2-19 (outcome line glues onto unterminated program output) still reproduces on every
    `cat` of a file with no trailing newline.

### What landed well (usermode)

- The mkfs→grant→compose→persist loop is coherent and genuinely capability-shaped: the
  image file is the *only* shared state; no daemon, no mount table, no global namespace.
- Block-level integrity is real and on by default (blake3 on every read; lz4 + checksums
  per block; Merkle hashes up the tree). Most hobby filesystems never get this far.
- The mkfs reformat refusal + `--force` is exactly the right shape.
- Crash-consistency-by-construction (CoW + alternating uberblocks + commit = root flip) is
  the right architecture, and the both-slots-corrupted case fails loudly instead of
  guessing.
- fsync is actually wired end-to-end (file device `sync_all` at commit boundaries), not
  just claimed.
- Performance is a non-issue for the demo workloads (0.16 s per cold-process disk
  composition, release build).

*(Triage table to be completed after the metal demos and the participant session.)*
