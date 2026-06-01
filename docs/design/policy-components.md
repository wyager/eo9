# Policy components: functions as values in Eo9 APIs

Status: **DESIGN — for owner review.** Nothing here is implemented; WIT shown is PROPOSED.

Owner direction this responds to (2026-06-01): *"Might be cool to follow a general design
pattern of 'prefer functions over complex configs' for our APIs, so instead of having to
account for all the possible restart policies, we just let people provide a function like
`restart_policy : FailureHistory -> Action`."* Plus the follow-up question: *"Why can't
runtime-passed components be fused/inlined?"* — answered precisely in Part 1.

References: SPEC "Programs as values" (component-typed arguments, `interpret (…)`), SPEC
"Performance" (fusion/inlining), SPEC "The module store and compilation cache", wit/exec
(component-algebra / compile / task), docs/design/executor-model.md (the restart-policy
exemplar), plan/10 D6b (the parked component-typed-arguments item), study 09 (the
address-fragility finding that the pci predicate solves).

---

## Part 1 — When can a runtime-passed component be fused?

### What "fusion" is in this codebase

Fusion is a property of **compilation, not of passing**. When the algebra composes two
components (`compose(p, c)` — the `$` operator), the result is one new component whose
internal wiring connects `p`'s exports to `c`'s imports. When that *composition* is handed to
a compiler (`Component::new` in wasmtime terms; the `eo9:exec/compile.compile` op in WIT
terms), the compiler sees both sides at once: every cross-component call becomes a
statically-wired, compiled adapter — and a virtualization layer that does nothing compiles
down to (nearly) nothing. That is the SPEC's Performance story, and it is real today on all
three targets (on-target Cranelift on metal, host Cranelift in usermode, in-blob
Cranelift→Pulley in the browser).

A component that arrives **at runtime** (as bytes, through `load`) is not fused with anything
*yet* — fusion has not happened because no compiler has seen it next to a receiver. The
receiver, being already-compiled native code, cannot have new code spliced into it; artifacts
are immutable, content-addressed, and cacheable, and we do not patch native code at runtime.

So the precise answer to "why can't runtime-passed components be fused?" is: **they can — but
fusion is an act of compilation, so someone has to compile something.** The receiver has two
options, and both already exist in the codebase:

### The two binding times

| | **Instantiate directly** | **Fuse at receipt** |
|---|---|---|
| What happens | The policy component is compiled *alone* (small, one-time) and instantiated; the receiver calls its export through the executor's dynamic call machinery (host-mediated canonical ABI) | The receiver (or a consumer stub) is **re-composed with the policy and recompiled**: `compose(policy, consumer)` → `compile(...)` → one fused artifact |
| Setup cost | Milliseconds (a tiny pure component compiles fast); zero if the policy was precompiled and cached | One full compile of the composition: **~95 ms** on metal (measured), **~50–112 ms** in the browser blob, similar in usermode — but **~2 ms** when the content-addressed compile cache hits (storedisk / in-blob / usermode cache), which it always does for a repeated (receiver, policy) pair |
| Per-call cost | A host-mediated dynamic call: lift args → cross the host API → lower into the policy instance. Microsecond class. | A compiled adapter call inside one artifact: nanosecond class; a do-nothing policy can compile out entirely |
| Swappability | Swap the policy without touching the receiver; the pairing is dynamic | A new policy = a new composition = a new artifact (cached thereafter) |
| Identity / audit | The policy is a separate artifact; the (receiver, policy) pairing exists only in the registry's runtime state | The policy is **part of the program's content hash**: `describe`/wiring shows it, the compile cache keys on it, determinism covers it |
| Who can do it | Any executor/host (instantiating a no-import component requires no privilege) | Whoever holds `eo9:exec/compile` (the privilege boundary, by design) |
| Use when | Cold paths; policies that change at runtime; v1 simplicity | Hot paths (per-frame, per-fs-op); policies fixed at composition time |

### Verified in the codebase

* **Fuse-at-receipt is what every `$` at the eosh prompt already does.** Components are
  loaded from raw bytes *at runtime* (`component-algebra.load`), composed at runtime
  (`compose`/`restrict`/`configure`), and then compiled on the spot:
  `kernel/eo9-kernel/src/wasm/shellexec.rs:1656–1705` strips annotations
  (`executable_bytes()`) and runs `Component::new(&engine, &exec_bytes)` — full on-target
  Cranelift codegen of a composition that did not exist until the user typed it. Usermode
  (`crates/eo9/src/compile.rs:175`) and the browser blob do the same. The storedisk /
  usermode / in-blob caches make the second receipt of the same pairing ~2 ms.
