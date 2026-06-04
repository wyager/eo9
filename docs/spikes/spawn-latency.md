# The warm spawn path — measured, then made cheap

Owner-commissioned (2026-06-04): the draw-latency spike attributed 339 ms of warm
`gpu.virtio $ draw` to "spawn/instantiate machinery" and the owner expects sub-ms
process spawn natively. This spike instruments the path phase-by-phase (the
`spawn-trace` kernel feature: per-phase micros accumulated across the algebra/compile/
spawn host calls, one summary line per spawn; enable with
`EO9_KERNEL_FEATURES_EXTRA=spawn-trace`), then removes the redundant work.

## Baseline (QEMU aarch64 TCG, medians over ≥7 warm runs, session-cache warm)

| phase (kernel-side) | `gpu.virtio $ draw` (242 KiB fused) | `time.frozen … $ hello` (97 KiB fused) |
|---|---|---|
| alg-load (store match, 2 loads) | 0.9 ms | 0.3 ms |
| **alg-op (`compose` re-fusing)** | **125 ms** | **46 ms** |
| **exec-bytes (re-extraction)** | **66 ms** | **21 ms** |
| hash-lookup (FNV + 242 KiB memcmp) | 1.4 ms | 0.5 ms |
| linker population (~76 host fns) | 0.6 ms | 0.5 ms |
| state + session manifest | 0.05 ms | 0.05 ms |
| store creation | 0.02 ms | 0.02 ms |
| **instantiate (wasmtime)** | **0.9 ms** | **0.9 ms** |
| bind entrypoint | 0.01 ms | 0.25 ms |
| args + task registration | 0.03 ms | 0.03 ms |
| kernel total | ~195 ms | ~70 ms |
| **external total (echo → ok)** | **377 ms** | **151 ms** |
| guest-side residual (eosh parse/resolve/byte-passing) | ~182 ms | ~81 ms |

## The verdict on the draw-bench attribution

"Spawn/instantiate machinery" was the right region, wrong suspects. **Instantiation is
0.9 ms and the linker 0.6 ms — wasmtime is not the cost.** The warm cost is *redundant
algebra work*: `compose` re-fuses the identical composition every run (125 ms), the
`implements`-stripping `executable_bytes` re-parses the fused bytes every run (66 ms),
and eosh re-reads and re-passes every component's bytes through the canonical ABI
(~180 ms guest-side). All three are cacheable; the first two kernel-side.

## The fix: the fusion-graph cache (owner design)

The cache key is the **fusion graph**, not the prompt spelling: every component handle
carries a blake3 *graph hash* — a leaf (loaded component) hashes its bytes (store
entries: computed once per boot); an interior node hashes (op tag ‖ ordered child
hashes ‖ canonicalized args) with domain separation. The root hash is the composition's
semantic identity: independent of spelling, whitespace, and binding names. Three caches
key on it, all session-scoped, all LRU-8:

* **fused bytes** — a repeated `$`/`&`/`only`/`rename`/`configure` skips the encoder
  entirely (the 125 ms);
* **compiled artifacts** — `compile` returns the cached `Component` before exec-bytes
  extraction, deserialization, or codegen run (the 66 ms + the pristine-entry
  deserialize);
* the **spawn `Linker`** is built once per boot (the host-function set is
  boot-constant; the one conditional registration — `eo9:pci` — is guarded by a stored
  grant bit re-checked on every reuse, so a linker built under one grant shape can
  never serve another; per-spawn capability state lives in the per-spawn `Store`).

Skipping the encoder on a hit is sound because the encoder is deterministic
(`eo9-component` is BTreeMap-ordered with no randomness) and the hash is strong;
the `graph-verify` kernel feature re-runs the op on every hit and asserts byte
equality — verified live ("fusion hit re-encoded identically") and available to any
battery. Invalidation is structural: a changed `let` changes exactly the subtree
hashes that contain it. Subtree-level partial-encode sharing is a recorded follow-up.

**InstancePre was evaluated and deliberately skipped**: instantiation measures 0.9 ms
(0.4 ms small) — pre-resolved imports would shave a fraction of a millisecond at the
cost of a second keyed cache with grant-shape composite keys (the security review's
(hash × linker-identity) requirement). The design is recorded here for the day
instantiation grows; the grant-shape guard already exists on the shared linker.

## The compiler fingerprint (owner requirement)

