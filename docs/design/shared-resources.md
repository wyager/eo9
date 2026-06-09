# Shared Resources via Owned Instances and Kernel Call Gates

**Status:** design proposal for owner review — no code until sign-off.
**Supersedes:** the channel-based sharing sketch (rejected; recorded in §11) and dissolves the plan/09 D44 fused-task constraint (sequential-only telnetd sessions).
**Reading prerequisites:** SPEC.md (capability doctrine), plan/09 entries 44–45 (the handle-transfer finding), `crates/eo9-runtime/src/task.rs` and `tests/spike_cm_async.rs` (the wasmtime-45 store discipline findings), `kernel/eo9-kernel/src/wasm/{shellexec,fibercompile,svc}.rs` (the checkout discipline and the nested-entry precedent).

---

## 1. Doctrine

**Exclusive ownership is the semantic ground truth.** Today a task's claim on a NIC is sole and total: the driver stack is fused into the task's own component, the device is claimed at bring-up, quiesced at teardown, and nothing outside the task can reach it. Nothing in this design weakens that. Sharing is **optional**, and it is an implementation choice made by the *resource owner* — never by the OS, never by a consumer, never ambiently.

**The uniformity invariant (new SPEC text).** This design adds one sentence-level invariant to SPEC.md:

> *There is no API distinction between an exclusive and a mediated implementation of an OS interface. A consumer importing `eo9:net/l4` (or any OS API) cannot tell whether its import is satisfied by (a) the driver stack fused into its own task, (b) a shared instance hosted in an ancestor's task reached through a kernel call gate, or (c) any nesting of the same. Capability grant and implementation strategy are orthogonal axes; switching between them is a deployment-configuration change, never a code change.*

The recursion is what makes this an invariant rather than a feature: a shared implementation *itself imports the same interfaces*. `net.l4.over-l2` imports `eo9:net/l2`; that l2 import can be satisfied by a fused `net.virtio`, or by a gate to an ancestor hosting the NIC, or — one level up — by the enclosing deployment's own gated l2. Granting init the raw NIC (today's `pci` boot token) is hardware passthrough to the whole deployment; granting init a gated/virtualized NIC is nested virtualization; **init cannot tell the difference**, because its import surface, instantiation behavior, and error vocabulary are identical in both worlds. The third example in SPEC's `only` section (`only eo9:fs $ virtualnet $ browser`) already shows the compose-time half of this; the call gate supplies the run-time half without changing the API's shape.

**No new channel APIs. No service-discovery APIs.** Consumers keep importing exactly the WIT they import today. The entire mechanism lives in the executor (spawn-time plumbing) and the kernel (the gate), which is where SPEC already locates the act of granting capability ("spawn ... is precisely the act of granting capabilities and CPU").

---

## 2. The model in one page

Five terms, used precisely throughout:

| term | meaning |
|---|---|
| **owner** | The task whose store contains the live provider instance (e.g. the smoltcp stack). Always an ancestor of every sharer (supervision-tree rule, §6). |
| **hosted instance** | A provider instance inside the owner's fused component whose exported interface the compose/compile step kept addressable (a *hosting export*, §5.1) so the kernel can call it. |
| **gate** | A kernel-held, unforgeable value minted to the owner: the authority to bind one interface to one hosted instance, optionally scoped (§4). A capability in the ordinary Eo9 sense — possessed, passed down at spawn, never discovered. |
| **grant** | The spawn-time act of satisfying a child's import slot from a gate. After a grant, the child's `eo9:net/l4` import is implemented by kernel call-gate shims instead of fused code or a kernel root provider. |
| **gate call** | One invocation of a granted import: the child's host call traps to the kernel, the kernel executes the typed call against the owner's hosted instance, the typed result flows back. Canonical ABI does all boundary copying; there is no protocol code anywhere. |

Shared state lives in **one place**: the owner's store. There is no pump process, no server task, no message loop. The owner's *guest code* does not need to be runnable for children's calls to execute (§3.3 explains exactly what does need to happen, against wasmtime's real store discipline).

---

## 3. The call-gate kernel design

### 3.1 Spawn-time binding

A grant binds a triple at spawn: **(owner task, hosted instance export, child import slot)**, plus an optional scope (§4).

Mechanics, against today's `spawn_child` (`kernel/eo9-kernel/src/wasm/shellexec.rs:932`):

