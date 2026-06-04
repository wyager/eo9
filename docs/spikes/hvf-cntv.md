# HVF: the CNTV switch and the ISV fix

Two kernel changes make `cargo xtask qemu aarch64 hvf` (Apple Hypervisor.framework,
added as an opt-in flag by the spawn-latency work) run the full Eo9 battery at
near-native speed. TCG stays the default and is byte-for-behavior unchanged.

## 1. The virtual generic timer (CNTV)

Apple HVF exposes only the EL1 *virtual* generic timer to guests; the kernel drove the
physical one (`cntp_*`) and died in the timer self-test (ESR EC=0x00, HVF's
unknown-sysreg signature). The switch — `cntpct→cntvct`, `cntp_*→cntv_*` in
`arch/aarch64/timer.rs` — is safe on TCG because QEMU `virt` boots with `CNTVOFF = 0`
(virtual == physical there), and the generic-timer PPI family 26/27/29/30 was already
configured, enabled, and serviced by `kirq`, so the move from PPI 30 to PPI 27 needed
no interrupt-path change at all. Under HVF the counter runs at the host's real
`CNTFRQ` (24 MHz on Apple Silicon vs TCG's 1 GHz); `resolution_ns` reports 41 ns and
all tick math is frequency-derived, so nothing else moves. The boot banner's
"counter advancing" check now spins until it observes a real advance — at 24 MHz two
back-to-back reads are routinely equal.

## 2. Syndrome-valid MMIO accessors (the ISV fix)

With the timer fixed, HVF booted to the prompt but **any PCI access aborted QEMU
itself**: `Assertion failed: (isv), function hvf_handle_exception, file hvf.c`.
Root cause, found by disassembly: LLVM compiled the ECAM `read_volatile::<u32>` into
`ldr s0, [x8]` (and the byte form into `ldr b0`) — **SIMD/FP loads**. `volatile`
constrains *that* an access happens, not *which register class* performs it. SIMD/FP
device accesses produce ISV=0 data-abort syndromes, which hardware virtualizers
cannot decode (QEMU's HVF backend asserts; KVM has the same restriction). TCG never
traps, so the latent bug was invisible for the project's whole life.

Fix: `pci::mmio` — inline-asm accessors (`ldrb/ldrh/ldr` / `strb/strh/str`, single
general-purpose register, no writeback → ISV=1) on aarch64, plain volatile elsewhere.
All four PCI primitives (`config_read`/`config_write`/`bar_read`/`bar_write`) route
through them, which covers every wasm driver and the in-kernel virtio-blk (their
device access all flows through these host functions; the virtio DMA rings are
ordinary RAM and never trap).

**Residual — CLOSED (`area/12-mmio-sweep`, plan/12 entry 75):** every remaining
aarch64 device-memory access (PL011 UART, GICv2/v3 distributor/redistributor/CPU
interface, PL031 RTC) now routes through the accessors, promoted to the crate-level
`src/mmio.rs`. The non-aarch64 volatiles stay (ISV syndrome decoding is an
aarch64-hypervisor concern; each site carries a one-line comment saying so), and the
RAM-classified volatiles (virtio DMA regions, the DTB blob, the PVH start_info) stay
plain volatile with their classification documented in place — the sweep's inventory
is provably complete because `grep read_volatile kernel/eo9-kernel/src/arch/aarch64`
now returns nothing.

## The numbers (HVF vs TCG, same kernel, same machine)

| measurement | TCG | HVF | speedup |
|---|---|---|---|
| on-target compile, 82 KiB hello composition | 2004 ms | 186–193 ms | ~10× |
| `gpu.virtio $ draw` cold (compile + bring-up + present) | 11.98 s | 1.27 s | 9.4× |
| `gpu.virtio $ draw` warm | 0.39 s | 0.06 s | 6.5× |
| demo session, boot → two programs → poweroff (kernel uptime) | 3.46 s | 0.24 s | 14× |

Full HVF battery: lspci, disk round-trip (INTx-served, zero polled fallbacks), net
ARP + real DNS, gpu draw cold/warm, the switch chain (`vnicheck --mode arp`, both
virtual MACs) — all PASS, with `cruncher`'s digest and the frozen-time hello output
byte-identical to TCG (guest determinism holds across accelerators; wall-clock
timing differs, guest-observed values do not).

TCG regression: the canonical demo, disk INTx session, check-gpu (pixel-exact),
svcdemo with timer-paced backoff restarts, 10/10 unpaced 53-char paste bursts, and
the riscv64/x86_64 demos are all unchanged.
