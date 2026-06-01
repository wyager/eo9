# Spectre bounds-check mitigation audit

**Date:** 2026-06-01 · **Auditor:** area-12 verification pass · **Verdict: the SPEC's "masking stays
enabled" sentence is true for usermode, structurally impossible for bare metal under wasmtime 45, and
not-applicable for the browser.**

## What was audited

Whether Cranelift's speculative bounds-check mitigations (`enable_heap_access_spectre_mitigation`,
`enable_table_access_spectre_mitigation`) are enabled in every engine configuration Eo9 ships, per the
SPEC Security section's claim that "the compiler's speculative bounds-check masking stays enabled (this
matters most on no-MMU targets, where bounds checks are explicit branches rather than guard pages)".

## The Cranelift defaults

Both settings default to **true** — `kernel/vendor/cranelift-codegen/src/settings.rs:513-514` (the
generated-defaults test pins `enable_heap_access_spectre_mitigation = true` and
`enable_table_access_spectre_mitigation = true`).

## The wasmtime 45 constraint that decides everything

`kernel/vendor/wasmtime/src/config.rs:2871-2891`: when `signals_based_traps(false)` is set, wasmtime
**forces both Spectre settings to `false`** (`ensure_setting_unset_or_given(…, "false")`,
`config.rs:240-249`: unset → forced off; explicitly set to true → hard error
*"when signals-based traps are disabled then spectre mitigations must also be disabled"*).

The reason (documented at `config.rs:3067-3071`): Cranelift's mitigation replaces the speculatively
bypassable bounds-check branch with a conditional select that redirects out-of-bounds accesses to the
**null address**, relying on a fault at null to produce the trap. Without signals-based traps there is
no fault-on-null machinery — and on our bare-metal identity map, address 0 is mapped RAM — so the
load-from-null formulation is unsound there. An explicit-bounds-check-compatible formulation
(mask the index with a conditional move *and* keep the branch) is "future work" upstream.

## Per-configuration verdict

| # | Engine config | `signals_based_traps` | Spectre mitigations | Evidence |
|---|---|---|---|---|
| 1 | **Kernel**, deserialize + on-target codegen, all three architectures | `false` | **OFF — forced by wasmtime, cannot be enabled** | `kernel/eo9-kernel/src/wasm/mod.rs:126`; codegen.rs uses the same engine ("config … is identical", codegen.rs:44) |
| 2 | **Usermode** (`crates/eo9-runtime`) | unset → default `true` | **ON** (Cranelift defaults; additionally, default 64-bit-host guard regions are sized so even speculative 32-bit-index accesses land in unmapped reservation — guard pages are themselves the v1 mitigation for heap accesses) | `crates/eo9-runtime/src/engine.rs:33-60` (no `signals_based_traps` call anywhere in `crates/`) |
| 3 | **xtask `precompile_for_kernel`** (host-AOT for metal) | `false` | **OFF — forced** (must match engine #1's flags or the artifacts would not load) | `xtask/src/main.rs:1955` |
| 4 | **xtask `preaot_for_web`** (host-AOT to pulley32) | `false` | **OFF — forced** (and see #5: not meaningful for Pulley) | `xtask/src/main.rs:1338` |
| 5 | **Browser blob** (pulley32) | `false` | **N/A** — Pulley is an interpreter; Cranelift's mitigation governs *native* speculative execution of generated code, but here the "CPU" is the browser-JIT-compiled interpreter loop. The classical wasm-controlled v1 gadget does not map onto it; the browser's own process/site isolation is the relevant layer. | `www/web-eo9/blob/src/lib.rs:130-135` |
| 6 | **www server** | — | **N/A** — no wasmtime engine remains (the `/vm/compile` endpoint was removed) | `www/src/` has no wasmtime references |

## What this means

1. **Usermode is fine.** Mitigations on, plus guard-region sizing makes heap accesses
   speculation-safe by construction. No change needed, none made.

2. **Bare metal has no compiler-level Spectre v1 mitigation, and cannot have one under wasmtime 45.**
   This is not a configuration mistake — it is an upstream limitation tied to exactly the mode the
   kernel must use. The load-bearing Spectre defense on metal is therefore the one the SPEC lists
   *first*: **fine-grained time is a capability**. A program composed with `time.fuzzy`, `time.frozen`,
   `time.none`, or no timer cannot measure cache state, so it cannot exfiltrate what speculation
   leaks. Programs granted real `eo9:time` on metal should be treated as trusted with respect to
   side channels until the upstream gap closes.

3. **The SPEC sentence needs one qualifier.** "The compiler's speculative bounds-check masking stays
   enabled" should read that it stays enabled **where the execution mode supports it (usermode guard
   pages)**, and that on explicit-bounds-check targets the masking is **not currently available
   upstream** — there, the time-capability defense is not defense-in-depth but the primary and only
   mitigation. (SPEC is planner-owned; not edited here.)

4. **The fix, when wanted, is upstream/vendored work, not configuration:** implement Cranelift's
   explicit-bounds-check Spectre formulation (cmov-mask the index before use *and* keep the trap
   branch — no fault-on-null dependence). This is a candidate for the same vendored-fork +
   upstream-PR path used for the no_std work. Until then, nothing to flip.

## The no-MMU note

`signals_based_traps(false)` + `memory_reservation(0)` + `memory_guard_size(0)` — the kernel's exact
configuration — **is already the no-MMU configuration** from wasmtime's point of view: no signal
handlers, no virtual-memory reservations, no guard pages; every linear-memory access carries an
explicit bounds check. The kernel runs in this mode today even on MMU-equipped hardware (the MMU is
used only for W^X on generated code, never for memory safety). So a hypothetical MMU-less port changes
**nothing** about wasm memory safety or this audit's conclusions — which is also the concrete
verification of the SPEC's "secure even on hardware with no MMU" claim: the safety argument on metal
is purely the explicit bounds checks the compiler emits, on every target, today.
