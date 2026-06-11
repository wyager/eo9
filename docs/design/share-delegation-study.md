# Share Delegation Without the Macro — a First-Principles Study

**Status:** design study (area/45). No implementation. Companion to `docs/design/shared-resources.md` (the settled model) and the area/41 M1 gate implementation (branch `area/41-gate-m1`, not yet merged).
**The prompt:** the area/41 serving tail (`l4.factory`) carries ~15 one-line delegation wrappers; the proposed fix was a `delegate_l4!` macro. The owner: *"Having a macro feels like a code smell... think, from a first-principles perspective, if there is a more elegant way to achieve our end goal."*

---

## 1. The end goal, restated cleanly

An owner provides an implementation of a WIT interface. A grantee in **another task** uses it through its **ordinary import** (the uniformity invariant — the grantee cannot tell). The owner **may** interpose policy, but the no-policy case should cost ~zero ceremony. Capability discipline holds: factory-minted handles are the only access path, and the composition seal is either respected or its relaxation is **principled**, never expedient. The kernel mediates only because wasmtime stores are isolation units.

**Why the wrappers exist at all** (the mechanical fact every direction below must answer): a resource handle is just an index. When a grantee calls `recv(handle)`, the kernel — standing outside the component — needs an **exported function** to enter through. Inside `net.virtio $ net.l4.over-l2 $ l4.factory`, over-l2's own `recv` is sealed by `$`; only the tail's exports are visible. The tail therefore re-exports the whole contract. The stub says so itself (`guest/stubs/net-l4-factory/src/lib.rs:12-17`, area/41):

> Why the full `l4` re-export exists: the kernel call gate executes a child's gated import by calling *exported* functions of the owner's instance — non-exported (fused-internal) functions are unreachable from the embedder, so the serving composition's terminal component must surface the whole transport contract. Every method here is one delegation line.

And the gate consumes exactly that surface, by name lookup (`gate.rs:896-915`, `lookup_owner_funcs`): `exported_func(instance, store, L4_INTERFACE, "recv")` etc. — the gate has **no idea which inner component an export aliases to**. That fact is load-bearing for the recommendation.

---

## 2. Direction A — the macro (baseline): why exactly it smells

The 245-line `net-l4-factory` stub is ~180 lines of pure transcription: four wrapper resource types, 13 delegating methods, and type-mapping helpers (`to_underlying`/`to_export`/`map_error` — `lib.rs:47-96`) that exist only because the export side and import side of *the same interface* are distinct nominal types to wit-bindgen. A `delegate_l4!` macro compresses the typing but not the smell:

1. **It is textual, not structural.** The WIT already states the contract machine-readably; the macro restates it in Rust token trees. Two sources of truth.
2. **Drift is real, not hypothetical.** It already happened in area/41: when `dns-servers` joined the interface (area/42), every tail built before the growth silently lacks the export. The gate had to grow a workaround — `gate.rs:867-870`:
   ```rust
   /// `dns-servers` (area/42-curl-ux). Optional during the transition: an owner
   /// composition built before the interface grew it simply lacks the export, and
   /// gated calls answer the typed io error instead of refusing the whole share.
   dns_servers: Option<Func>,
   ```
   A macro does not fix this — the macro's expansion is frozen into each built component; the `Option<Func>` class of patch recurs on every interface growth.
3. **Per-interface arity.** `delegate_l4!` is l4-shaped. Sharing fs next means `delegate_fs!`; the macro count scales with the interface count, each one hand-maintained against its WIT.
4. **N copies in N owners.** Every policy owner that delegates *most* methods and intercepts a few re-instantiates the expansion.

The macro treats the symptom (typing) and preserves the disease (a guest component whose entire content is a restatement of a WIT file).

---

## 3. Direction B — fuse-time export lifting (the recommendation)

First-principles observation: the delegation tail exists to make sealed exports visible again. But the composition algebra **already has an operator whose defining property is "keep the base's exports visible"** — and it is not `$`. SPEC.md, *Environments and the `&` operator*:

> - **Wiring.** Every import of `y` matched by an export of `x` is satisfied by `x` (and sealed, exactly as with `$`).
> - **Exports.** `exports(x & y) = exports(y) ∪ (exports(x) ∖ exports(y))` — the right-biased union.