1. `eo9:exec/task.spawn` grows a `grants: list<gate-grant>` parameter, `gate-grant = { slot: string, gate: gate }` where `gate` is a host resource (like `component`/`image`/`task` today). Taking the gate out of the spawner's table at spawn is the same ownership-transfer-at-the-API-boundary move the component-typed-argument path already does (`shellexec.rs:2445`, "the detach precedent").
2. For each grant, the kernel registers the granted interface's host shims in the child's linker and records a **grant binding** in the child's `KernelState`: `{ grant_id, owner_task, export_path, scope, translation_table }`. The shims are ordinary `func_wrap_concurrent` host functions, exactly the shape of `providers.rs`'s `read-line`/`sleep` — each one looks up the store's grant binding and runs the gate-call future of §3.5.
3. **v1 simplification:** the spawn linker is cached per grant-shape (`SPAWN_LINKER`, `shellexec.rs:894`). For M1 we register gate shims for the shareable interface(s) unconditionally in that cached linker; a shim in a store with no grant binding answers with today's capability refusal (`missing_capability`'s "the program requires the network…" story). This is collision-free today because the kernel links **no** root provider for `eo9:net/*` (`shellexec.rs:2594`). The general rule — a grant overrides a root provider per slot, requiring per-spawn linkers — is recorded as a follow-up; nothing in v1 needs it.
4. The loader rule is unchanged: an import that is neither fused, nor root-provided, nor granted is refused at instantiation with the capability story. Required-vs-optional, `only`, sealing — all unchanged; a grant is just one more way an import slot gets satisfied, decided by someone who *possesses* the authority.

### 3.2 The trap path