* **Direct instantiation is what every executor already does with `main`.** The host
  instantiates components and calls their exports through the dynamic `Val` API (the same
  machinery the bind entrypoint uses). A pure policy component (no imports) instantiates
  against an empty linker — no privilege, no environment, nothing to wire.

### Gaps (what does *not* exist yet)

1. **Component-typed arguments cannot cross `spawn`/`detach` yet.** `named-arg` carries WAVE
   text only; a `component` parameter is "classified correctly but rejected at
   argument-encoding time" (plan/10 D6b — the parked `interpret (…)` item). Any API that
   takes a policy component as an *argument* (rather than via composition) needs this
   plumbing. Building it once for `eo9:svc/detach` un-parks D6b for free — they are the same
   feature.
2. **A provider cannot be `compile`d standalone** (compile requires a closed *binary*), so
   "fuse a policy" always means "recompile the *consumer* with the policy composed in" —
   which is the natural shape anyway (`policy $ consumer`).
3. **An unprivileged guest can do algebra on received bytes but cannot compile them** — it
   must hand the composition to something holding `compile` (the registry, the shell, a
   spawn). This is the privilege model working as intended, not a defect.

---

## Part 2 — Inventory: where the pattern applies

Legend: **binding** = how the policy reaches its receiver (compose-time fusion vs runtime
argument); **purity** = whether must-import-nothing enforcement matters; verdicts are
**strong win** / **marginal** / **reject**.