Every *persistent* compiled-artifact cache key now includes a **compiler fingerprint**:
build.rs hashes kernel/vendor/** (the patched wasmtime + cranelift sources) plus the
engine-config sources (wasm/mod.rs, wasm/codegen.rs) into a blake3 emitted as
`EO9_COMPILER_FINGERPRINT`, and the storedisk cache key is
`blake3("eo9-compile-cache-v2" ‖ fingerprint ‖ executable-bytes)`. A vendored compiler
change — including a miscompile fix — therefore makes every old entry an unreachable
**clean miss** (owner ruling: the fingerprint lives in the lookup key; no
staleness-verification-failure path exists; the keyed MAC stays reserved for genuine
integrity failures). This closes the gap the make-warm review proved: wasmtime's own
compatibility check accepts stale-but-compatible artifacts, so content changes
previously needed a human `PRECOMPILE_CONFIG_REV` bump. xtask's host-side precompile
stamps share that gap and still rely on the REV — recorded as the follow-up to fold the
same fingerprint into the make-warm stamp machinery.

**Hidden-dependency audit**: codegen target features are build-time fixed — the engine
sets an explicit target triple and explicit cranelift flags (x86's SSE3..4.2 set
mirrors the precompile side); the only runtime CPU probe is x86's load-time CPUID
*verification*, which refuses rather than varies codegen. No runtime-detected feature
set exists to join the key; hashing mod.rs pins the flag choices themselves. On real
hardware, artifacts remain portable exactly within (target triple × flag set ×
fingerprint).

## Eviction audit (owner requirement)

| cache | scope | policy (confirmed in source) | verdict |
|---|---|---|---|
| session fused-bytes + compiled (kernel) | one boot | LRU, 8 entries each (refresh-to-back on hit, evict-oldest) | bounded, fine |
| storedisk compile cache (kernel) | persistent | **unbounded** — entries are MAC'd files written by key, never deleted (only `/bin` saves have removal) | needs a budget |
| usermode `~/.eo9` compile cache | persistent | `Store::gc` enforces a size budget, evicting by ascending (use-count, last-used) — LFU then LRU; meta tracks last-used/use-count per entry | bounded, good |

The storedisk recommendation (not implemented in this lane — it needs eofs metadata
plumbing): mirror the usermode policy — a per-entry last-used record and a size-budget
sweep at mount or after each store, ~32 MiB default on the 64 MiB scratch disk
(artifacts run 0.5–1.5 MiB, so ~50–100 compiles fill the disk today). Old-fingerprint
entries are exactly the garbage this sweep reclaims: they stop being hit the moment the
kernel upgrades, their last-used ages out, and the budget removes them — no special
upgrade handling needed. The usermode cache key already includes a `compiler_version`
string, which is adequate for registry wasmtime (content changes only with version
bumps) but is the same gap for anyone vendoring — noted in the doc comment trail.

## After (same benches, same machine, TCG)

| | before | after | kernel-side after |
|---|---|---|---|
| `gpu.virtio $ draw` warm | 377 ms | **121 ms** | **2.1 ms** (was 195) |
| `time.frozen … $ hello` warm | 151 ms | **28 ms** | **0.6 ms** (was 70) |
| bare `hello` warm | — | **2 ms** | **0.6 ms** |
| cold (any) | unchanged | unchanged | codegen dominates |

Per-phase after (gpu warm): alg-load 0.9 ms (the store-entry match on eosh's
re-passed bytes), alg-op 0.09 ms (fusion hit), exec-bytes 0 (skipped), lookup 4 µs,
linker 1–2 µs (reused), instantiate 0.93 ms, bind 10 µs, args 27 µs.

**The owner's sub-ms target, honestly:** kernel-side warm spawn is **0.6 ms** for a
small program *under TCG* — on native silicon (~3× per the draw-latency spike's
TCG-vs-native ratio) that is roughly 0.2 ms, with instantiation (0.4 ms TCG) the floor.
Sub-ms native spawn is real today. The remaining *external* warm latency (119 ms of
the 121 ms gpu line) is guest-side: eosh re-parses the line, re-reads every component
from `/bin` through fs host calls, and re-passes the bytes through the canonical ABI
(~470 KiB of copies for the gpu line) before the kernel sees the first algebra call.
That is the eosh lane's problem and the next lever: a session-side resolve cache in
eosh (name → component handle) would skip the re-read entirely; recorded as the
follow-up, out of this lane.

## HVF (the separate milestone)

`cargo xtask qemu aarch64 hvf` (opt-in; TCG stays the default) boots under Apple's
Hypervisor.framework through MMU/heap bring-up, then dies in the generic-timer
self-test: ESR EC=0x00 (HVF's unknown-sysreg signature) at
`arch::aarch64::timer::self_test` — **Apple HVF exposes only the virtual generic timer
to guests, and the kernel drives the EL1 physical timer (`cntp_*`)**. The CNTV switch
(cntpct→cntvct, cntp_*→cntv_*, timer PPI 30→27) is the recorded follow-up that would
unlock native-speed aarch64 sessions (and with them, native-speed on-target codegen —
the 11.4 s cold compile would drop toward the ~0.5 s native figure). The flag and the
finding stay; nothing else changes until that switch is built and verified.


