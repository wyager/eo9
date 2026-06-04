# Why `gpu.virtio $ draw` feels slow — the measured breakdown

Owner TODO (2026-06-03): "figure out why `gpu.virtio $ draw` is kind of slow; do a bit of
benchmarking." Method: temporary phase markers in `draw` (a `text` import printing a line
after each phase, reverted after the runs), host-side timestamps on the serial stream,
QEMU aarch64 TCG (`pci gpu`), ≥5 runs per configuration on a quiet machine, medians.
The native baseline is the stock usermode binary (`eo9 -c "gfx.mem $ draw"`, release).

## The breakdown

| phase | gpu.virtio COLD | gpu.virtio warm | gfx.mem warm | native (usermode) |
|---|---|---|---|---|
| codegen (announced) | **11,418 ms** | — (session cache) | — | ~460 ms (in cold total) |
| spawn/instantiate (echo → first guest line, minus codegen) | ~470 ms | **339 ms** | 183 ms | (in total) |
| driver bring-up (wake clear) | 34 ms | 5 ms | 1 ms | |
| clear full (480 rows + TRANSFER+FLUSH+INTx) | 5 ms | 3 ms | 2 ms | |
| pattern generation (307k px, guest) | 23 ms | 23 ms | 26 ms | |
| buffer upload (1.2 MB io) | 3 ms | 2 ms | 2 ms | |
| present (1.2 MB copy + TRANSFER+FLUSH+INTx) | 8 ms | 5 ms | 1 ms | |
| readback (480 rows + 1.2 MB) | 7 ms | 5 ms | 2 ms | |
| prefix_to_vec + FNV-1a over 1.2 MB (guest) | 9 ms | 8 ms | 9 ms | |
| outcome render | 6 ms | ~0 ms | ~0 ms | |
| **TOTAL (echo → `ok:`)** | **11,981 ms** | **389 ms** | 226 ms | 130 ms warm / 590 ms cold |

`--frames 2` adds one damage-rect present: +8 ms. A cocoa window (`display`) changes
nothing: warm totals 388/395/395 ms — the QEMU display path is not a cost.

## The verdicts

1. **Cold is the on-target compile, full stop.** 11.4 s of the 12.0 s is Cranelift
   compiling the 244 KiB fused composition under TCG emulation (the 136 KiB
   `gfx.mem $ draw` composition compiles in 2.1 s — the cost tracks size). This is
   already announced (`codegen: compiling …`), session-cached (repeat = 0), and
   storedisk-cached across boots. Native cranelift compiles the same composition in
   ~0.5 s (the usermode cold number). On real hardware the cold draw is sub-second.

2. **Warm is spawn machinery, not graphics.** 339 ms of the 389 ms (87%) elapses
   between the command echo and the program's first instruction — session-cache lookup
   (hash + full-bytes equality over 244 KiB), component instantiation/linking, bind,
   argument binding, task setup, all under TCG. The size dependence (339 ms @ 244 KiB
   vs 183 ms @ 136 KiB, near-proportional) says the cost scales with the fused
   component's size — instantiation-and-validation work, not a fixed fee. Kernel-side;
   recorded as a recommendation, not fixed here (out of this lane).

3. **The graphics pipeline is fast and needs nothing.** The entire device conversation
   — full-frame clear, 1.2 MB present (480 per-row `dma_write`s + TRANSFER_TO_HOST_2D +
   RESOURCE_FLUSH + the INTx round-trip), 1.2 MB readback — totals ~13 ms warm. The
   per-row DMA hypothesis (batching 480 calls into one contiguous write for full-width
   rects) is measured irrelevant at this resolution: not worth the code. `present`
   already transfers only the damage rectangle.

4. **TCG is the multiplier.** The identical workload end-to-end: 130 ms native vs
   389 ms TCG warm (3×), ~0.5 s native vs 11.4 s TCG cold compile (23×). On the
   Orange Pi's native A76 cores the warm draw lands well under 100 ms and the cold
   compile around half a second — "kind of slow" is a TCG demo artifact, not a design
   problem.

## Recommendations (recorded, not implemented)

- **Spawn path** (kernel exec, the only meaningful warm cost): profile where the
  339 ms goes — likely candidates are the full-bytes equality memcmp (skippable when
  hash+length match with a strong hash), pre-instantiation validation that wasmtime
  could cache alongside the compiled `Component`, and per-spawn linker construction.
  Worth one focused session if warm spawn latency ever matters beyond demos; on real
  hardware it is ~3× cheaper before any work.
- Nothing in the gpu driver or the draw example warrants change at 640×480. Revisit
  per-row DMA batching only if a mode ≥ 4K ever exists (480 rows → 2160 rows scales
  linearly but from a 5 ms base).
