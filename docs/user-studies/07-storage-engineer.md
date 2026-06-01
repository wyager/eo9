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

Six QEMU boots, driven over the serial console by a pacing script (one command per prompt,
per the plan/12 D49 console conventions). The kernel image (22-component baked store,
`wasm-storedisk` feature) rebuilt in ~14 s against a warm cache; each boot reaches the
`eosh>` prompt in ~12–16 s of host wall including the xtask rebuild check.

### 2.1 `storedisk` boot 1 — a blank disk becomes a persistent store

`cargo xtask qemu aarch64 storedisk` created a blank 64 MiB raw image and attached it as
virtio-blk; the kernel claimed and formatted it:

```
cmdline: storedisk
store: 22 components baked in (1956 KiB components, 15442 KiB artifacts): eosh, hello, …
storedisk: virtio-blk 131072 sectors (64 MiB) claimed for the kernel store
storedisk: blank disk formatted with eofs (block 4096, lz4 on)
storedisk: eofs mounted (txg 1), 0 cached compile artifact(s), 0 saved program(s)
eosh> save frozen-hello = time.frozen --now-seconds 5 --monotonic-ns 0 $ hello
storedisk: saved 82 KiB program as /bin/frozen-hello.wasm
saved: /bin/frozen-hello.wasm (run it as `frozen-hello`)
eosh> ls /bin
…(22 baked names)…
frozen-hello.wasm
ok: listed(23)
eosh> frozen-hello --name saved --excited true
storedisk: compile cache miss (compiled on-target in 1240 ms)
storedisk: cached 664 KiB of compiled code as 7e12829885de980e…
[5.000000000] Hello, saved!
ok: greeted
eosh> exit
eosh: session ended, outcome = ok(exited)
[ 6691923 us] kernel run complete; requesting PSCI SYSTEM_OFF
```

### 2.2 `storedisk` boot 2 — full power cycle: everything survives

A new QEMU process against the same image:

```
storedisk: eofs mounted (txg 3), 1 cached compile artifact(s), 1 saved program(s)
eosh> ls /bin
…ok: listed(23)                                  # frozen-hello.wasm still there
eosh> frozen-hello --name reboot --excited true
storedisk: compile cache hit (664 KiB loaded in 2037 us)
[5.000000000] Hello, reboot!                     # identical deterministic output
ok: greeted
eosh> rm /bin/hello.wasm                         # a BAKED name
error: fs("FsError::ReadOnly")                   # the kernel image stays the trust anchor
eosh> rm /bin/frozen-hello.wasm                  # the disk-saved one
storedisk: removed /bin/frozen-hello.wasm
ok: removed
eosh> frozen-hello --name gone --excited true
error: cannot resolve `frozen-hello` (/bin/frozen-hello.wasm): FsError::NotFound
```

**1240 ms of on-target Cranelift became 2.0 ms across a full power cycle**, the saved
program persisted, the deterministic output is bit-identical, baked names cannot be
shadowed or removed, and the save → run → reboot → run → rm lifecycle worked exactly as
documented in plan/12 D60. The cache key (`7e12829885de980e…`) is identical across boots —
the algebra produces the same executable bytes for the same composition every time.

### 2.3 `storedisk` boot 3 — tamper with the disk between boots

One byte of the cached-artifact region of the store-disk image was flipped (offset
247808, `dd`) while the machine was off. The cached artifact is 664 KiB of *native aarch64
code* that the kernel would otherwise `Component::deserialize` and run:

```
storedisk: eofs mounted (txg 4), 1 cached compile artifact(s), 0 saved program(s)
eosh> time.frozen --now-seconds 5 --monotonic-ns 0 $ hello --name tamper --excited true
storedisk: reading a cached artifact failed: ChecksumMismatch
storedisk: compile cache miss (compiled on-target in 1281 ms)
storedisk: cached 664 KiB of compiled code as 7e12829885de980e…
[5.000000000] Hello, tamper!
ok: greeted
eosh> time.frozen --now-seconds 5 --monotonic-ns 0 $ hello --name tamper2 --excited true
storedisk: compile cache hit (664 KiB loaded in 1935 us)
[5.000000000] Hello, tamper2!
```