A gate call, end to end (the names are the implementation's, file-anchored):

```
child guest code                      kernel                              owner store
─────────────────                     ──────                              ───────────
l4.recv(conn, buf) ──canonical ABI──► gate shim (func_wrap_concurrent
                                      in the CHILD's store):
                                       1. translate handles: child reps
                                          → owner-side entries via the
                                          grant's translation table
                                       2. build GateCall { grant_id, fn,
                                          args: Vec<Val>, result cell }
                                       3. fast path? (§3.3) else enqueue
                                          on the owner's GateQueue and
                                          ring the owner runnable
                                       4. park (GateCallFuture, §3.5)
                                                                          drive loop polls owner slot
                                                                          ──► owner drive future's INTAKE
                                                                              drains GateQueue, starts the
                                                                              call as a concurrent guest
                                                                              call on the hosted export
                                                                          ──► smoltcp runs; may park on RX;
                                                                              interleaves with main + other
                                                                              gate calls poll-by-poll
                                                                          ◄── call completes: results
                                                                              (Vec<Val>) into the cell,
                                                                              owner-side resources entered
                                                                              into the grant table
                                       5. child's parked future wakes,
                                          translates result handles
                                          owner→child, returns
◄──canonical ABI── typed result
```

Everything that crosses the kernel is plain data: `Val`s plus translation-table indices. The gate holds **no** pointer into either store. Full WIT types end-to-end; the only "encoding" anywhere is the canonical ABI, applied exactly where it is already applied for every host call.

**Honest cost accounting** (the spec's claim, made precise): one gate call costs **two** host-call boundary crossings — the caller's lower/lift at its own store boundary (which it would pay for any host call) plus the owner-side lower/lift when the kernel calls the hosted export. For `eo9:io` buffers there is **no byte copy at the gate**: buffers are host resources with kernel-side byte storage (`shellfs::BufferTable`), so an `own<buffer>` crossing the gate is a host-table entry move; the actual byte copies remain the guest↔buffer accessor calls each side pays anywhere. §8 quantifies.

### 3.3 Store discipline and re-entry rules — the hard part

The pinned wasmtime-45 facts, from the spike and the executor as built (these are load-bearing; the design is shaped around them, not around the abstract Component Model):

* **F1.** All component-model-async execution state lives in the `Store`; the embedder drives it by polling one future. Usermode: `Store::run_concurrent` (`crates/eo9-runtime/src/task.rs:11-24`). Kernel: a bare `main.call_async` inside the drive future (`shellexec.rs:1137`).
* **F2.** While that drive future exists it **mutably borrows the store**, and a fuel yield suspends the executing fiber *in place inside the in-flight poll* — the future cannot be dropped and re-created without destroying the guest (`task.rs:16-20`, spike test 2).
* **F3.** wasmtime forbids re-entering a store's event loop from one of its own host functions (`shellexec.rs:23` — this is why children execute on the drive loop, not inside `spawn`).
* **F4.** Polling a *different* store's drive future from inside a host call is sound and field-proven: fibercompile's `pump` runs `drive_children() + drive_services()` from inside a `compile` host call, on the fiber stack, with the **checkout discipline** (`ChildSlot::Polling` / `SRun::Polling`) guaranteeing the nested pass skips whatever task is currently being polled (`fibercompile.rs:24-27, 193-205`). `CURRENT_PARENT` is saved/restored around the nested pass.

**Consequence (the central re-entry rule).** Because of F1+F2, the kernel can never enter an owner's store "from outside" at an arbitrary moment — the store is mutably borrowed by the owner's own drive future for the task's whole life. Therefore the only legal entry point into a live owner's store is **from inside the owner's own drive future**. The gate is designed accordingly:

* A task spawned with hosting exports (a *hosting task*) gets a widened drive future: instead of "one call to `main`", it is **`run_concurrent` over (main + a kernel-side gate intake)**. The intake is kernel code living inside the owner's `run_concurrent` closure; on every poll of the owner's drive future it (a) drains the owner's `GateQueue`, starting each entry as a concurrent guest call on the hosted export via the closure's accessor (the only place wasmtime permits starting calls — `task.rs:690-692` is the exact precedent), and (b) polls the in-flight gate-call subtasks alongside `main`. Non-hosting tasks keep today's `call_async` drive shape — **zero delta for the exclusive-ownership path.** The kernel does not use `run_concurrent` today; bringing it up on bare metal (no_std) is M1's largest single work item and Risk R2.
* **"The owner does not need to be scheduled," stated precisely.** What must happen for a child's call to execute is one poll of the owner's *drive future* by the drive loop — a kernel-side turn for the owner's **store**. No guest instruction of the owner's `main` runs unless `main` itself is independently runnable; the intake costs the owner no guest fuel and exists whether `main` is parked on `read-line`, parked forever, or already returned-pending-children. This is categorically different from a pump *process* (the rejected netd, §11): there is no guest server loop, no protocol, no liveness dependence on the owner's program logic. The one honest cost is scheduling latency: a queued call waits for the next drive pass. Which leads to:
* **The fast path (inline first-poll).** When the gate shim's future is first polled inside the child's poll, the kernel checks the owner's registry slot:
  * **Checked in (`Running`)** → the shim may *nested-poll the owner's drive future right now*, from inside the child's poll. This is entering a different store (F3 not violated) and is exactly the F4/fibercompile shape: check the owner out (`Polling`), poll once with the gate call freshly enqueued, check back in, save/restore `CURRENT_PARENT`. An eagerly-completing call (smoltcp has the bytes buffered) completes within the child's own poll — the "first-poll-inline" discipline SPEC already prescribes for host calls. If the call parks inside the provider, the shim parks too and the slow path takes over.
  * **Checked out (`Polling`)** → we are inside the owner's own poll chain (e.g. the gate call originates from a child being pumped from inside the owner's poll by fibercompile, or the owner is an ancestor in the current nesting). Nested entry here would be re-entry into a store already mutably borrowed up-stack — **forbidden**; the `Polling` sentinel is precisely the guard, doing for the gate what it already does for the compile pump. The shim enqueues and parks; the call executes on the next regular pass. Correctness never depends on the fast path.
  * **`Done`** → the gate is severed; typed failure (§6).
* **Quiescence summary** (the rule, quotable): *an owner's store is enterable iff its registry slot is checked in; entry is always one checked-out poll of its drive future; the in-store concurrency between `main` and gate calls is wasmtime's own (subtasks inside one `run_concurrent`), never two embedder entries at once.* Single boot core today; on multi-core this rule becomes "one poller per store at a time," which is also where SPEC's open multi-core task rule lands — recorded, not designed here.

### 3.4 The lock and the wait queue

Two layers, deliberately distinguished:

* **Store serialization (OS-mandatory, free).** Inside one store, wasmtime interleaves concurrent calls only at await points; code between awaits is atomic with respect to other callers. Plus the checkout rule above: one embedder entry at a time. The OS gives every provider this baseline for free; no provider can opt out of it.
* **The resource lock (provider-decided granularity).** Each hosted instance carries a gate lock partitioned into **lock domains**. A gate call must hold its domain's lock **for each poll** of its execution inside the owner, releasing at each park point — the lock is held *per-poll, never across a wait*, which Eo9's async/poll-shaped APIs make natural (a `recv` parked on RX holds nothing; a `send` mid-poll holds its domain for that poll only). Contended admission — two calls wanting the same domain's poll — is resolved one-winner-FIFO: losers sit on the domain's **wait queue**, which is just their parked `GateCallFuture`s (§3.5), woken in order when the domain frees. Executor-suspended, no spinning, no timers (event-driven liveness: enqueue and lock-release both ring wakers; a `liveness: stranded runnable` line during gate traffic is a bug, per the SPEC backstop doctrine).
  * **[OWNER] Lock granularity is the API implementer's decision per provider, declared in the share (§5.1), never hardcoded in the OS.** Choices: `whole-instance` (one domain; every call serializes per-poll against every other — the safe default) or `per-token` (one domain per ID token, §4 — e.g. per-connection, letting two sessions' `send`s interleave without queueing against each other while the instance-level domain still covers token-less ops like `listen`).
  * **[OWNER] Default for `net.l4.over-l2` (smoltcp): `whole-instance`.** smoltcp's interface + socket-set is one data structure; its poll model tolerates poll-interleaved callers but not intra-poll concurrency, which `whole-instance` per-poll locking gives exactly. `per-token` for l4 is a measured follow-up, not v1.

### 3.5 The caller's suspended future

The gate shim returns a `GateCallFuture` — the same species as `task.wait`'s `poll_fn` future (`shellexec.rs:2494-2538`) and the parked `ReadLine`/`SleepUntil` providers, so it composes with the existing executor without new machinery:

```
GateCallFuture { call: Arc<GateCall> }            // GateCall lives in the kernel gate registry
  state machine per poll:
    Done(results)      → translate result handles owner→child; lift; Poll::Ready
    Severed(reason)    → the interface's own denied/io error arm; Poll::Ready (§6)
    Enqueued | InFlight:
       fast path legal? (owner checked in, first poll)  → nested owner poll (§3.3), then re-check
       else: register waker on the GateCall (and on the lock domain's wait
       queue if waiting for admission); Poll::Pending
```

Wake edges (all events, no polling): intake completes the call → rings the cell's waker; lock domain frees → rings the queue head; gate severed → rings everyone. The child's drive future then re-polls on the normal pass. The `GateCall` is `Arc`-shared between the child's future and the gate registry so either side can disappear first (next section).

### 3.6 Cancellation — caller killed mid-gate-call

When the caller dies (kill cascade, Ctrl-C, trap), its slot becomes `Done(Killed)` and its drive future is dropped at the next checkout observation (`shellexec.rs:627-631`) — which drops the `GateCallFuture`. Its `Drop` impl settles the call by state:

* **Enqueued** → removed from the queue; never enters the owner. Nothing happened; nothing to clean.
* **InFlight** → the call **runs to completion inside the owner and the result is discarded** (the cell's other end is gone). This is SPEC "Kill and linearity" verbatim: anything the killed task transferred away (the `own<buffer>` inside a gated `send`) belongs to the transferee — the provider — which completes or aborts on its own schedule and drops the now-unreceivable result. The owner's linear memory is never torn: a call either never started or completes on the provider's own terms. We deliberately do **not** use upstream subtask-cancellation in v1 (less vendored surface to trust; recorded as a refinement). Orphan duration is bounded by the API's own deadlines (l4's `accept` carries one; SPEC's "everything that parks, parks bounded" makes unbounded provider waits a pre-existing liveness bug, not a gate problem).
* In both cases, the grant's teardown enqueues **owner-side handle drops** for every translation-table entry the grant owned (the scoped `tcp-connection`, listeners, in-flight buffers' owner entries); these execute as kernel-internal gate operations on the owner's next pass, running the provider's destructors — which is where a per-session connection gets its FIN driven (§7, a strict improvement over the D44 close-gap workaround, because the owner *stays alive to pump it*).

### 3.7 Fuel