So take a factory component that is *only* a factory — world `l4-factory-lift { import l4; export l4-factory; }`, whose entire body is:

```rust
fn get() -> Result<L4Impl, L4Error> {
    Ok(underlying::default())   // mint a root handle onto the imported stack
}
```

(the `l4-factory` WIT interface from area/41 is unchanged — `get: func() -> result<l4-impl, l4-error>`; its `use l4.{l4-impl}` now resolves to the **imported** l4, the wasi-http `incoming-handler`-uses-imported-`types` precedent) — and build the serving composition with `&` instead of a `$`-tail:

```
net.virtio $ net.l4.over-l2  &  l4.factory
```

By the export law: result exports = `{l4-factory} ∪ {l4, …}` — the factory **and** over-l2's own l4 surface, aliased outward by the algebra, zero delegation code. The implementation already does this: `compose.rs:215-229` (`extend`) wires the layer's imports from the base, `export_all`s the layer, then `export_all`s the base minus shadowed slots. And because `gate.rs`'s `lookup_owner_funcs` finds exports by name, **the gate code does not change at all** — a lifted alias and a hand-written wrapper are indistinguishable to it.

What each criterion says:

* **No-policy ceremony: ~zero.** One ~20-line get-only component, written once per shareable API, containing no method wrappers, immune to interface growth (nothing to drift — the lifted surface *is* the provider's real surface; the `dns_servers: Option<Func>` class of tail-staleness dissolves: a stale link can no longer be the tail, only an honestly-old provider).
* **Policy ceremony: a real component, as doctrine demands.** `net.virtio $ net.l4.over-l2 $ net.l4.filtered & l4.factory` — the policy component re-exports l4 with every endpoint op gated (`net-l4-filtered/src/lib.rs`, in-tree today), `&` lifts *its* exports, the factory mints handles onto *it*. Policy lines are not boilerplate; the 15-method shape is written exactly where each method carries a decision. Per-grantee scoping (§4 wrapper handlers, spawn grants) is untouched.
* **Capability discipline: clean, and *more* honest than the tail.** Nothing is exported by accident: `$` still seals; sharing is precisely the visible, deliberate act of writing `&` at the branch point. **"`$` seals; `&` shares"** is a one-sentence doctrine extension, and it is already the operators' documented semantics. The exported l4 is not ambient authority — no guest can name another task's exports; only the kernel calls them, and only through a grant. The seal is not violated; the composition simply *chose not to seal* its serving surface, using the algebra's own operator for that choice.
* **`describe`/kind: truthful.** Result is a provider (extend: provider × provider → provider). `describe` reports `exports: eo9:net/l4, eo9:net/l4-factory` + the R7 `serves: eo9:net/l4` line — and now the export list *literally shows* the surface the gate enters through, instead of a wrapper pretending to be it.
* **Performance: strictly better.** The area/41 tail interposes one guest method body (wrapper resource indirection + type re-mapping) on **every** gated call. Lifted calls land directly on over-l2's export: two boundary crossings, zero interposed guest code in the no-policy case.
* **Scales to the next interface.** Per shareable API: one blessed `*-factory` WIT interface (exists, §5.2) + one get-only component. No per-method anything, anywhere.
* **Upstream delta: zero.** No vendored-wasmtime change, no wac change (one spike to confirm, §9).

**The automatic variant — rejected.** Could `$` auto-lift the provider's l4 export whenever the consumer-side tail exports a factory whose resource type comes from an interface the tail imports? Mechanically yes (the metadata is present at fuse time). But `exports(p $ c) = exports(c)` is the sealing law itself; making it conditional on a property of `c`'s export types turns the algebra's simplest invariant into a special case, and makes the share *invisible* in the expression. The explicit `&` is one character of ceremony and is the audit trail. Defaults are capability policy; the conservative default is the sealed one.

**Honest limitations.**
1. **Binary-that-serves is not expressible with `&`** (`compose.rs:187` — "binaries do not participate in `&`"). A telnetd that both runs `main` and exports a factory keeps the area/41 options: author its own factory export, or (the M2 shape, which needs no factory at all) spawn grants with §4 session wrappers. Relaxing `&` to admit a binary as the final layer is a recorded follow-up question, not needed by any current milestone.
2. **Adding policy to a deployed no-policy share changes the composition's shape** (insert a filtered-style component) rather than editing one function body. See §8, the argument against.
3. One structural assumption to spike before committing M-work: wac-graph resource-type unification when the same base export is both an instantiation argument (the factory's import) and an aliased outer export — the factory's `get` return type and the lifted methods' self type must unify into one resource type in the encoded component. `compose.rs::extend` already produces exactly this shape for config riders, and the CM type system says yes; a 30-minute `check-share`-derived unit settles it.

---

## 4. Direction C — synthesized adapter component

Move the macro out of guest source into infrastructure: xtask (bundle time) or the kernel (detach time) generates the 15-wrapper delegation component mechanically from the WIT — the wasi-virt precedent.

Assessment: this is the macro with better hygiene. It fixes drift (regenerated against current WIT at build time), fixes N-copies (one generator), fixes per-interface arity (generator walks any interface). But the generator is real machinery: synthesizing canonical-ABI delegation requires emitting a core module (handle-table plumbing, lower/lift glue per method) plus the component wrapper — wasi-virt is a substantial codebase doing exactly this. Vendored `wasm-tools` 0.250 (`wasm-encoder`) can express the output; *we* would own the generator. Detach-time synthesis adds kernel-resident codegen (a new trusted-computing-base item, against the no_std budget) for zero benefit over build-time; build-time synthesis adds a bundle step + hashing/caching of generated tails. And the generated component still pays the interposed guest hop per call that B eliminates.

**Verdict: dominated by B.** C produces, at the cost of a generator we maintain, exactly the artifact that `&`-lifting makes unnecessary. C would only win if delegation through a *distinct* resource type were itself required — no requirement says so. Reject; note as the fallback if the §3 spike fails.

---

## 5. Direction D — handle-as-capability primitive (vendored runtime change)

The philosophical question: if a factory deliberately returns `own<l4-impl>`, hasn't the component granted the holder method access? Then a vendored entry point — "invoke a method on a held `ResourceAny` regardless of export visibility" — makes `get()` alone sufficient: no re-exports, no lifting, no wrappers.

**The case for (the ocap reading).** In capability theory the handle *is* the capability: designation and authority are one act. E-style ocap systems have no separate "interface possession"; possessing an object reference means being able to invoke it. The component model's "methods are interface functions you must separately import/export" is, on this reading, an artifact of the canonical ABI's compilation model, not a security judgment — and the factory's `own<>` return is the deliberate grant. Eo9 already half-agrees: the gate's whole design says possession of the handler (spawner passed it / factory minted it) is what authorizes the grantee.

**The case against (the seal reading) — which wins, on three grounds.**
1. *The primitive is not a door-opening; it is door-synthesis.* A method that was never `canon lift`ed does not exist as a callable component-level function — lifting is where memory/realloc/string-encoding options are fixed. Inside a fused composition the inner instance's lifted exports do exist (instance linking), but they are not part of the **outer component's type**. The vendored change is therefore not "relax a visibility check": it is "give the embedder a navigation API into sealed inner instances." That breaks the judgment every Eo9 tool relies on — `only`, `restrict`, kind validation, and `describe` all reason over the outer component type. With the primitive, `describe` lies: authority is reachable that no export shows. Appendix A already executed this exact suspect once: "Hosting exports / the `eo9-host:` rider prefix — the kernel reaching past `$`-sealing into a fused component's internals."
2. *Eo9's own algebra separates designation from operation.* SPEC's capability shape is "possession of the root handle **plus** a linked import" — `default()` hands you the handle, the import gives you the verbs. The handle answers *which instance*; the interface linkage answers *what you may ask of it*. D collapses an intentional two-axis system.
3. *Upstream-delta reality.* `kernel/vendor/README.md`: "Nothing here is a fork we intend to keep: every change is the minimal, upstream-shaped relaxation needed for the bare-metal target." D is not upstream-shaped — upstream's canonical-ABI docs treat resource methods as ordinary interface functions with no ambient dispatch, and no design note proposes `ResourceAny` dynamic dispatch. The vendored delta is already widening (first-poll, mmio, the compile-callback patch); adding a feature upstream would refuse is the moment the vendor directory becomes a fork.

**Verdict: reject — the seal is the capability boundary; the export requirement is mechanism, but principled mechanism.** Record one trigger for revisit: if upstream ever ships first-class dynamic method dispatch on `ResourceAny`, the cost side of this argument collapses and only ground 1/2 remain to argue.

---

## 6. Direction E — WIT restructure (methods ride the factory)

Declare the resource and its methods inside `l4-factory` itself (or a shared types-only interface), so exporting the factory necessarily exports the methods.

This fails on WIT's own semantics before reaching ergonomics: **functions belong to the interface that declares them.** Exporting `l4-factory` exports `get` — a `use l4.{l4-impl}` brings the *type* along, never `l4`'s functions. To make "factory implies methods" true, the methods themselves must be re-declared inside `l4-factory` — a full second copy of the 13-function contract, i.e. the boilerplate moved into WIT and made permanent API surface. Then: type identity with `eo9:net/l4` breaks (a `tcp-connection` from the factory flavor and one from the ordinary import are distinct nominal types — every current importer of `eo9:net/l4` is on the wrong side: `net.text`, eosh, every l4 stub); the area/41 erasure note (shared-resources.md §3.1: "WIT cannot express 'any resource'… the handler's nominal type belongs to the spawner's own import, which exec cannot name") gets *worse*, since there are now two nominal homes per API; and the N-interfaces-to-share-N-things ceremony lands on every future API. **Reject.**

---

## 7. Direction F — type erasure and other generated alternatives

* **Single generic entry: `call(method: enum, args: …) -> …` on the factory.** Reject precisely: it re-introduces serialization/framing where WIT is already the contract — the literal sentence that killed channels (Appendix A). Args/results become bytes or a giant variant; the canonical ABI stops typing the boundary; per-method validation, `describe` truthfulness, and the typed error vocabulary all degrade to a protocol the kernel must parse. It is the channel smell relocated into a function signature.
* **Per-method blessing at detach validation.** Orthogonal, not an alternative: R7's structural validation already checks the serving surface at detach; whatever scheme produces the exports, validation stays. It removes zero wrappers.

---

## 8. Comparison

| | A: macro | B: `&`-lift | C: synthesized adapter | D: ResourceAny primitive | E: WIT restructure | F: generic call |
|---|---|---|---|---|---|---|
| No-policy owner ceremony | macro invocation + stub crate per API | one `&` in the composition; ~20-line get-only component per API | zero lines (toolchain) | zero lines | full WIT re-declaration per API | enum+codec per API |
| Policy owner ceremony | real component (macro for pass-through arms) | real component (filtered shape, in-tree) | real component | real component | real component ×2 types | policy inside codec — worst |
| Capability cleanliness | clean | clean; share = visible `&`; seal law untouched | clean | **breaks the seal judgment** | clean but two nominal types | erases types |
| Upstream/vendor delta | none | none (1 wac spike) | none (we own a generator) | **real vendored feature, not upstream-shaped** | none | none |
| `describe` / kind | wrapper poses as surface | truthful: lifted exports + `serves:` | truthful-ish (generated tail) | **describe lies** | confusing double surface | one opaque function |
| Calls per gated op | 2 crossings + wrapper body | **2 crossings, no guest interposition** | 2 + wrapper body | 2 crossings | 2 + mapping | 2 + codec |
| Next interface (fs, disk) | new macro each | factory interface + get-only stub each (mechanical, tiny) | generator handles it | free | full re-declaration each | new enum each |
| Drift on interface growth | proven (`dns_servers: Option<Func>`) | **dissolves** (lifted surface = real surface) | regenerated away | n/a | double maintenance | enum drift, untyped |
| Malicious/buggy owner can… | lie in wrappers (callee-side; policy anyway) | nothing new — same exports, fewer bodies to be wrong in | trust the generator | reach into *any* component given a handle | same as A | smuggle anything through bytes |

---

## 9. Recommendation and migration from the area/41 v1 shape

**Recommend B: fuse-time export lifting via the existing `&` operator, with a get-only factory layer.** The doctrine line is one sentence: *`$` seals; `&` shares — making a composition shareable is choosing `&` at the branch point, which keeps the serving surface exported for the gate to enter, exactly per the operator's existing export law.* The delegation boilerplate then exists in the repo only where each line carries a policy decision (`net.l4.filtered`), which is not boilerplate. No macro, no generator, no vendored-runtime change, no WIT change beyond what area/41 already added (the `l4-factory` interface is kept verbatim; only the `l4-factory-tail` world is replaced).

Migration (cheap, pre-merge — area/41 is not on master yet):
1. Replace world `l4-factory-tail { import l4; export l4; export l4-factory; }` with `l4-factory-lift { import l4; export l4-factory; }` in `wit/net/net.wit`.
2. Shrink `guest/stubs/net-l4-factory/src/lib.rs` from 245 lines to the get-only body (delete `SharedL4/SharedConnection/SharedListener/SharedUdp`, all 13 methods, all `to_*`/`map_*` helpers).
3. Change the serving compositions (`check-share` gate config, the station-net boot config, the QEMU unit shape) from `… $ l4.factory` to `… & l4.factory`.
4. `gate.rs`: **no change** — `lookup_owner_funcs` finds the lifted exports by the same names; R7 validation text gains the `&` spelling. The `dns_servers: Option<Func>` workaround stays only for genuinely-old *providers*, no longer for stale tails.
5. First: the §3 spike — wac resource-type unification for import-arg + aliased-export of the same base instance; fallback if it fails is C (build-time synthesis), with B's composition spelling kept so the fallback is invisible to configs.

**The strongest argument against B** (stated, not buried): it changes the *shape* of the upgrade path. With the area/41 tail, "add policy later" meant editing the factory component in place — same composition expression, new body. With B, a no-policy share is `stack & l4.factory`, and adding policy means inserting a full 15-method wrapper component (`stack $ net.l4.filtered & l4.factory`): the boilerplate returns exactly at the moment you first want policy, now without a macro to soften it — and `&` acquires a second meaning ("the sharing operator") on top of "environment packaging," which is doctrinal surface to defend. The honest reply: the wrapper that returns is `net.l4.filtered`, which already exists, is policy (not transcription), and is the §4-sanctioned shape; and `&`'s "second meaning" is literally its first meaning — keep exports visible — applied where it matters.

## 10. Workarounds and assumptions

* **Assumption to spike (§3.3 / §9.5):** wac-graph unifies the resource type between the factory layer's instantiation argument and the base's aliased outer export. CM type rules say yes; not yet proven on wac-graph 0.10.0.
* **Inherited, unchanged:** `dns_servers: Option<Func>` (`gate.rs:870`) remains for old providers — honest version skew, no longer tail skew.
* **Deferred:** `&` admitting a binary final layer (binary-that-serves via lifting); until then that case uses self-authored factory exports or spawn grants (M2's shape needs neither).
* No new workarounds introduced by this study.

## Postscript: the owner pushed past B (2026-06-10, settled in area/41)

The recommendation above (B, `&`-lift with a get-only factory layer) was approved and
then immediately superseded by the owner's sharper question: "ought we just expose the
factory methods on the providers themselves, since `l4.factory` is basically a no-op
signifier?" Final settled shape, implemented in area/41's respin:

- The blessed `eo9:net/l4-factory` INTERFACE survives as the kernel's uniform mint
  point and the R7 validation target — but no standalone component implements it.
- Every l4 provider exports it natively (11-12 lines each; filtered's `get()` mints
  filtered sessions, so policy sharing composes as a plain `stack $ net.l4.filtered`).
- Serving compositions are plain provider chains plus the share clause; the marker
  role the study assigned to `&` is carried (authoritatively) by the share grant —
  export ≠ grant.
- `&` retreats to retrofit duty: adding a factory to a third-party provider that
  lacks one. Doctrine line as merged: "`$` seals — a provider-tailed chain serves;
  `&` retrofits."
- The B-vs-C spike (wac resource-type unification) was obviated: with no lift there
  is no cross-component type-unification question.

The analysis above stands as the reasoning record — particularly the D rejection
(handle-as-capability primitive), which the final shape did not revisit.