Tampered native code was **detected, named, refused, recompiled, re-cached, and never
executed** — and the user is told. (Layering note: this byte-flip was caught by the eofs
block checksums, the *first* line of defense; the keyed-blake3 MAC behind it exists for an
adversary who can rewrite blocks *and* recompute the eofs Merkle chain. Forging that chain
end-to-end was out of scope for this session, so the MAC layer itself was not independently
exercised.) Mount-time counting is directory-level: the banner said "1 cached compile
artifact(s)" before discovering at read time that the artifact was corrupt.

### 2.4 `pci disk` boots 4–5 — the wasm virtio-blk driver + fs.eofs, across a power cycle

`cargo xtask qemu aarch64 pci disk`: a separate blank 64 MiB scratch image attached as a
virtio-blk PCI function; the *driver for it is a wasm component* (`disk.virtio`).

```
eosh> disk.virtio $ fs.eofs $ readwrite /metal.txt persists-across-power-cycles
disk.virtio: virtio-blk 131072 sectors (64 MiB), queue size 16, completion: INTx interrupt
pci: INTx delivery on line 3 served an interrupt wait (the cpu halted instead of polling)
ok: round-tripped(28)                                            # 32.9 s wall (on-target compile)
eosh> disk.virtio $ fs.eofs $ ls /
metal.txt
ok: listed(1)                                                    # 21.2 s wall
```

Full power cycle, then:

```
eosh> disk.virtio $ fs.eofs $ cat /metal.txt
persists-across-power-cycles                                     # 30.8 s wall
eosh> disk.virtio $ fs.eofs $ stat /metal.txt
file 28 bytes
```

The storage stack here is three composed wasm components — virtio-blk driver, eofs
filesystem, program — compiled on the machine, with interrupt-driven request completion
(the CPU halts in `wfi` during disk waits; the transcript line proves it). Data written
before the power-off was read back after it.

**The latency asymmetry is structural:** in `pci disk` mode there is no compile cache (the
`storedisk` grant cannot be combined with the guest-facing `pci`/`disk` grants until
machine-global device claiming lands), so *every* storage command pays 20–35 s of on-target
compilation under TCG. The boot mode that has the persistent compile cache can't have a
guest-visible disk, and the boot mode with the guest-visible disk can't have the cache.

### 2.5 Cross-environment portability — the same image file, three worlds

The scratch image was formatted and written *by the bare-metal kernel through the wasm
virtio-blk driver*. The same file then mounted in usermode on macOS:

```
$ eo9 --disk kernel/target/eo9-scratch-disk.raw -c "fs.eofs $ cat /metal.txt"
persists-across-power-cycles                                     # written on metal, read on macOS
$ eo9 --disk kernel/target/eo9-scratch-disk.raw -c "fs.eofs $ readwrite /from-macos.txt written-by-usermode"
ok: round-tripped(19)
```

…and back on metal (boot 6):

```
eosh> disk.virtio $ fs.eofs $ cat /from-macos.txt
written-by-usermode
```

A file written on bare-metal ARM through a wasm device driver is readable on macOS through
a host file device, and vice versa — same `fs.eofs` component, same engine, same on-disk
format, no conversion step. (The metal compile cache disk is the same format too: usermode
`mkfs.eofs`, the metal kernel, and the fs.eofs provider all produce mutually mountable
images.)

### Metal observations beyond the brief

- `env` at the metal prompt does not mention the `pci` grant or the virtio disk at all,
  even in the `pci disk` boot where the session clearly holds them (the composition runs).
  It also still says "programs get no writable filesystem on bare metal yet," which is now
  only true for the *fs* capability — `disk.virtio $ fs.eofs` is exactly a writable
  persistent filesystem, and on `storedisk` boots `/bin` itself is writable.
- `ls /bin` shows baked and disk-saved programs undifferentiated (known gap, plan/12 D60).
- Metal error rendering has the same debug-text problem as usermode:
  `error: fs("FsError::ReadOnly")`.

## Participant reactions

The participant received the full demo transcript (parts 1 and 2, with the facilitator's
narration but not the findings below) and replied in one structured sitting. Condensed,
their words where quoted.

