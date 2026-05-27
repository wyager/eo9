# User study 02 — senior security engineer

## Session metadata

- **Participant persona:** senior security engineer, 10+ years (sandboxing, container escapes,
  supply-chain security, capability systems; knows seL4 / Capsicum / pledge–unveil conceptually);
  no prior exposure to Eo9; interacted only with what the facilitator showed — never read the
  repository or ran tools themselves.
- **Format:** conversational demo session; the facilitator ran every requested experiment live and
  pasted real (trimmed) output. The session was handed over between facilitators after round 1;
  this report covers the full session.
- **Build under test:** `study/session-security` worktree, `eo9` debug binary built from that
  branch, wasmtime 45.0.0 (crates.io), macOS/aarch64 host.
- **Fixtures:** a study store at `/tmp/eo9-study-sec/store` seeded with the 35 bundled components
  (coreutils, examples, stub providers); a sandbox directory `/tmp/eo9-study-sec/sandbox`
  containing `notes.txt` and a pre-placed symlink `sneaky-link -> /tmp/eo9-study-sec/host-secret.txt`
  (a host file outside the sandbox root). Additional fixtures (a 4 MiB file, an in-tree symlink, a
  dangling outside symlink) were added during the session.
- **Honesty rules:** all output below is from real runs against the build above, trimmed only for
  length (long wasm backtraces and repeated lines elided, marked `[…]`). Failures and unexpected
  behavior are shown as they happened.

---

## Round 1 (previous facilitator) — summary

**Pitch given:** imports are permissions; a composition algebra (`provider $ program`,
`only <interfaces> $ program`) decides authority before the program runs; deny-by-default — a
capability you did not compose in simply is not there.

**Demos shown:** a filesystem-needing program refused without an explicit grant; the same program
running confined under a `--fs-root` sandbox grant; an `only` lockdown refusing a program whose
required imports exceeded the allow-list; `describe` / `env` showing per-import status for a
program in the session.

**Participant's opening reply (their points, abridged, in their words):** the UX is nice but so
far this is "a CLI that refuses to wire up imports it wasn't told to" — policy at the composition
layer; they wanted to poke one level down:

1. **Trust boundary.** When `cat` runs with an fs sandbox, what actually enforces confinement —
   they assumed a host-side path-mapping shim plus the Wasm runtime, so the TCB = that shim +
   runtime + compiler + the CLI itself. What exactly is in the TCB, what runtime/version does it
   sit on, and how does a CVE fix ship?
2. **Containment tests.** Run three traversal attempts (`../../../etc/passwd`, `/etc/passwd`, an
   encoded `..%2f` variant) plus a symlink-inside-the-sandbox test; is enforcement
   canonicalize-then-open (TOCTOU?) or openat2/`RESOLVE_BENEATH`-style?
3. **The `only` algebra.** Is the refusal a static check against declared imports or re-checked at
   call time; can a program granted fs spawn a child and forward that capability; does `only`
   constrain transitive flow; how do you attenuate a granted fs to read-only or a subdirectory;
   can you revoke mid-run?
4. **Resource limits.** A zero-import component can still spin CPU, allocate until OOM, recurse
   the stack — what limits exist on memory/fuel/handles and who enforces them; say out loud what
   is out of scope (timing side channels — a busy loop is a clock).
5. **Malicious-by-construction component.** They want a component written to try to escape, not
   just polite coreutils.

---

## Round 2 — facilitator: answers + first batch of experiments

### [1] Trust boundary / TCB

The participant's model was confirmed as essentially correct. For a usermode run the chain is:

- the wasm guest(s): the program plus any composed providers — **not** trusted for host isolation;
- **wasmtime 45.0.0** (crates.io release) with Cranelift — the isolation and codegen layer;
- **eo9-runtime** — the linker (only links interfaces the component imports and the session
  grants), task/fuel machinery, WAVE argument/outcome plumbing;
- **eo9-providers-unix** — the host-side fs/text/time/entropy providers; plain Rust running in the
  CLI process. The "path-mapping shim" the participant guessed at is the fs provider here;
- the **eo9 CLI binary** itself, plus the on-disk **module store and compile cache**
  (content-addressed objects; the cache holds compiled native code that a hit deserializes
  without codegen).

Guest-side stub providers (`fs.readonly`, `fs.memfs`, `time.frozen`, …) are *not* part of the
host-isolation TCB — they are wasm inside the same sandbox; a buggy or malicious one can weaken
its own attenuation promise but cannot reach beyond what the host root grant allows.

