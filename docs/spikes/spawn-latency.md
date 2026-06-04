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

(Numbers below are filled in per increment.)