| # | Surface | Today | Policy-component version (PROPOSED WIT sketch) | Frequency | Binding | Purity | Verdict |
|---|---|---|---|---|---|---|---|
| 1 | **svc restart policy** (executor design §8C) | Doesn't exist; owner rejected enum configs | `decide: func(history: failure-history) -> action` | Cold (on service exit) | Runtime argument to `detach` | Essential (a policy must not phone home / read clocks) | **Strong win** — the approved exemplar |
| 2 | **pci.filtered** | `configure(allow: list<device-address>)` — compound config, baked (wit/pci/pci.wit:219) | `admit: func(device: device-info) -> bool` | Cold (at enumerate/open) | Either; compose-time natural (`my-filter $ pci.filtered $ driver`) | Essential (device info must not leak) | **Strong win** — also fixes study 09's "address-keyed allow-lists are fragile across boot configs": filter by vendor/device/class instead of bus address |
| 3 | **fs.filtered** (new attenuator) | Doesn't exist; only `--fs-root` granularity | `check: func(path: string, op: fs-op) -> verdict` where `verdict = allow \| deny \| read-only` | **Hot-ish** (every fs op; thousands/sec for io-heavy programs) | **Compose-time only** (`my-path-policy $ fs.filtered $ prog`) — fused, so per-op cost compiles away | Essential | **Strong win** — the most-requested attenuator shape (per-path rules), expressible today with zero new machinery: it is ordinary middleware |
| 4 | **net firewall, l4 form** (new middleware) | Doesn't exist | `admit: func(conn: connection-tuple) -> bool` (per connect/accept/bind) | Cold-ish (per connection) | Either | Essential | **Strong win** — `firewall $ net.l4.over-l2 $ prog` |
| 5 | **net firewall, l2 form** | Doesn't exist | `admit: func(frame-header: frame-info) -> verdict` | **Hot** (per frame) | **Compose-time only** — must fuse | Essential | Marginal for now (the l4 form covers the realistic need; per-frame policy is a DPI/router feature) |
| 6 | **only / allow-lists as predicates** | `restrict(c, allow: list<interface-ref>)` — static data | `admit: func(iface: interface-ref) -> bool` | Compose time | — | — | **Reject.** Allow-lists must be *legible as data*: `describe` shows them, audits read them, the compose-time error names what is missing. A predicate makes the capability surface opaque — you would have to *run* it to know what it admits. Data beats functions when the value is the documentation |
| 7 | **svc log filtering** | Proposed `log-policy` enum (capture/discard) | `keep: func(line: log-line) -> bool` | Medium (per output line) | Runtime | Nice-to-have | **Marginal** — the enum covers v1; revisit if log volume becomes a problem |
| 8 | **Compile-cache eviction** (storedisk) | LRU/MFU TODO in SPEC; nothing user-facing | `evict: func(state: cache-state) -> list<key>` | Cold | — | — | **Reject for now** — kernel-internal policy; running guest code inside cache management raises trust/reentrancy questions for negligible benefit. Revisit only if cache policy becomes user-visible |
| 9 | **time/entropy virtualization schedules** | `time.frozen --now-seconds N`, `time.fuzzy --granularity-ms N`, `entropy.seeded --seed N` | Already are policy components | — | Compose-time | — | **Already done** — the stub family *is* this pattern: a clock policy is a provider. The principle is precedent, not a change |
| 10 | **`interpret (…)` component-typed args** (plan/10 D6b, parked) | Rejected at argument-encoding (spawn takes WAVE text only) | `spawn`/`detach`/any API accepting `component`-typed parameters | — | Runtime argument | — | **Strong win by synergy** — the plumbing detach needs (#1) is exactly what un-parks this. One implementation, two features |
| 11 | **exec spawn-limits / child-admission** | `spawn-limits` record (max-memory) | `admit: func(info: component-info, limits: spawn-limits) -> result` | Cold (per spawn) | Runtime | Yes | **Defer** — this *is* the stage-B supervisor (executor design §4); not a separate API |
| 12 | **Schedulers** | SPEC: "a scheduler is just a program that holds task handles" | Already the pattern at the largest scale | — | — | — | **Already designed** — note as precedent |

The inventory surfaces a clean two-class split:

* **Class A — decision parameters** (restart policy, device filter, connection filter,
  admission): cold, runtime-swappable, passed as arguments. Need the component-argument
  plumbing (gap 1). Direct instantiation is fine.
* **Class B — data-plane attenuators** (path rules, frame rules): hot, fixed per
  composition, expressed as ordinary middleware (`policy $ attenuator $ prog`) and **fused at
  compose time with zero new machinery** — they work today; we just have not shipped the
  attenuator stubs.

## Part 3 — Recommendations

### Adopt now (rides with executor v1)

1. **The component-argument plumbing** (gap 1): `named-arg` (or a sibling) learns to carry a
   component value across `spawn`/`detach`. One implementation serves the restart policy,
   future Class-A policies, and un-parks `interpret (…)` (plan/10 D6b).
2. **`eo9:svc` restart policy as a component** — the exemplar, with standard policies shipped
   as stubs exactly like every other capability family:
   `restart.never`, `restart.always`, `restart.backoff --max 5 --base-ms 1000`.
   (Note the pleasing recursion: the *standard policies* are themselves configured via
   compound config; a *custom* policy is a 20-line component.)

### Adopt when the area is next touched

3. **`fs.filtered` + path-policy interface** (Class B middleware) — the highest-value new
   attenuator; works with existing machinery.
4. **`pci.filtered` predicate form** — add `admit(device-info) -> bool` alongside (not
   replacing) the address allow-list; deprecate nothing.
5. **`net.l4` firewall middleware** (connection predicate).

### Reject (and why the rejections are principled)

6. **Allow-lists as predicates** — capability surfaces must be auditable as data.
7. **Kernel-internal policies (cache eviction, GC)** — guest code inside TCB maintenance
   paths is the wrong trust direction.
8. **Log filtering** — config suffices; not every parameter deserves to be a function.

### The constraint set every policy component must satisfy

| Constraint | Meaning | How it is enforced |
|---|---|---|
| **Pure** | Imports nothing — no clock, no entropy, no I/O, no exec | The accepting API checks `describe(policy).imports == []` (equivalently: `only [] $ policy` composes). A typed refusal (`policy-not-pure(list<string>)` naming the imports) otherwise |
| **Provider-shaped** | Exports exactly the policy interface; no `main` | `describe(policy).kind == provider` + the export check the algebra already does |
| **Deterministic** | Same input → same verdict | Follows from purity (nothing nondeterministic is importable). Stated, not separately enforced |
| **Bounded** | Terminates within a fuel budget per call | The existing fuel machinery: the receiver donates a fixed fuel allowance per policy call; out-of-fuel = policy failure (typed), never a hang |

### PROPOSED SPEC wording (for the "Eo9 API design" section)

> **Policies are programs.** Where an Eo9 API takes a non-trivial decision parameter — a
> restart policy, a device filter, a path-access rule — it takes a *component*, not a
> configuration enum: a tiny provider exporting the policy interface (typically one function,
> `decide: func(input) -> verdict`), required to import nothing. Purity is therefore enforced
> by the capability system rather than promised by documentation: a policy provably cannot
> read the clock, draw entropy, or touch I/O, and a fuel bound per call makes it provably
> terminating from the caller's point of view. Policies bind at either of the two times
> everything else in Eo9 binds: **composed in at compose time** (ordinary middleware — fused,
> so the per-call cost compiles away; this is how hot-path attenuators work) or **passed as a
> value at call time** (instantiated by the receiver — for cold-path decisions and policies
> that change at runtime). Configuration data remains for what must be legible at a glance:
> capability allow-lists, resource limits, addresses — anything `describe` should show
> without running code.

---

*Pointer: plan/02-wit.md references this doc; the executor design (docs/design/executor-model.md
§8C) adopts recommendation 2 as its restart-policy answer.*