**What this is, in their terms.** "A single-writer, checksummed, copy-on-write object store
with a bump allocator and a filesystem-shaped API. The data path — CoW, per-block blake3,
txg commit by root flip, alternating uberblocks — is the ZFS skeleton, and the demo shows
that skeleton actually works. What's missing is everything ZFS spent fifteen years building
*around* that skeleton: space reclamation, multi-mount protection, scrub, recovery tooling,
an error model, and operator safety." Summary verdict: "a correct core with no operational
armor"; as a storage system, "pre-alpha."

**What genuinely impressed them** (their three, "and I don't say this lightly"):

1. *Corruption tests A–C had exactly the right blast radius.* Live data block → that file
   errors, directory still lists; live directory → both error; stale CoW block → no effect.
   "That last one is the tell. It means the Merkle tree boundary is real — the checksums
   cover precisely the live tree and nothing else. A lot of 'we have checksums' systems
   fail that test because their integrity coverage is approximate. This isn't."
2. *The tampered compile cache (2.3).* "The best moment in the whole demo... Detect →
   refuse → regenerate is the correct lifecycle for cached executable code, and almost
   nobody does it." They also noted it produced "a real typed error" (`ChecksumMismatch`),
   unlike the filesystem API.
3. *Cross-environment portability (2.5), both directions, no conversion.* "You cannot fake
   that. It means the on-disk format is genuinely well-defined... Most prototype
   filesystems fail this the first time they leave the machine they were written on."

Honorable mention: 1240 ms → 2.0 ms across a power cycle with an identical cache key —
"the persistence layer doing its actual job."

**What broke their trust** ("in descending order of how fast it ends someone's career"),
each mapped to a production failure:

1. **Test D (silent rollback) is the disqualifier.** "One 4-byte corruption in the live
   uberblock and the filesystem *silently* rewinds to the prior txg. Exit 0. No warning. No
   log line." Production failure: "silent data loss that *propagates*. An application reads
   the rolled-back state, makes decisions on it, writes on top of it — now the lost
   transaction is permanently unrecoverable. Worse: your next backup faithfully captures
   the rolled-back state, so your backups are now wrong too." They distinguished the
   legitimate case (torn write *during* an unacknowledged commit should fall back — "that's
   the whole design") from this one: "this system can't distinguish 'commit never
   completed' from 'commit completed, was acknowledged, then rotted,' and it handles both
   silently. ZFS makes txg rewind (`zpool import -F`) an explicit, operator-invoked, loudly
   logged disaster action for exactly this reason."
2. **The uberblock geometry compounds it.** Two slots, adjacent, at offsets 0 and 4096:
   "eight corrupted bytes total destroy the filesystem with no recovery path," and "a
   *single* misdirected or torn 8 KiB write takes out both. ZFS puts four labels at both
   ends of the device... because misdirected writes and head crashes are spatially local."
3. **The auto-format of non-eofs files.** "This is 'I pointed the tool at the wrong LUN,'
   the oldest data-loss story in storage... The implementation's definition of 'blank' —
   *no eofs magic* — means *every file in the world that isn't already mine is blank*. That
   is a data destroyer documented as a convenience feature."
4. **The write-budget brick.** "This isn't 'GC is a TODO,' it's 'every volume bricks itself
   at a deterministic write count regardless of logical usage.'" Production failure: "a
   fleet of devices all hitting ENOSPC-of-death on roughly the same day, with reformat as
   the only remedy." Plus, inside the same test: "rewrite #165 truncated the file in one
   committed transaction, *then* failed the write for space — so the old contents are gone
   too. Failed write = lost old data."
5. **Zero concurrency control.** "Two writers on one image, both tripping checksum errors,
   and the image 'happened to be consistent' afterward. *Happened to be* is not a property;
   that's a coin that landed heads... Even SQLite gets file locking right." And on the
   session race: "Race 1 is arguably worse for the project's credibility: two concurrent
   `echo hi` invocations corrupt the *component store* a third of the time. That's not a
   storage bug, that's the launcher."
6. **The error model.** "Monitoring blindness. I cannot alert on corruption rate. I cannot
   distinguish 'the media is dying' from 'file not found'... The integrity error class is
   *the most important error class this filesystem has* — it's the headline feature — and
   it does not exist at the API boundary. People will end up string-matching on debug
   output, and it will break."

**What they would demand to see next:** a real crash test ("you demonstrated
corruption-at-rest detection, you did not demonstrate crash consistency... I want fault
injection: kill -9 at every write offset in a commit, torn-write simulation on the
uberblock itself"); flush semantics on each backend (macOS F_FULLFSYNC vs plain fsync; does
the wasm virtio driver issue VIRTIO_BLK_T_FLUSH); allocator design and gc()'s crash story;
scale (largest file, deep trees, 100k-entry directories, multi-GiB images); whether
rename/atomic-replace exists ("on a CoW filesystem, atomic replace should be the *easy*
path"); the MAC key's threat model; the engine's test/fuzzing coverage.

**Would they put data on it today?** "No. Not negotiable, and Test D alone is sufficient: a
system that can silently rewind committed, acknowledged data is not a storage system, it's
a cache. What it *is* fit for today: regenerable data — exactly the compile cache. Anything
where loss costs CPU time, not information." Re-evaluation preconditions, in their order:
(1) uberblock fallback becomes loud and operator-gated, with more slots spread across the
device; (2) auto-format removed entirely — "formatting is always explicit, always";
(3) mount locking / multi-mount protection; (4) `rm` frees space and gc/df/scrub are
reachable; (5) corruption is a typed error at the API; (6) crash fault-injection results,
not corruption tests.

**Top 3 pain points:** auto-format of non-eofs files; silent txg rollback; space exhaustion
with reformat-only recovery. **Top 3 missing things:** concurrency control; operational
tooling (df/scrub/fsck — "checksums without a reachable scrub means latent corruption sits
in cold data until the day you finally read it, which is the day you needed it"); a typed
integrity error class plus crash-injection evidence.

**Where they think the project is misreading its own demo** (their sharpest section):

- "They're conflating corruption detection with crash consistency. The pitch says 'crash
  consistency by construction,' and every test in the demo is a *bit-flip at rest*. Those
  are different failure models with different proofs."
- "They've documented the auto-format rule as a feature... nobody on the team has ever
  destroyed a volume by pointing a tool at the wrong path. Storage tooling earns its
  confirmation prompts in blood."
- "The capability model is securing the wrong direction for storage. They're proud —
  justifiably — that a program can't touch the disk without a grant. But for storage, the
  dangerous party isn't the program, it's the *operator issuing the grant*: `--disk
  wrong-file.img` is itself the destructive act, because granting triggers auto-format.
  They've built deny-by-default for programs and yes-by-default for data destruction."
- "Their canonical example program teaches the worst pattern their own filesystem has"
  (truncate-then-write; it caused both the data loss in 1.5 and the meaningless
  "verification" in test G).
- "The priority ordering is backwards, and the gap list proves it. They built on-target
  Cranelift compilation, a wasm virtio driver, deterministic compile-cache keys across
  power cycles — genuinely hard things — while gc() and verify() sit in the engine,
  finished, *unreachable from any user-facing surface*... For a storage system, the boring
  parts are the product."
- "The compile cache is the one consumer in the entire demo that handles corruption
  correctly... because *that* consumer was written by someone who knew the data was
  disposable and thought through the failure path. The lesson: every consumer of this
  filesystem needs the 2.3 treatment, and the API has to make that possible. Right now it
  doesn't."

**Their bottom line:** "The core is real, the integrity layer in the data path is genuinely
good, the portability result is impressive — and I would not let this within a mile of data
anyone cares about until Test D, 1.4, 1.5, and 1.6 are fixed. Those aren't polish items.
Each one is a named, well-understood production disaster that the storage field already
learned how to prevent twenty years ago."

## Facilitator verification of the participant's factual challenges

Three of the participant's claims were checked against the repository after the session
(they had no way to know; the answers go to whoever owns the follow-up):

1. **"Crash consistency is currently untested" — partly wrong, structurally right.** The
   engine *does* have a power-cut test suite the demo never mentioned:
   `crates/eofs-core/tests/crash.rs` runs a five-transaction scenario over a `CutDevice`
   that loses power at **every write boundary**, with torn final writes, then remounts,
   verifies, and compares state (`power_cut_at_every_write_boundary`). The engine-level
   crash-consistency claim is tested. What does **not** exist is what the participant
   actually asked for: fault injection through the *full stack* (provider + unix file
   device + a real `kill -9` mid-commit), and any demonstration of it. Their structural
   point — the demo (and the pitch) substitutes corruption-at-rest detection for crash
   evidence — stands.
2. **"Does flush actually reach stable media on macOS?" — yes.** Rust's
   `File::sync_all()` on Apple targets issues `fcntl(F_FULLFSYNC)` (verified in the pinned
   toolchain's std sources), which is the strong barrier the participant was worried plain
   `fsync` doesn't provide.
3. **"Does the wasm virtio driver issue VIRTIO_BLK_T_FLUSH?" — yes.** `disk.virtio`
   negotiates `VIRTIO_BLK_F_FLUSH` (feature bit 9) and issues `BLK_T_FLUSH` requests; when
   the device doesn't offer the feature it is write-through by spec and flush is a no-op
   (`guest/stubs/disk-virtio/src/lib.rs`).

Confirmed (participant right): there is **no rename or atomic-replace operation** in the
`eo9:fs` WIT (open/open-exec/read/write/list/stat/create-directory/remove only); the MAC
key is per-checkout, 0600, baked into the kernel image, and documented as tamper-evidence
rather than a secrecy boundary (plan/12 D58); the engine's test suite covers roundtrip,
crash, corruption, hostile images, snapshots, compression, and a model test, but there is
no fuzzing of the on-disk parser.

## Findings

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

### Verified during the metal session

13. **The `storedisk` lifecycle works end to end exactly as documented**: blank disk →
    kernel formats it → `save` persists a composition to `/bin` → on-target compile result
    cached to disk → full power cycle → saved program still listed, **compile cache hit at
    2.0 ms vs the 1240 ms miss**, bit-identical deterministic output, identical cache key
    across boots → `rm` of the saved program works, `rm` of a baked name is refused.
14. **Tampering with the metal store disk between boots is detected, named, and recovered
    from**: `reading a cached artifact failed: ChecksumMismatch` → recompile → re-cache;
    tampered native code never executed. The best corruption story in the system.
15. **The wasm virtio-blk driver + fs.eofs round-trip survives power cycles**, with
    interrupt-driven completion (`pci: INTx delivery … the cpu halted instead of polling`
    in the transcript).
16. **Full cross-environment format portability**: metal-written images mount in usermode
    and vice versa, byte-for-byte, no conversion.
17. **The cache and the guest disk are mutually exclusive on metal** (the storedisk vs
    `pci`/`disk` don't-combine rule), so every `disk.virtio $ fs.eofs $ …` command pays
    20–35 s of on-target compilation under TCG; the compile cache that would fix it cannot
    be present in the same boot.
18. **Metal `env` omits the pci/disk grants entirely** and still claims "no writable
    filesystem on bare metal" in boots where writable persistent storage is demonstrably
    available.

### What landed well

- The mkfs→grant→compose→persist loop is coherent and genuinely capability-shaped: the
  image file is the *only* shared state; no daemon, no mount table, no global namespace.
- Block-level integrity is real and on by default (blake3 on every read; lz4 + checksums
  per block; Merkle hashes up the tree). Most hobby filesystems never get this far.
- The mkfs reformat refusal + `--force` is exactly the right shape.
- Crash-consistency-by-construction (CoW + alternating uberblocks + commit = root flip) is
  the right architecture, and the both-slots-corrupted case fails loudly instead of
  guessing.
- fsync is actually wired end-to-end (file device `sync_all` at commit boundaries; virtio
  FLUSH on metal), not just claimed.
- Usermode performance is a non-issue for the demo workloads (0.16 s per cold-process disk
  composition, release build).
- The whole metal `storedisk` story (2.1–2.3): formatting, save/run/rm, the power-cycle
  cache hit, and tamper detection with recovery — every step printed what it was doing and
  did what it printed.
- Cross-environment image portability (2.5) — written on bare metal through a wasm driver,
  read on macOS — with zero ceremony.
- The engine's own test suite (crash/corruption/hostile/snapshots) is more serious than the
  demo surface suggests — the gap is in what's *reachable*, not what's *written*.

## Triage table

Every finding from this study, dispositioned **Fix now**, **Tracked** (needs a recorded
work item / roadmap slot), or **Owner decision** (design call). Nothing dropped.
Attribution: F = facilitator demo, P = participant.

| # | Finding | Who | Disposition |
|---|---|---|---|
| S7-1 | **Silent txg rollback when the newest uberblock is corrupted**: stale state served as current, exit 0, no warning anywhere; participant's #1 disqualifier ("not a storage system, it's a cache") | F+P | **Fix now** (minimum: a loud mount-time warning + the event surfaced in the fs error/reporting path when the highest-txg slot fails its checksum) **+ Owner decision** (the full design: operator-gated rewind vs warn-and-mount vs refuse; how to distinguish torn-commit fallback from rot) |
| S7-2 | **`fs.eofs` auto-formats any non-eofs file on first mount** ("blank" = no eofs magic at 2 offsets); `mkfs.eofs` formats non-eofs data without `--force` too | F+P | **Fix now** (tighten "blank" to all-zero uberblock slots — a one-function change that removes the data-destroyer while keeping the blank-device convenience) **+ Owner decision** (participant's stronger ask: remove auto-format entirely, formatting always explicit) |
| S7-3 | **Every image has a finite write budget then bricks**: CoW garbage never reclaimed, `gc()` exists but unreachable, `rm` frees nothing, no `df`, recovery = reformat | F+P | **Fix now** (run the engine's `gc()` at mount or post-remove in the provider, so space is reclaimed without new surface) **+ Tracked** (real space accounting: a `df`-equivalent, gc/scrub reachable from the CLI, eviction policy) |
| S7-4 | **A failed rewrite destroys the previous file content** (TRUNCATE = remove+recreate as its own committed txn; the later write's NoSpace leaves 0 bytes) | F+P | **Fix now** (provider: stage truncate+write in one transaction; the engine's CoW already makes this natural) |
| S7-5 | **Corruption errors flattened to `Io(string)`** — no integrity/corruption variant in the `eo9:fs` WIT error type; users get debug text, programs can't react, monitoring can't count | F+P | **Fix now** (WIT: add a `corruption` error case + map `ChecksumMismatch`/`Corrupt` onto it; planner owns wit/) |
| S7-6 | **Concurrent `eo9 -c` invocations corrupt each other's session bin view** (store-level race, no disk involved; ~⅓ failure rate in the test) | F+P | **Fix now** (lock or per-process session view under the store; this is a launcher bug, not eofs) |
| S7-7 | **No locking on `--disk` images**: concurrent mounts race at the allocation frontier, cross-writer checksum errors, lost updates; single-writer constraint neither enforced nor documented | F+P | **Fix now** (flock the image file on `--disk` open; refuse the second process with a clear message) **+ Tracked** (real multi-mount protection at the format level — owner/format change) |
| S7-8 | **No fsck / scrub / verify / df surface anywhere** (engine `verify()` finished but unreachable) | F+P | **Tracked** (an `eo9 fsck.eofs <image>` host-side command is the cheap first step — same pattern as `mkfs.eofs`; guest-reachable verify needs the WIT hash/verify surface that plan/14 already parks) |
| S7-9 | **Uberblock geometry**: 2 slots, adjacent, both in the first 8 KiB — one torn/misdirected 8 KiB write kills the volume; participant cites ZFS's 4 labels at both device ends | P | **Owner decision** (on-disk format change; weigh before any format-stability promise) |
| S7-10 | **The pitch/demos conflate corruption-at-rest detection with crash consistency**; engine-level power-cut tests exist (`tests/crash.rs`) but no full-stack (kill-9-through-the-CLI) fault injection | P (corrected by F) | **Tracked** (full-stack crash-injection harness; cite the engine suite in SPEC/docs so the claim is evidenced) |
| S7-11 | **The `readwrite` example teaches truncate-then-write** — the worst pattern for this fs (caused S7-4's loss and invalidated corruption-test methodology) | P | **Fix now** (rewrite the example as write-new-then-remove-old or add an atomic-replace example once S7-12 lands; also a docs note on read-back testing) |
| S7-12 | **No rename / atomic replace in `eo9:fs`** — "on a CoW filesystem, atomic replace should be the easy path" | P | **Owner decision** (WIT addition; interacts with S7-11) |
| S7-13 | **The missing-`--disk` refusal buries the remedy** after four clauses of linker internals | F | **Fix now** (same error-rendering pass as round-1 finding #3; flip the order: friendly sentence first, linker detail after) |
| S7-14 | **README has no persistence story** (mkfs.eofs / `--disk` / fs.eofs absent from the repo front page while STATUS calls it a headline feature) | F | **Fix now** (README section with the verified 1.1 transcript) |
| S7-15 | **Metal `env` omits the pci/disk grants** and still says "no writable filesystem on bare metal" in boots where writable persistent storage is demonstrably available | F | **Fix now** (kernel session-manifest text; same class as round-1 finding #18) |
| S7-16 | **The compile cache and the guest-visible disk are mutually exclusive per boot** (storedisk vs `pci`/`disk`), so the flagship storage demo pays 20–35 s per command | F | **Tracked** (already in GAPS as machine-global device claiming; this study adds the user-visible cost) |
| S7-17 | `ls /bin` shows baked vs disk-saved programs undifferentiated | F | **Tracked** (already recorded, plan/12 D60 remaining-rungs list) |
| S7-18 | Outcome line glues onto unterminated program output (round-2 R2-19, still reproducing) | F | **Fix now** (already dispositioned fix-now in round 2; still open) |
| S7-19 | **"Deny-by-default for programs, yes-by-default for data destruction"**: the operator-side hazard (the `--disk` grant itself triggers formatting) is outside the capability model's threat model | P | **Owner decision** (design framing; concretely resolved by S7-2's fix, but the blind spot deserves a SPEC paragraph on operator-side safety) |
| S7-20 | Mount-time banner counts artifacts from the directory without verifying them ("1 cached compile artifact(s)" shown for a corrupt artifact) | F | **Tracked** (minor; honest-counting nit, fold into S7-8's scrub work) |
| S7-21 | **Scale untested**: nothing in the demo (or the integration suite) exercises large files, deep indirect trees, large directories, or multi-GiB images | P | **Tracked** (test-suite work item alongside the round-1 hostile-component item) |
| S7-22 | No fuzzing of the on-disk format parser (mount/walk paths) against adversarial images (the hostile-image tests are hand-written) | P | **Tracked** (extends the existing round-1 fuzzing item #12 to eofs) |

### What this study adds to the cross-study picture

Rounds 1–2 established the pattern "trust losses come from documentation overclaim and
off-happy-path rough edges, never from the core model or speed." This round sharpens it
for storage: **the data path is genuinely sound (checksums, CoW, portability, the metal
cache lifecycle) and every trust loss came from the operational shell around it** —
formatting safety, space lifecycle, concurrency, error surfacing, and tooling reachability.
The participant's framing is the headline: *for a storage system, the boring parts are the
product.*

## Facilitator observations

- **The lost study's "silent corruption" claim is now explained, three ways.** A naive
  corruption test against this filesystem can produce a "silently read back corrupted
  data" conclusion via (a) corrupting a stale CoW block (harmless, unreferenced — grep
  finds those first), (b) corrupting the newest uberblock (the genuinely silent rollback,
  finding S7-1), or (c) "verifying" with `readwrite`, which overwrites the corruption
  before reading. Against *live data and metadata blocks* with a *real read*, detection
  never failed once in this session. Corruption-test methodology for this design: map the
  image, identify the live tree, corrupt that, and read with `cat` from a fresh process.
- **Exit-code annotations** in the transcript are quoted only where the code was captured
  directly after the command; a few probe runs during the session interposed an `echo` and
  their codes were discarded rather than guessed.
- **Timing caveat**: usermode numbers are a release build; the brief's `cargo build -p eo9`
  (debug) build is functionally identical but ~12× slower per disk-composition run (2.0 s
  vs 0.16 s), all of it host-side overhead. Metal numbers are QEMU TCG on an M-series Mac —
  on-target compile times (~1.2–1.3 s guest-reported) are not comparable to real silicon.
- **Driving QEMU**: the serial console was scripted (wait for the `eosh>` prompt, send one
  command at a time, paced character-by-character per plan/12 D49); no input was ever lost
  in six boots. The kernel's per-step narration (`storedisk: …` lines) made the metal demos
  essentially self-documenting — the usermode CLI could borrow that habit.
- **The store disk and scratch disk images** under `kernel/target/` were left as the demos
  produced them (the tampered cache entry was already healed by the kernel's own
  recompile); the usermode images under `/tmp` and the throwaway stores were deleted after
  the session.