CVE story, stated honestly: in usermode wasmtime is an ordinary cargo dependency — bump, rebuild,
re-ship; cache keys include the compiler version so stale compiled artifacts miss and get
recompiled. Two honest caveats were given: (a) the project records that moving off wasmtime 45 is
not free — the Component Model async ABI internals are still churning upstream and an internal
binder depends on ABI constants that must be re-verified per bump; (b) on bare metal the kernel
vendors forked, no_std-patched copies of the wasmtime/cranelift compile layers, so a CVE there
means patching vendored forks by hand. The usermode binary's dependency tree is ~190 unique
crate versions.

### [2] The traversal and symlink tests (run live)

Control first, then the participant's three attempts plus the symlink, then a host-side check that
the *same user* can read the targets directly (so a refusal is the provider's doing, not file
permissions):

```
$ eo9 --store … --fs-root /tmp/eo9-study-sec/sandbox cat --path notes.txt
hello from sandbox
success(printed(19))                                   # exit 0

$ … cat --path ../../../etc/passwd
failure(fs("FsError::Denied"))                         # exit 1
$ … cat --path /etc/passwd
failure(fs("FsError::NotFound"))                       # exit 1
$ … cat --path '..%2f..%2f..%2fetc%2fpasswd'
failure(fs("FsError::NotFound"))                       # exit 1
$ ls -la sandbox/sneaky-link
sneaky-link -> /tmp/eo9-study-sec/host-secret.txt      # target exists, outside the root
$ … cat --path sneaky-link
failure(fs("FsError::Denied"))                         # exit 1

$ head -3 /etc/passwd            # host-side, same user: readable
##
# User Database
$ cat /tmp/eo9-study-sec/host-secret.txt
top secret host file
```

Explanation given: enforcement is **lexical normalization + canonicalize-then-operate, not
openat2/`RESOLVE_BENEATH`**. Guest paths are normalized lexically — any `..` component is refused
outright before touching the filesystem (hence `Denied` for the first attempt); a leading `/`
means the provider root, not the host root (hence `NotFound` for `/etc/passwd` — it looked for
`<sandbox>/etc/passwd`); `%2f` is never URL-decoded, so the third attempt is a literal filename
lookup inside the sandbox (`NotFound`). For paths that exist, the provider canonicalizes
(following symlinks) and requires the result to stay under the canonicalized root — that is what
denied the symlink. In-tree symlinks are allowed (demonstrated in round 3).

The TOCTOU question was answered yes, honestly: canonicalize-then-operate is not atomic; a racing
host-side actor can swap a path component between the check and the operation. This is a
documented known gap; the planned fix is a per-component `O_NOFOLLOW` walk or
`openat2(RESOLVE_BENEATH)`, deferred past the MVP.

### [3] The `only` algebra, spawning, attenuation, revocation

Static vs call-time: both, in a specific sense. The gate is a compose-time static judgment (it
fails before anything is instantiated, naming offenders), and at run time there is nothing to
re-check because an unsatisfied import is simply never linked — denial is the absence of wiring,
not a filter in front of a syscall.

Live demos (note the allow-list spelling — see the doc-mismatch finding below):

```
$ eo9 --store … shell -c 'only eo9:text/text,eo9:time/time $ hello --name boxed --excited true'
[1779920017.887400000] Hello, boxed!
ok: greeted                                            # exit 0

$ … shell -c 'only eo9:text/text $ hello --name boxed --excited true'
error: `only` failed: RestrictError::RequiredOutsideAllowList(["eo9:time/time@0.1.0"])   # exit 1

# Gate beats grant: the session HAS --fs-root, the gate still refuses cat before run
$ eo9 --store … --fs-root sandbox shell -c 'only eo9:text/text,eo9:time/time $ cat --path notes.txt'
error: `only` failed: RestrictError::RequiredOutsideAllowList(["eo9:io/buffers@0.1.0", "eo9:fs/fs@0.1.0"])
```

The README/SPEC spelling `only eo9:text,eo9:time $ hello` (package-level shorthand) does **not**
work on this build:

```
$ … shell -c 'only eo9:text,eo9:time $ hello --name boxed --excited true'
error: `only` failed: RestrictError::InvalidAllowList("`eo9:text` is not an interface name (expected `namespace:package/interface`)")
```

Spawning / transitive flow: spawning is itself a capability (`eo9:exec/*`). `cat`/`readwrite` do
not import it, so they cannot spawn anything, period. Of the seeded programs only the shell
(`eosh`) imports `eo9:exec/*` (`describe eosh` shown). Children spawned through exec get exactly
what their composed image carries plus what the embedder's child policy hands out — never the
parent's own host-side authority, and never exec itself. The session's `env` states this and it
was demonstrated directly:

```
$ eo9 --store … --fs-root sandbox shell -c 'env'
capabilities granted to this shell:
  text / time / entropy / fs (session dir) / exec
programs started from this shell receive:
  text / time / entropy / fs  host directory /tmp/eo9-study-sec/sandbox (from --fs-root)
  note: children never receive the exec capability

$ … shell -c 'eosh'        # try to start a nested shell as a child
error: spawn failed: SpawnError::Internal("component imports instance `eo9:exec/component-algebra@0.1.0`,
  but a matching implementation was not found in the linker: …")        # exit 1
$ … shell -c 'cat --path notes.txt'    # control: ordinary child fine
hello from sandbox
ok: printed(19)
```

Attenuation: read-only is composition with `fs.readonly`:

```
$ … --fs-root sandbox shell -c 'fs.readonly $ readwrite --path note2.txt --contents hi'
error: fs("FsError::ReadOnly")                          # exit 1
$ … --fs-root sandbox shell -c 'fs.readonly $ cat --path notes.txt'
hello from sandbox
ok: printed(19)                                         # reads still fine
$ … --fs-root sandbox shell -c 'readwrite --path note2.txt --contents hi'
ok: round-tripped(2)                                    # control without the wrapper
```

Subdirectory attenuation: today that is "choose a narrower `--fs-root` at grant time"; there is no
re-root-to-a-subdirectory provider in the algebra yet (admitted as a gap).

In-memory fs: `fs.memfs` exists, but composing it onto an arbitrary consumer **trapped**:

```
$ eo9 --store … shell -c 'fs.memfs $ readwrite --path scratch.txt --contents hi'
abnormal: trapped: … panic in eo9_stub_fs_memfs.wasm: ProviderState::with …
  "provider used before `configure` bound its state" […full wasm backtrace elided…]   # exit 1
```

Facilitator's honest explanation: the memfs provider needs its `configure` entry called before its
fs export is used; the algebra's `configure` operation does not support resource-owning providers
yet (a documented gap), and nothing calls it for you, so the first `open` hits a guest panic. The
deterministic-environment integration tests pass because their fixture program calls the config
interface itself. Containment held (the panic became a trapped outcome; the host was unaffected),
but the failure mode is a panic with a backtrace, not a clean "this provider needs configuration"
error, and the README's deterministic-environment pitch does not work against off-the-shelf
consumers like `readwrite` from the shell today.

Revocation mid-run: no. Authority is decided before run and fused in; the lever during a run is
`kill` (with a documented kill/linearity contract). Nothing like Capsicum per-fd revocation or
seL4 cap deletion mid-run exists.

### [4] Resource limits

```
# control: count a 4 MiB file, no limit
$ eo9 --store … --fs-root sandbox wc --path big.bin
16304 95858 4194304
success(counted)                                        # exit 0

# linear-memory ceiling hit mid-run: contained, surfaces as a trap
$ … --max-memory 1500000 wc --path big.bin
abnormal(trapped("… cabi_realloc … prefix_to_vec … wasm `unreachable` executed"))   # exit 2

# ceiling below the program's initial memory: refused at spawn
$ eo9 --store … --max-memory 65536 hello --name tiny --excited false
eo9: error: cannot spawn hello (…): spawn failed: memory minimum size of 17 pages exceeds memory limits   # exit 3

# guest panic containment
$ eo9 --store … outcomes --mode trap --detail boom
abnormal(trapped("… panic_fmt … main: wasm trap: wasm `unreachable` instruction executed"))   # exit 2
```

Stated limits and their enforcement points: linear memory — `--max-memory`, enforced by the
runtime at `memory.grow` (and at spawn if the initial memory already exceeds it); host-side I/O
buffers (`eo9:io/buffers`) are host memory outside the guest ceiling but capped at 16 MiB per
buffer / 64 MiB total per task; the exec surface caps live handles (32 components / 64 MiB of
component bytes / 16 images / 8 children per exec-holding task). Guest stack overflow becomes a
trap.

Honest admissions, unprompted: CPU is fuel-metered (codegen-inserted yields, 10 000-unit quantum,
donation-based), but the CLI's drive loop just keeps donating until the task finishes — **there is
no user-facing CPU budget flag yet, so a zero-import busy loop spins until you Ctrl-C the host
process**; on bare metal child fuel is not implemented at all yet. The mid-run memory-ceiling
failure mode is a trap (allocation failure → panic), not a graceful out-of-memory error to the
guest. There are no disk-space quotas and no per-task open-file-count quota on the fs provider.
Timing side channels are design-stage: the design answer is "fine-grained time is a capability"
(`time.fuzzy` / `time.frozen` / `time.none`, and shared-memory threads are simply not granted),
but a busy loop is indeed a coarse clock, nothing here is constant-time, and Spectre-class
hardening beyond "don't hand out clocks or threads" is not implemented.