**[OWNER-adjacent, recorded as a v1 ruling to confirm:** gate-call execution inside the owner burns the **owner's** pool (effectively-infinite, quantum-sliced like every task — `FUEL_QUANTUM` preemption bounds any one pass). Doctrinal account: sharing is the owner's implementation choice, so serving is the owner's metered cost; an owner throttles via lock admission. The principled end state — caller-donated fuel, conserved down the tree per SPEC — is deferred with the rest of guest-directed `resume` (the E5 limitation), which this design neither worsens nor fixes.**]**

---

## 4. The ID-token convention

Tokens are **names within a grant — never authority**. The kernel knows the calling task on every gate call (the shim runs in the caller's store; the grant binding is in the caller's `KernelState`), so caller identity is implicit and unforgeable. A token exists so the *owner* can address sub-resources of its instance when scoping a grant: "this child's l4 is connection 3."

The convention, in three rules:

1. **The consumer-facing API does not change.** Scoping rides the root handle: the granted import's `default()` returns a root handle whose translation-table entry the owner pre-bound. The token never appears in consumer code; the opaque resource handle *is* the name. (SPEC's multi-instance rule already makes every named import mint its own abstract root-handle type, so a gated slot's handles can't leak into another slot's — the type system was built for this.)
2. **Confinement is the translation table.** A child can only ever name owner-side entries in its own grant's table; reps outside it are typed unknown-handle errors, never owner-store access. Resources the owner's instance returns through a gated call (an `accept`ed connection) are entered into *that grant's* table — births stay inside the scope that produced them.
3. **[OWNER] The exact owner↔provider surface for minting scoped handles.** Two candidates, decided per provider:
   * **(a) Handle passing, zero new WIT** — the owner passes an existing resource (its accepted `tcp-connection`) into `gate(...)` (§5.2); the grant's `default()`-visible scope is exactly those handles, and the provider needs no new function. Sufficient for telnetd (§7). **Recommended v1 default.**
   * **(b) A provider-exported `scope` function** — `scope: func(root: borrow<l4-impl>, token: string) -> l4-impl` in the provider's own vocabulary, for providers that want token-named attenuation richer than handle enumeration (e.g. "an l4-impl that may only `connect` to 10.0.0.0/8"). A minor, uniform, opt-in addition — present only in providers that choose to support it, invisible to consumers.

---

## 5. Spawn-time plumbing and the compose algebra

### 5.1 Hosting exports — `$`-fusion is literally unchanged

The algebra's laws are untouched: `$` still seals, still drops unconsumed provider exports (kind preservation), `&` still bundles, `only` still bounds. The one new knob is at the **compile/spawn boundary**, where executor concerns already live (limits, fuel, debug info):

* A compile/spawn option `share <interface>` asks the fusion step to keep the named inner provider export addressable, re-exported under a reserved rider prefix (working name `eo9-host:` — e.g. `eo9-host:l4` aliasing the fused `net.l4.over-l2` instance's `eo9:net/l4` export). Like `eo9:rt/configured` and `eo9:rt/diagnostics`, rider-prefixed exports are runtime contract, ignored by the binary-vs-provider kind judgment, never importable by guests, never named in allow-lists — so SPEC's "binary or provider, never both" survives intact: in the *algebra*, the artifact is still a binary; the hosting export is executor surface, exactly as `bind` is. `describe` reports it in a distinct `hosted` field (auditability without changing the kind). **[OWNER]** to confirm the prefix spelling and the `describe` shape.
* The lock granularity declaration (§3.4) rides the same option: `share eo9:net/l4 lock=instance`.

**Both deployment shapes of the same components** (this is the uniformity invariant made concrete — the components are byte-identical; only configuration differs):

```
# Shape A — exclusive, fully fused (today's D44 shape; remains the hot-path choice):
  net.virtio $ net.l4.over-l2 $ net.text $ eosh        ── one task, one NIC claim,
                                                          zero-cost in-task calls

# Shape B — shared, gated (this design):
  owner:   spawn( net.virtio $ net.l4.over-l2 $ telnetd,  share eo9:net/l4 )
  child:   spawn( net.text $ eosh,  grants: [ l4 ← gate(connection N) ] )
                                                       ── one NIC claim (the owner's),
                                                          N concurrent session tasks
```

### 5.2 The exec surface (sketch)

```wit
// eo9:exec/task additions — executor surface, the locus where granting already lives
resource gate;                          // authority to bind one interface to a hosted instance

variant gate-error { not-hosting(string), unknown-resource, gate-limit }

/// Mint a gate over one of THIS task's hosted instances (spawned with `share`).
/// `scope`: owner-side resource handles moved into the grant (the §4(a) convention) —
/// ownership transfer at the API boundary, the component-arg precedent.
/// `token`: the grant's name, for diagnostics and per-token lock domains.
gate: func(interface: string, scope: list<scoped-handle>, token: string)
    -> result<gate, gate-error>;

record gate-grant { slot: string, gate: gate }
// spawn(..., grants: list<gate-grant>)  — satisfied slots behave per §3.1
```

Gate values flow only downward (spawn arguments), so every holder is a descendant of the owner — the ancestry invariant of §6 holds by construction. **v1 rule: `detach` of a task holding or granted-by a live gate is refused with a typed error** (outliving your gate's owner is exactly the half-dead state this design exists to make unrepresentable).

### 5.3 Init's services config grammar

```
# hosting service:  share <share-name>=<interface>[ lock=instance|token]
lan    = net.virtio $ net.l4.over-l2 $ netmon   share lan=eo9:net/l4   restart restart.always
# grantee:          use <slot>=<share-name>
httpd  = httpd   use l4=lan   restart restart.always
console = eosh
```

Init resolves `use` edges by minting gates from the named hosting service and passing them in the grantee's spawn. One honest wrinkle, stated rather than hidden: init's services are siblings under the registry, so the owner is not *literally* an ancestor of its grantees. The registry therefore treats every `use` edge as a **supervision edge with subtree-identical semantics**: the owner's death kills all its grantees (then policies apply, restarting the owner before its grantees — a topological restart order over the `use` DAG; cycles are a config error, refused pre-run). For M2, none of this is needed — telnetd spawns its own sessions, literal ancestry — so the registry-edge generalization is specified here but scheduled post-M2. **[OWNER]** to confirm the grammar spelling and the restart-ordering rule.

**[OWNER] Which resources ship shared-by-default in the usermode deployment.** Proposed for decision: usermode's default profile hosts **nothing** shared — exclusive/fused remains the out-of-the-box shape everywhere; sharing is always an explicit `share`/`use` in config. (Rationale: defaults are capability policy; the conservative default is the one whose blast radius is a single task.) The first candidates *worth* sharing when the owner opts in: the l4 stack (this design's driver), the future host-unix-l4 provider (plan/09 entry 45's usermode-parity gap), and `eofs` once multiple writers exist.

---

## 6. Death and teardown

**The supervision tree is the death model — there is nothing else.** The owner is an ancestor (or supervision-superior, §5.3) of every sharer. Owner dies → the subtree dies. No re-introduction protocols, no reconnect states, no generation counters: a sharer that could outlive its owner is unrepresentable, which is precisely why gates only travel downward and detach-with-gates is refused.

Teardown ordering when the owner dies (kill, trap, or normal exit) — each step ordered by the invariant it protects:

1. **Sever first.** The gate registry marks every grant on the dying owner severed, atomically with the kill marking (same `KLock` critical section as `kill_task_tree`'s slot flip). From this instant no gate call enqueues; parked and in-flight calls complete with the interface's own error vocabulary — `denied`/`io("provider task ended")` where the interface has the arm (every Eo9 net layer does, per SPEC). Severed-before-anything is the only ordering the gate *needs*: the gate holds reps and `Val`s, never store pointers, so no use-after-free is reachable once severance precedes the store drops.
2. **Kill cascade.** The existing machinery, unchanged: `kill_task_tree` walks `PARENTS` to a fixed point (`shellexec.rs:456`); registry `use` edges extend the walk for init-level shares. Children's drive futures drop on next checkout observation, releasing their stores — and with them their gate shims and translation tables (kernel-side plain data; order-independent by step 1).
3. **Owner store drop.** The drive future drops; the store drops; provider destructors run; `Drop for PciTables` quiesces the device — the plan/12 D62 quiesce-at-teardown precedent, verbatim: bus-mastering off and rings disarmed *before* DMA buffers free, on completion, trap, **and** kill, with the `pci: quiesced N device(s) at task teardown` line keeping the ordering visible in transcripts. Sequential telnetd (plan/09 entry 45, session 2) already proved NIC quiesce + re-claim across owner lifetimes; this design inherits that proof.
4. **Supervision policy.** The owner's own supervisor (telnetd's parent; init for services) observes the outcome and applies its restart policy — restarting the *whole subtree* fresh: new owner task, new NIC claim, new gates, new sessions. There is never a "reattach."

Caller-side death is §3.6. The remaining case — **owner's `main` returns while gated children live** — is the owner's choice like everything else here: the hosting task's drive future completes only when `main` is done **and** no live grants remain (mirroring init's "console exited with services still running" logic); an owner that wants hard-stop semantics kills its children first, which it can, being their ancestor.

---

## 7. The worked example: concurrent telnetd

**Who owns what.** One task — `net.virtio $ net.l4.over-l2 $ telnetd`, spawned by init with `share eo9:net/l4 lock=instance` — owns the NIC claim and hosts the one smoltcp instance. telnetd's *own* l4 use (listen/accept) is the fused import: zero-cost, exactly as today. Per-session children are `net.text $ eosh` — **no NIC, no stack, two components** — each granted l4 scoped to its accepted connection.

```
                 init (svc grant, restart policy)
                   │ spawn(share l4)
                   ▼
        ┌─────────────────────────────┐
        │ telnetd task (OWNER)        │   one NIC claim, one smoltcp instance
        │  net.virtio ── l2 ──┐       │
        │  net.l4.over-l2 ◄───┘       │   hosting export: eo9-host:l4
        │  telnetd (accept loop)      │
        └───────┬─────────────┬───────┘
   gate(conn 1) │             │ gate(conn 2)        … conn N
                ▼             ▼
        ┌──────────────┐ ┌──────────────┐
        │ session 1    │ │ session 2    │   children: net.text $ eosh
        │ net.text+eosh│ │ net.text+eosh│   l4 import = kernel call gate
        └──────────────┘ └──────────────┘
```

**Accept → spawn → first bytes** (note how `net.text` runs unmodified — its `accept` simply yields the pre-scoped connection):

```
client            telnetd (owner)                 kernel gate            session child
──────            ───────────────                 ───────────            ─────────────
SYN ─────────────► fused l4.accept → conn₃
                   gate("eo9:net/l4",
                        scope=[listener₃,conn₃],   mint gate g₃; move
                        token="conn-3") ─────────► conn₃ into g₃'s table
                   spawn(net.text $ eosh,
                         grants=[l4 ← g₃]) ──────► bind grant in child   ── child boots
                                                                          net.text: l4.default()
                                                                          ◄─ scoped l4-impl (g₃)
                                                                          net.text: listen+accept
                                                   gate call: accept on
                                                   scoped listener ──► owner store: yields conn₃
                                                                       (pre-accepted; immediate)
                                                                          ◄─ tcp-connection (child rep)
"hello\n" ───────► (NIC, smoltcp RX buffer)                               net.text: recv(conn, buf)
                                                   gate call recv:
                                                   fast path → owner
                                                   store → smoltcp has
                                                   bytes → returns ──────► line to eosh; prompt back
◄──────────────────────────────────────────────── gated send ◄──────────  eosh output via net.text
```

**Two sessions concurrently** — the thing D44 could not do. Both children's `recv` calls sit in-flight inside the one owner store as concurrent calls; the instance lock interleaves them per-poll; a parked `recv` (no bytes) holds nothing:

```
drive-loop pass:   poll owner ──► intake: poll recv(conn₃) → parked on RX
                                          poll recv(conn₄) → parked on RX
                                          poll telnetd.main → parked in accept
   (NIC RX irq: bytes for conn₄ → owner runnable — an EVENT, never the backstop)
next pass:         poll owner ──► smoltcp ingests frame; recv(conn₄) completes → cell₄
                   poll session₄ ──► GateCallFuture ready → eosh₄ gets its line
                   poll session₃ ──► still parked; session₃ unaffected
```

**Session death.** Client FIN → gated `recv` answers `none` → eosh exits → child task done → telnetd (its `task.wait` precedent) observes, drops g₃ → grant teardown enqueues conn₃'s owner-side drop → provider destructor closes the socket and — because the owner is alive and scheduled — **the FIN actually gets pumped**, retiring the D44 throwaway-accept close workaround for gated sessions. Kill mid-transfer is §3.6: the in-flight call completes into the void; conn₃ is closed the same way.

**Whole-subtree restart.** telnetd traps or is killed → §6 ordering: gates severed (any racing session call gets `l4-error::denied`-class severance), sessions killed by cascade, NIC quiesced at owner store drop, init's `restart.always` respawns telnetd → fresh claim, fresh stack, port 23 listening again. Clients reconnect; nothing reattaches. (Remote `poweroff` keeps its D44 behavior: it propagates as the *session's* outcome to telnetd, which refuses it by policy — unchanged.)

---

## 8. Performance notes (honest)

* **Per gate call vs fused:** a fused call is an inlined function call (the canonical-ABI copies optimized out — SPEC "Contract vs cost"); a gate call pays two host-call boundary crossings (caller's + owner's lower/lift of `Val`s), the kernel-side queue/translate bookkeeping, and — off the fast path — up to one drive-loop pass of latency (event-driven; the 10 ms `IDLE_WAKE_INTERVAL_NS` is a backstop, not the cadence, and a gate-induced backstop wake is by doctrine a bug). There are **no measurements yet**; producing the number (gate `recv` vs fused `recv`, QEMU and board) is an M1 exit criterion, and the spawn-trace machinery (`shellexec.rs:1232`) is the measurement precedent to extend.
* **Buffers don't multiply the cost:** `own<buffer>` transfer through the gate is a host-table entry move (§3.2); the dominant data-path cost — guest↔buffer byte copies — is identical in both shapes.
* **Why acceptable:** the gated path's natural grain is socket *operations* (a line, a segment batch), not bytes; per-op overhead in the µs range against ops that already traverse smoltcp + a NIC is in the noise for an interactive shell session — and was previously *impossible* concurrently at any price.
* **When fusion remains the right choice:** hot paths — per-frame l2 forwarding, the disk block path under eofs, anything where the op rate makes two extra boundary crossings visible. The uniformity invariant is the escape hatch in both directions: shapes A and B of §5.1 are the same bytes, so promoting a deployment from gated to fused (or back) is a config change, measured not believed.

---

## 9. Risks, each with a discriminating test

| # | risk | discriminating test (pass/fail observable) |
|---|---|---|
| R1 | The inline fast path (nested owner poll from inside a child's host call) corrupts one of the two process-wide TLS slots or the fiber discipline. | M1 QEMU test: gate a call (a) owner idle, (b) owner nested via a fibercompile-pump-style pass; instrument both TLS slots with canaries around the nested poll. Fail → ship queue-only (correct, +1 pass latency), keep fast path on the follow-up ledger. |
| R2 | `run_concurrent` + multiple concurrent calls in one store doesn't work on the no_std vendored wasmtime (the kernel only exercises `call_async` today). | M1 bring-up gate, *before* any gate code: QEMU unit boot driving one store with `main` + one concurrent export call; pass = interleaved completion. Fail → vendor-patch lane (the README's upstream-shaped-relaxation discipline), re-size M1. |
| R3 | Resource translation is forgeable or leaky (a child reaches an owner entry outside its grant). | M1 negative test: hand-crafted guest passes raw reps; must get typed unknown-handle, never owner-store effect. Plus: handles from grant A presented through grant B refused. |
| R4 | smoltcp misbehaves under poll-interleaved concurrent callers (state assumptions D44's one-caller world never tested). | M2 `check-telnet-concurrent`: two live sessions echoing session-tagged payloads interleaved byte-paced, third connection handled per policy; any cross-session bleed or stall fails the scripted gate. |
| R5 | Death races: caller killed mid-call corrupts the owner; owner killed mid-call hangs a child. | M2 scripted kills at randomized points (the byte-paced D49 harness): session killed mid-`recv` → owner serves remaining session; owner killed → children report severance then die, NIC re-claims on respawn (assert the existing `pci: quiesced` line). |
| R6 | Gate wakes leak into the backstop (liveness regression — work runnable while the core slept). | M2 transcripts must contain zero `liveness: stranded runnable/input/intx` findings during gate traffic; the detectors already exist (`mod.rs:430-448`) and the scripted gates already print transcripts. |
| R7 | The hosting export breaks the kind judgment or leaks into the algebra (a guest importing `eo9-host:*`, an allow-list having to name it). | M1: `describe` on a shared artifact reports kind=binary + hosted list; `only` without the rider unchanged; a guest import of `eo9-host:*` refused at load. |

---

## 10. Milestones (honest sizing)

* **M1 — the call gate for ONE interface (`eo9:net/l4`), QEMU.** Hosting-export fusion option in `eo9-component` + the rider rule; kernel hosting-task drive on `run_concurrent` with the intake (the big item — R2 first); gate registry, typed hand-written l4 shims (the `providers.rs` pattern; the generic `func_new`-dynamic gate is deliberately deferred); `gate`/`grants` exec surface; tests: owner = `net.l4.loopback $ host-test` sharing l4 to a child doing connect/send/recv through the gate, plus R1/R3/R7 negatives, plus the gate-vs-fused measurement. **Sizing: the largest kernel-lane change since component-model-async bring-up itself; ~3 weeks if R2 is clean, +1–2 weeks if the vendored wasmtime needs concurrent-call patches.**
* **M2 — concurrent telnetd, QEMU.** `gate(scope=[conn])` handle-scoping; telnetd spawn-per-accept; net.text verified unmodified against a scoped grant; `check-telnet-concurrent` scripted gate (R4/R5/R6); gated-session close path replacing the D44 pump trick. **Sizing: ~1–2 weeks on a clean M1; the harness (byte-paced typing, transcripts, kill scripting) all exists.**
* **M3 — board (Orange Pi 5 Plus, `net.rtl8125`).** Same compositions, real silicon: gate traffic under the DW-WDT and fibercompile interleaving (a session compile parked while another session serves — R1's field test), bench-LAN concurrent sessions, D44 security posture restated (cleartext, trusted LAN). **Sizing: ~1 week if M2 is clean; board-lane margin applies; never touches the serial-loader port rules (boards/BOOT.md), never `/dev/cu.usbserial*`.**

Post-M3 ledger (explicitly deferred, not dropped): generic dynamic gate shims; per-token lock domains for l4; registry `use` edges for init-level shares; caller-donated fuel; grant-overrides-root-provider per-spawn linkers; subtask-cancellation for orphaned calls; usermode parity (blocked on the host l4 provider exactly as plan/09 entry 45 records).

---

## 11. Rejected alternatives

* **Channels (the Message-API service design) — rejected by the owner; reasons recorded:** (1) *Types:* WIT already is the contract; a channel re-introduces serialization, framing, and version skew at runtime — per-interface protocol code the gate has literally none of. (2) *Backpressure:* channels force buffer-sizing policy and head-of-line blocking decisions onto the OS; the gate's "one outstanding call per caller per op, executor-suspended" is the async API's own natural backpressure. (3) *Death:* a channel endpoint can outlive its peer — half-dead connections, reconnect protocols, generation counters; the supervision tree makes all of that unrepresentable. (4) *The netd pump:* a channel server is a scheduled guest process that must be live and burning quanta for anyone to make progress, with its own restart story interposed on every consumer; the gate serves with zero owner-guest scheduling. (5) *Shape:* consumers would import channel/discovery APIs — the API changes, the uniformity invariant dies, and the capability set stops being statically enumerable from imports.
* **True handle transfer** (move the live `tcp-connection` between stores): pinned impossible by the D44 investigation — an accepted connection is live state inside the owner's linear memory (smoltcp's), not a host-table entry; "transfer" would mean extracting and re-implanting provider-internal state, which no provider can be required to support. The gate moves the *call* to the state instead of the state to the caller.
* **Service mesh / discovery registry:** ambient names are ambient authority; everything reachable-by-name violates possession-is-authority and makes the import list lie. Gates are possessed values that flow only down the tree.

---

## 12. Workarounds and assumptions

* **Pinned substrate:** wasmtime 45 (vendored, patched per `kernel/vendor/README.md`); all store-discipline reasoning in §3.3 is against that version's documented behavior (`task.rs`, `spike_cm_async.rs`) and must be re-validated on any vendor bump.
* **M1 shims are hand-written typed registrations** for l4 only; the generic dynamic gate assumes async-dynamic host functions (`func_new`'s concurrent sibling) — unverified on the vendored build, hence deferred.
* **Single boot core assumed** throughout (the `KLock`/checkout machinery's existing assumption); the gate lock is where the multi-core entry rule will land.
* **Fuel:** owner-pays in v1 (§3.7); fuel conservation across the gate deferred.
* **Detach × gates refused** in v1; init-level `use` edges (supervision-edge generalization) specified but post-M2.
* **Known gaps inherited, not worsened:** the l4 close gap (improved for gated sessions, §7, but the fused-exclusive shape still needs the D44 pump trick); per-task text capture (a session child's own stdout still lands on serial); usermode l4 parity.
* **Security posture unchanged:** telnet remains cleartext/unauthenticated, trusted-LAN/dev only, said loudly everywhere it was said before.

---

### Critical Files for Implementation

- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/shellexec.rs — the child registry, checkout discipline, spawn path, and kill cascade the gate extends
- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/providers.rs — `KernelState` (grant bindings live here) and the host-shim pattern the gate shims follow
- /Users/wy/code/eo9/crates/eo9-runtime/src/task.rs — the `run_concurrent` drive shape and store-borrow findings the hosting-task drive future is built against
- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/fibercompile.rs — the nested-entry / checkout precedent the fast path reuses (and R1's reference behavior)
- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/svc.rs — init's service plumbing and restart machinery the `share`/`use` config grammar extends