### [5] Malicious-by-construction component

The facilitator declined to improvise one mid-session and said so: the coreutils are not that, and
writing a purpose-built hostile component against the guest SDK was judged more than the session
budget allowed. What the session substituted, honestly labeled as a substitution: `cat`/`readwrite`
are confused deputies — the guest passes attacker-chosen paths straight into `fs::open`, so the
traversal/symlink suite exercises exactly the fs surface a malicious component with the same
imports would have; the memory/buffer/trap tests cover the "allocate until OOM / crash" class.
What it does not cover: a component that hammers the exec surface, churns resource handles, or
abuses the async ABI deliberately. Logged as an open request (see Findings).

---

## Round 2 — participant's reply (abridged)

1. The traversal results look right — lexical `..` refusal plus no decoding is the correct call,
   and mapping `/` to the sandbox root is chroot-like and fine. One probe-ish question: `Denied`
   vs `NotFound` differ — can a guest use that error distinction to learn anything about host
   files outside the root?
2. TOCTOU: they want specifics. Between which two operations is the window; is the final open
   `O_NOFOLLOW`; do you re-verify the opened descriptor after open; and who realistically is the
   racing attacker — they noted, pointedly, that this very demo's sandbox root lives under `/tmp`.
   Until openat2-style resolution lands, would the project at least re-check after open?
3. The cache holding native code that a hit "deserializes without codegen" makes
   `~/.eo9/store` part of the TCB. Prove the failure modes: tamper a store object and a cached
   image and show what eo9 does.
4. The memfs panic worries them less as a safety issue than for what it implies: providers sit in
   the consumer's data path. A malicious "fs.readonly" could observe or alter every file
   operation. Where do providers come from, is anything signed, and how would they pin/verify a
   third-party provider?
5. The README spelling of `only` not matching the implementation is a smell — "if the quick-start
   doesn't run as written, what else is aspirational?" Also: show that `describe`/`imports` on a
   composed-and-gated artifact actually reflects the reduced authority — that is the audit story
   they care about. And show an in-tree symlink working, so the symlink denial isn't just "all
   symlinks are banned".

---

## Round 3 — facilitator: TOCTOU detail, tamper tests, provenance, audit demo

### Symlinks, error channel

```
$ ln -s notes.txt sandbox/in-tree-link
$ … cat --path in-tree-link
hello from sandbox
success(printed(19))                                   # in-tree symlink: allowed

$ ln -s /nonexistent-host-path-xyz sandbox/dangling-outside-link
$ … cat --path dangling-outside-link
failure(fs("FsError::NotFound"))                       # dangling outside target
# (recall: sneaky-link, whose outside target EXISTS, gave FsError::Denied)
```

Honest answer to the probe question: a guest can never *name* a host path outside the root (`..`
is refused lexically and `/` means the sandbox root), so the error distinction cannot be used to
probe arbitrary host paths. But the in-tree-symlink test pair above shows a real, narrow leak: for
a symlink that already exists inside the sandbox, the guest can distinguish "outside target
exists" (`Denied`) from "outside target does not exist" (`NotFound`). The guest cannot create
symlinks through the provider (there is no symlink op in the fs API), so it can only probe links
someone else placed in the root. Acknowledged as a real, if minor, information channel.

### TOCTOU specifics

Stated plainly from the implementation: resolution is `symlink_metadata` → `canonicalize` →
prefix check against the canonicalized root → then a plain `OpenOptions::open` of the resolved
path. The final open is **not** `O_NOFOLLOW` and there is **no post-open re-verification** of the
descriptor. The window is between canonicalization and open (and within the multi-step
canonicalization itself). Who can race: only something with host-side write access inside (or to)
the granted root — the guest itself has no other handle to the host fs. In single-user usermode
that attacker can usually just read the target directly, so the practical risk concentrates where
the root is shared or under a world-writable parent — the participant's `/tmp` observation was
conceded as exactly the right example. The openat2/`RESOLVE_BENEATH`-style walk is the recorded
post-MVP fix; the participant's "at least fstat/re-check the opened fd in the meantime" was
accepted as a reasonable interim hardening request and logged.

### Store / cache tamper experiments (run live)

Store object (the `cat` component, blake3-named object file):

```
# objects are stored read-only (mode 0444, hard-linked); a same-user attacker can chmod, so we do
$ chmod u+w objects/27a1ef… && flip 1 byte at offset 5000
$ eo9 --store … --fs-root sandbox cat --path notes.txt          # normal cache-HIT path
eo9: error: store object for cat no longer matches its content hash
     (expected 27a1ef…, found d7a1e2…)                           # exit 3
$ … --debug-info cat --path notes.txt                            # different compile opts → cache MISS
eo9: error: store object for cat no longer matches its content hash (…)   # exit 3
# restore + chmod 444: control run is back to normal
hello from sandbox
success(printed(19))
```

Compile-cache image (the cached native-code artifact for `cat`; file is mode 0644):

```
$ flip 16 bytes at offset 150000 of cache/6bf46c…/image
$ eo9 --store … --fs-root sandbox cat --path notes.txt
eo9: warning: ignoring compile-cache entry 6bf46c…: image bytes do not match their
     recorded content hash (expected b35b5e…, found 52c470…)
hello from sandbox
success(printed(19))                                   # fell back to recompiling from source; exit 0
# restore: control run identical, no warning
```

So: source objects are verified against their content hash on every run (tampering is a hard
error), and cached native images carry a recorded blake3 that is checked before the unsafe
deserialize, with a clean fall-back to recompilation. The facilitator then volunteered the limit
of this before being asked: the recorded hash lives in the cache entry itself, so this is
**integrity against corruption and drift, not authentication** — anyone who can write the cache
directory can re-seal a well-formed envelope around bytes of their choosing, and `Image::deserialize`
trusts well-formed input. In single-user usermode that attacker could equally replace the `eo9`
binary, so it is not a new boundary; it becomes one the moment a store is shared between users or
shipped around. There are no signatures anywhere yet.

### Provider provenance / supply chain

Honest answers: the seeded components (including all stub providers) are embedded in the `eo9`
binary itself, so today their provenance is the binary's provenance. Anything else arrives via
`eo9 store add` — content-addressed (blake3) but unsigned; there is no signing, no trust root, no
provenance metadata, no revocation story for components. Pinning today = pinning content hashes
yourself. The interposition concern was conceded as valid: a provider sees every call of the
interface it exports to the consumer; the algebra makes the *presence* of an interposing layer
visible and content-addressed, but nothing yet vouches for what the layer is.

### Audit story demo

```
$ eo9 --store … shell -c 'describe only eo9:text/text,eo9:time/time $ hello'
kind: binary
imports:
  required eo9:text/types … eo9:text/text … eo9:time/types … eo9:time/time
exports: (none)                                         # import surface ⊆ the gate's allow-list

$ … shell -c 'describe fs.readonly $ cat'
kind: binary
imports:
  required eo9:io/buffers … eo9:fs/types … eo9:fs/fs … eo9:text/types … eo9:text/text
```

The facilitator pointed out the catch unprompted: `describe fs.readonly $ cat` is
indistinguishable from `describe cat` — the residual import surface is the same because the
read-only wrapper itself still needs an fs from outside. The attenuation is real (round 2 demo)
and the composed artifact has a different content hash, but `describe` shows the boundary surface,
not the internal wiring, so an auditor cannot see "there is a read-only layer in here" from
`describe` alone. Logged as a tooling gap (something like a wiring/tree view).

---

## Round 3 — participant's reply (abridged)

- Tamper results are better than expected — verified-on-every-run for sources, integrity-checked
  envelope with recompile fallback for the cache is the right shape. But they repeated it back
  precisely: "this is integrity, not authentication." What is the plan for signing — and what is
  the multi-user / bare-metal story, where "same user could replace the binary anyway" stops being
  an excuse?
- TOCTOU answer accepted as honest; the fd re-check stays on their list. The in-tree-symlink
  existence probe is minor but real; note it.
- The `describe` blind spot matters to them: for a security review they need to enumerate not just
  the import surface but the layers ("what stands between this program and the real fs").
- Resource limits: the missing CPU budget is the weakest of the four limit stories — would the
  project accept a `--max-fuel` analog of `--max-memory`? What about disk-space and
  open-handle quotas on the fs provider?
- What does a security review of an Eo9 application actually look like today — what artifact do
  they sign off on, and what tooling exists to diff it against what runs?
- Where does Eo9 actually sit relative to containers / microVMs / Capsicum-style in-process
  confinement — what is the threat model where this wins?
- The malicious-component request stands: what would it take, concretely?

## Round 4 — facilitator: honest answers (no new demos requested)

- **Signing / sharing:** not designed yet. Content addressing gives identity, not authority or
  provenance; a shared or shipped store needs signatures over object hashes plus a trust policy,
  and none of that exists. On bare metal today the store is baked into the kernel image, so the
  trust question collapses into "who built the image" — fair for a demo, not an answer for real
  deployments.
- **CPU/disk/handle quotas:** agreed the CPU budget is the weakest limit story. The metering
  mechanism (fuel) exists and is what makes tasks pre-emptible-by-construction; what is missing is
  a user-facing budget (`--max-fuel` or a `limit` gate) and that is a small CLI/API addition, not
  a redesign — logged as a concrete feature request. No disk-space quotas; no per-task open-file
  cap in the fs provider (only the host process's rlimit); both logged.
- **Security review today:** review the composition expression (the shell line or the embedding
  code), `describe` of the artifact that will actually run (its full import surface and argument
  signature), the content hashes of every module involved, the run-time grants (`--fs-root`, the
  session policy for children), and the TCB pins (eo9 commit, wasmtime version). What is missing:
  an exportable manifest/SBOM-like artifact that captures all of that in one signed document, an
  audit log of grants at run time, and the layer-visibility tooling from round 3. The deterministic
  compile cache is a building block for "the artifact I reviewed is the artifact that runs", but
  codegen determinism is not yet verified bit-for-bit across machines, so that claim is not made.
- **Positioning:** stated plainly — this is not a container/microVM replacement today and should
  not be reviewed as one. Wasmtime is the load-bearing isolation; Eo9's contribution is what sits
  on top: per-import, compose-time least authority with an inspectable artifact, attenuation and
  determinism as ordinary composition, and the same model down to bare metal. Capsicum/pledge are
  the closest spiritual relatives (capability discipline / drop-everything-early) but operate on
  fds and syscall classes inside one process, whereas here the unit is a typed interface on a
  separate, memory-isolated component. For hostile-tenant isolation today you would still put the
  whole thing inside your existing sandbox/VM boundary (defense in depth), and that
  recommendation was given explicitly.
- **Malicious component cost:** roughly half a day with the existing guest SDK (a no_std Rust
  crate targeting a minimal world; the SDK macros do the bindings) — e.g. one component that takes
  no arguments, declares only `fs`, and walks every escape trick it can think of, plus one that
  abuses the exec/handle surface. Not done in this session; logged as the participant's strongest
  outstanding "prove it".

## Round 4 — participant's reply (abridged)

- Accepts the positioning ("inside an existing boundary, capability hygiene and auditability on
  top") as honest and more credible than the "secure OS" framing alone.
- Bare metal: on metal there is no MMU privilege separation by design — so what stands between a
  Cranelift miscompile (or a malicious pre-compiled image) and full machine compromise? Is W^X at
  least enforced for JIT-published code? What is the kernel's TCB?
- Side channels: they take the point that time is a capability, but want it on the record that
  fuel-determinism does not stop an adversary timing *itself* against external observation, and
  that `time.fuzzy` etc. are stubs, not evaluated mitigations.
- Testing/assurance: what security-relevant testing exists today — unit tests for the escape
  paths? fuzzing? any external review?

## Round 5 — facilitator: bare metal, side channels, assurance (honest status)

- **Bare metal TCB:** the kernel (boot, MMU setup, providers, executor), the vendored no_std
  wasmtime/cranelift compile layers, and the algebra closure — all in one privilege domain with
  the guests' compiled code. A compiler bug that emits an unchecked memory access is game over on
  metal; that is inherent to the language-level-isolation bet and the spec says so (the privilege
  line is drawn at codegen — "a compiler bug mints unsafe native code and harms everyone").
  Current hardening status, stated plainly: **W^X for JIT-published code pages is still TODO**
  (cache maintenance is done; QEMU tolerates the missing W^X, real hardware should not), the
  executor still busy-polls, there is no child fuel on metal yet, and exceptions are fatal. The
  bare-metal MVP is a capability demo, not a hardened kernel, and was presented as such.
- **Side channels:** agreed and put on the record — the mitigations are design-stage. `time.fuzzy`
  / `time.frozen` exist as stub providers but have not been evaluated against real attacks; a busy
  loop remains a coarse clock; nothing is constant-time; denying shared-memory threads (they are a
  capability that simply is not granted) removes the classic high-resolution-timer construction
  but is not a complete answer.
- **Assurance today:** provider unit tests cover the escape/symlink attempts and the open-exec
  immutability semantics; an integration suite covers the capability laws (sealing, `only`,
  optional absence), determinism, and kill/linearity; CLI transcript tests cover the refusal
  messages. There is no fuzzing harness, no penetration-style adversarial test suite, and no
  external review. The session's own findings (memfs panic, README/`only` mismatch, raw error
  surfaces) came out of exactly the kind of off-script poking the test suite does not do.

## Round 6 — wrap-up (participant's closing assessment, abridged)

**Top concerns (their ranking):**
1. The TCB is "wasmtime + a young Rust codebase + a compiler", and on bare metal it is all one
   privilege domain with W^X still pending — the security of the whole story currently rests on
   upstream wasmtime quality plus codegen correctness, not on anything Eo9 adds.
2. No signing/provenance for components or stores; content addressing is identity, not trust.
   Providers interpose on consumers, which makes provider provenance a first-class supply-chain
   problem the moment anything third-party enters the store.
3. The TOCTOU window in the fs provider — narrow attacker model in single-user usermode, but it is
   exactly the kind of thing that gets forgotten when the same provider gets reused somewhere the
   attacker model is wider (shared roots, multi-user hosts, anything under /tmp).
4. No CPU budget exposed (and none at all on metal yet) — availability is part of security.
5. Polish signals: README examples that don't run, raw `RestrictError::…`/`SpawnError::Internal`
   strings, a stub provider that panics when composed naively. None are vulnerabilities; all
   erode confidence in the claims around them.

**Evidence they would need to trust it:** a hostile-component test suite (their round-1 ask) run
in CI; the openat2/`RESOLVE_BENEATH` fix or at least post-open re-verification; W^X on metal;
signed stores or a stated single-machine-only trust model; bit-for-bit codegen determinism so
"reviewed artifact == running artifact" is checkable; fuzzing of the WIT/ABI boundary and the fs
provider; eventually an external review of the runtime/linker glue.

**Missing security features they called out:** `--max-fuel` / CPU budget, disk and handle quotas,
subdirectory re-rooting as an algebra-level attenuation, mid-run revocation (or an explicit
statement that kill is the only lever), a layer-visibility/audit view of a composition, signing,
and an audit log of grants.

**What felt like hand-waving:** the side-channel story ("time is a capability" is a design
position, not an evaluated mitigation); "deterministic by construction" while codegen determinism
is unverified; the README's deterministic-environment one-liner given the memfs behavior they
watched.

**What impressed them:** deny-by-default being real and pre-execution (the gate refuses before
instantiation, with the offending imports named); the containment tests all behaving correctly
with no decode-tricks or symlink surprises, and honest in-tree-symlink semantics; attenuation by
composition (`fs.readonly`) actually working; children never inheriting exec, demonstrated rather
than asserted; store objects verified on every run and the cache integrity check with clean
recompile fallback; and the facilitator showing failures (memfs trap, README mismatch) without
being pushed.

**What they would attack first:** the host-side fs provider (path handling, the TOCTOU window,
error channels) and the WIT/canonical-ABI boundary of the runtime linker — "the algebra is the
pretty part; the providers and the glue are where the bugs will be." Second target: the compile
cache and store on any machine where another principal can write to them.

---

## Findings

### Containment tests — what was requested, and what actually happened

| Request | Result |
|---|---|
| `cat ../../../etc/passwd` under sandbox grant | Refused: `FsError::Denied` (lexical `..` rejection), exit 1 |
| `cat /etc/passwd` | `FsError::NotFound` — `/` maps to the sandbox root, never the host root |
| `cat ..%2f..%2f..%2fetc%2fpasswd` | `FsError::NotFound` — no decoding; treated as a literal name |
| symlink inside sandbox → existing host file outside | Refused: `FsError::Denied` (canonicalized target escapes root) |
| in-tree symlink | Allowed (works as a normal file) |
| symlink → nonexistent outside path | `FsError::NotFound` (see information-channel note below) |
| tamper a store object, then run | Detected on every run path tried: hard error naming expected/actual hash, exit 3 |
| tamper a cached native image, then run | Detected: warning, entry ignored, recompiled from verified source, correct output |
| nested shell as a child (exec inheritance) | Refused at spawn: children never receive exec |
| `--max-memory` below initial memory | Refused at spawn |
| `--max-memory` hit mid-run | Contained; surfaces as `abnormal(trapped(…))`, exit 2 |
| guest panic (`outcomes --mode trap`) | Contained; `abnormal(trapped(…))`, exit 2 |
| malicious-by-construction component | **Not provided** — substituted with the above; logged as the top outstanding request |

No containment test behaved contrary to the project's claims. Two demos *around* containment did
not behave as documented or expected: `fs.memfs $ readwrite` traps with a guest panic
("provider used before `configure`"), and the README's `only eo9:text,eo9:time` spelling is
rejected by the implementation (full `namespace:package/interface` names are required).

### Concerns / threats raised by the participant

- TCB weight and provenance: wasmtime 45 + Cranelift + the eo9 runtime/providers/CLI (~190 crate
  versions in the usermode binary); vendored forks on the kernel side make CVE response manual.
- TOCTOU window in the unix fs provider (canonicalize-then-operate, no `O_NOFOLLOW`, no post-open
  re-verification); sharpest where sandbox roots live under shared/world-writable parents.
- Integrity vs authentication: store and cache checks are unkeyed blake3; no signing, no trust
  model for shared stores, providers interpose on consumers without any provenance story.
- No CPU budget in the CLI (busy loop runs until Ctrl-C); no child fuel on metal; no disk or
  open-handle quotas.
- Bare metal: single privilege domain, W^X still TODO, compiler correctness is the whole game.
- Side channels acknowledged as design-stage only.
- Minor information channel: a guest can learn whether the outside target of a pre-existing
  in-sandbox symlink exists (`Denied` vs `NotFound`).

### "Prove it" requests and how they went

- Traversal/symlink suite: ran as requested; all refused; control reads/writes worked.
- Host-readability check (that the refusals weren't just file permissions): shown.
- Store-object and cache-image tampering: ran; both detected; cache falls back to recompile.
  (First attempt at the object tamper was silently blocked by the store's read-only file modes —
  itself a small positive finding — and was redone with an explicit chmod.)
- Audit story (`describe`/`imports` on composed artifacts): shown; works for the gate, but an
  interposed attenuator is invisible in the residual import surface — gap acknowledged.
- Exec inheritance: nested-shell refusal demonstrated.
- Malicious-by-construction component: not delivered in-session; substitution declared openly.

### Hardening / feature requests (participant)

1. Post-open re-verification of the resolved descriptor now; openat2/`RESOLVE_BENEATH`-style
   resolution as the real fix.
2. `--max-fuel` (or a `limit` gate) parallel to `--max-memory`; child fuel on metal.
3. Disk-space and open-handle quotas on the fs provider.
4. Component/store signing and a provenance/trust model; treat provider provenance as first-class.
5. A composition "wiring view" so an auditor can see interposed layers, not just residual imports.
6. Subdirectory re-rooting as an algebra-level attenuation.
7. A hostile-component test suite in CI; fuzzing of the fs provider and the ABI boundary.
8. W^X for JIT pages on metal before any claim about bare-metal security.
9. An exportable, signable review artifact (composition + hashes + grants + TCB pins).

### Criticisms / rough edges

- README `only` example does not run as written; refusal text in the README is also more polished
  than the actual `RestrictError::…` output.
- `fs.memfs` composed onto an ordinary consumer panics (trap) instead of failing cleanly;
  resource-owning providers cannot be configured via the algebra yet.
- Raw internal error strings leak to the user (`SpawnError::Internal(… linker …)` for a child that
  needs exec; `RestrictError::…`; `FsError::…` inside outcome payloads).
- Mid-run memory-limit hits surface as opaque traps rather than a recognizable out-of-memory
  failure.
- The friendly "requires the eo9:fs capability — pass --fs-root" message exists for fs but not for
  exec (children get the raw linker error).

### What landed well

- Deny-by-default and the pre-execution gate are real and demonstrable; refusals name the exact
  offending imports.
- The fs containment behaved exactly as documented across all attempted escapes, including the
  encoded-path and symlink cases, with sensible in-tree-symlink semantics.
- Attenuation by composition (`fs.readonly`) and absence-by-composition (`fs.memfs` for fully
  sealed runs — when its configuration story applies) map well onto how a security reviewer thinks.
- exec as a capability, with children never inheriting it, demonstrated live.
- Store verification on every run; cache integrity check with clean fallback; read-only object
  files; per-task buffer/handle ceilings that exist and are enforced in code.
- The project's own gap tracking matched what the probing found (TOCTOU, configure limitation,
  fuel gaps were all already documented); nothing the participant uncovered contradicted the
  project's written claims about what is and is not done.

## Facilitator observations (gaps admitted / apologies made)

- Had to show a failing demo for the README's deterministic-environment one-liner (`fs.memfs`
  composed onto `readwrite` traps) and explain the configure limitation behind it.
- Had to admit the README/SPEC `only` shorthand does not match the implementation.
- Had to admit there is no CPU budget flag, no disk/handle quotas, no signing, no mid-run
  revocation, no fuzzing, no external review, and that side-channel mitigations are design-stage.
- Had to admit the TOCTOU window and the absence of `O_NOFOLLOW`/post-open re-checking, and that
  the demo's own sandbox location (/tmp) is the kind of place where that matters.
- Had to decline the malicious-by-construction component within the session and substitute
  targeted experiments, which the participant accepted but did not consider equivalent.
- The first store-tamper attempt was a facilitator error (the byte flip was silently blocked by
  the object file's read-only mode and the "tampered" run was actually a clean control); this was
  caught, disclosed, and redone correctly with chmod — recorded here for honesty.
- Two error surfaces shown during demos were raw internal debug strings; the facilitator
  acknowledged they undercut the otherwise strong "explains itself" impression the refusal
  messages make.
