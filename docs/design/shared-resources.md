# Shared Resources via Owner-Side Resources and Kernel Call Gates

**Status:** settled design (owner sign-off in design review) — this document is the reference for M1–M3 implementation.
**Supersedes:** the channel-based sharing sketch and the first call-gate draft (hosting exports, kernel scope tables, ID tokens) — both recorded with one-line whys in Appendix A. Dissolves the plan/09 D44 fused-task constraint (sequential-only telnetd sessions).
**Reading prerequisites:** SPEC.md (capability doctrine, "Policies are programs", the svc trust posture), plan/09 entries 44–45 (the handle-transfer finding), `crates/eo9-runtime/src/task.rs` and `crates/eo9-runtime/tests/spike_cm_async.rs` (the wasmtime-45 store discipline findings), `kernel/eo9-kernel/src/wasm/{shellexec,fibercompile,svc}.rs` (the checkout discipline and the nested-entry precedent).

---

## 1. Doctrine

**Exclusive ownership is the semantic ground truth — and the default everywhere.** Today a task's claim on a NIC is sole and total: the driver stack is fused into the task's own component, the device is claimed at bring-up, quiesced at teardown, and nothing outside the task can reach it. Nothing in this design weakens that. **Nothing is shared by default**, on any target, in any profile: exclusive/fused remains the out-of-the-box shape, and sharing is always explicit opt-in wiring by the *resource owner* — never by the OS, never by a consumer, never ambiently. (Rationale: defaults are capability policy; the conservative default is the one whose blast radius is a single task.)

**The uniformity invariant (new SPEC text).** This design adds one sentence-level invariant to SPEC.md:

> *There is no API distinction between an exclusive and a mediated implementation of an OS interface. A consumer importing `eo9:net/l4` (or any OS API) cannot tell whether its import is satisfied by (a) the driver stack fused into its own task, (b) a resource hosted in an ancestor's task reached through a kernel call gate, or (c) any nesting of the same. Capability grant and implementation strategy are orthogonal axes; switching between them is a deployment-configuration change, never a code change.*

The recursion is what makes this an invariant rather than a feature: a shared implementation *itself imports the same interfaces*. `net.l4.over-l2` imports `eo9:net/l2`; that l2 import can be satisfied by a fused `net.virtio`, or by a gate to an ancestor hosting the NIC, or — one level up — by the enclosing deployment's own gated l2. Granting init the raw NIC (today's `pci` boot token) is hardware passthrough to the whole deployment; granting init a gated/virtualized NIC is nested virtualization; **init cannot tell the difference**, because its import surface, instantiation behavior, and error vocabulary are identical in both worlds. The third example in SPEC's `only` section (`only eo9:fs $ virtualnet $ browser`) already shows the compose-time half of this; the call gate supplies the run-time half without changing the API's shape.

**No new channel APIs. No service-discovery APIs.** Consumers keep importing exactly the WIT they import today. The entire mechanism lives in the executor (spawn-time plumbing) and the kernel (the gate), which is where SPEC already locates the act of granting capability ("spawn ... is precisely the act of granting capabilities and CPU"). Channels were rejected outright ("channels as a synchronization primitive is a code smell" — Appendix A): with the task tree as the only lifetime model, an owner's death kills its sharers with it, so there is no handle-revocation protocol, no half-dead endpoint, no reconnect state anywhere in this design.

**The kernel does routing only.** Every *policy* decision — which connection a session may touch, which ports a child may dial, how much of the owner's instance a grantee sees — is a program: owner-authored wasm, per SPEC's "Policies are programs." The kernel's whole contribution is the mechanism of §3: route a child's ordinary import to a resource the spawner provided, soundly, against wasmtime's real store discipline.

---

## 2. The model in one page

**Gate = route a child's import to a resource the spawner provided.** That sentence is the entire design; the rest of this document is its consequences. Five terms, used precisely throughout:

| term | meaning |
|---|---|
| **owner** | The task whose store contains the live resource being shared (e.g. the smoltcp stack's `l4-impl`, or a wrapper the owner minted). Always an ancestor of every sharer (supervision-tree rule, §6). |
| **handler** | An owner-side resource implementing an API's root-handle contract (`l4-impl`, `fs-impl`, …). Either the owner's own fused implementation handle (full access) or a wrapper resource the owner's guest code minted (scoped access, §4). |
| **gate** | The kernel routing record binding a child's import slot to one handler in the owner's store. Established at spawn from a resource the spawner *possesses* — passed down, never discovered. |
| **grant** | The spawn-time act of satisfying a child's import slot with a handler. After a grant, the child's `eo9:net/l4` import is implemented by kernel call-gate shims instead of fused code or a kernel root provider. |
| **gate call** | One invocation of a granted import: the child's host call traps to the kernel, the kernel executes the typed call against the handler in the owner's store, the typed result flows back. Canonical ABI does all boundary copying; there is no protocol code anywhere. |

Shared state lives in **one place**: the owner's store. There is no pump process, no server task, no message loop. The owner's *guest code* does not need to be runnable for children's calls to execute (§3.3 explains exactly what does need to happen, against wasmtime's real store discipline). The child is entirely unaware — it imports `eo9:net/l4` and gets whatever implementation its spawner handed it.

Two wiring shapes, one mechanism:

* **Parent→child (the common case): no export surface at all.** The spawner holds a handler — its own import's root handle, or a wrapper it minted — and passes it in the child's spawn. Nothing about the owner's composition changes; sealing is untouched; the kernel never reaches into the owner's component (Appendix A records why the rider-export draft died).
* **The broker case (init wiring strangers): factory exports.** When the parties are not parent and child — init connecting a net-stack *service* to telnetd's import — the provider side deliberately **exports a factory** in the blessed namespace (§5.2): `get: func() -> result<l4-impl, l4-error>`. Init (holding the service's task) has the kernel call the factory once at wiring time; the returned handler is then granted to the consumer exactly as in the parent→child case. The factory is the *only* export surface in the entire design, and it exists only here.

---

## 3. The call-gate kernel design

### 3.1 Spawn-time binding

A grant binds a triple at spawn: **(owner task, handler resource, child import slot)**.

Mechanics, against today's `spawn_child` (`kernel/eo9-kernel/src/wasm/shellexec.rs:959`):

1. `eo9:exec/task.spawn` grows a `grants: list<gate-grant>` parameter, `gate-grant = { slot: string, handler: <resource> }`. Taking the handler out of the spawner's table at spawn is the same ownership-transfer-at-the-API-boundary move the component-typed-argument path already does (`shellexec.rs:151`, "the detach precedent"). One honest encoding note: WIT cannot express "any resource" in `spawn`'s signature, and the handler's nominal type belongs to the spawner's own import, which exec cannot name. The exec boundary is therefore type-erased at exactly this one point: the kernel receives the caller's handle rep plus the slot's interface identity, verifies the rep denotes a resource implementing that interface's root-handle contract, and refuses with a typed error otherwise. v1 sidesteps even this: the spawn shim is hand-written and typed for l4 only (§10), so the generic erased surface rides the same deferral as the generic dynamic gate shims.
2. For each grant, the kernel registers the granted interface's host shims in the child's linker and records a **grant binding** in the child's `KernelState`: `{ grant_id, owner_task, handler_rep, translation_table }`. The shims are ordinary `func_wrap_concurrent` host functions, exactly the shape of `providers.rs`'s `read-line`/`sleep` — each one looks up the store's grant binding and runs the gate-call future of §3.5.
3. **v1 simplification:** the spawn linker is cached per grant-shape (`SPAWN_LINKER`, `shellexec.rs:875`). For M1 we register gate shims for the shareable interface(s) unconditionally in that cached linker; a shim in a store with no grant binding answers with today's capability refusal (`missing_capability`'s "the program requires the network…" story). This is collision-free today because the kernel links **no** root provider for `eo9:net/*` (`shellexec.rs:2621`). The general rule — a grant overrides a root provider per slot, requiring per-spawn linkers — is recorded as a follow-up; nothing in v1 needs it.
4. The loader rule is unchanged: an import that is neither fused, nor root-provided, nor granted is refused at instantiation with the capability story. Required-vs-optional, `only`, sealing — all unchanged; a grant is just one more way an import slot gets satisfied, decided by someone who *possesses* the authority.

The granted import's `default()` returns the handler — the same configuration→capability shape SPEC already prescribes (`configure` produces the root handle; `default()` hands out exactly that handle). A child's whole view of its grant is one opaque root handle plus whatever resources calls on it return.

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
                                                                              call on the handler's method
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

**The translation table is plumbing, not policy.** It exists so handle reps are per-grant and unforgeable: a child can only ever name owner-side entries its own grant's table contains; reps outside it are typed unknown-handle errors, never owner-store access; resources a gated call returns (an `accept`ed connection) are entered into *that grant's* table. That is routing soundness — the same property every host table already has. What a child may *do* with the handles it legitimately holds is decided entirely by which handler the spawner passed (§4); the kernel never carries a scope, a filter, or a token.

**Honest cost accounting** (the spec's claim, made precise): one gate call costs **two** host-call boundary crossings — the caller's lower/lift at its own store boundary (which it would pay for any host call) plus the owner-side lower/lift when the kernel calls the handler's method. For `eo9:io` buffers there is **no byte copy at the gate**: buffers are host resources with kernel-side byte storage (`shellfs::BufferTable`), so an `own<buffer>` crossing the gate is a host-table entry move; the actual byte copies remain the guest↔buffer accessor calls each side pays anywhere. §8 quantifies.

### 3.3 Store discipline and re-entry rules — the hard part

The pinned wasmtime-45 facts, from the spike and the executor as built (these are load-bearing; the design is shaped around them, not around the abstract Component Model):

* **F1.** All component-model-async execution state lives in the `Store`; the embedder drives it by polling one future. Usermode: `Store::run_concurrent` (`crates/eo9-runtime/src/task.rs:11-24`). Kernel: a bare `main.call_async` inside the drive future (`shellexec.rs:1167`).
* **F2.** While that drive future exists it **mutably borrows the store**, and a fuel yield suspends the executing fiber *in place inside the in-flight poll* — the future cannot be dropped and re-created without destroying the guest (`task.rs:16-20`, spike test 2).
* **F3.** wasmtime forbids re-entering a store's event loop from one of its own host functions (`shellexec.rs:23` — this is why children execute on the drive loop, not inside `spawn`).
* **F3a (v1 deferral, recorded at M1).** `ResourceAny::resource_drop` needs a sync call context and `resource_drop_async` needs the bare store — neither is callable from inside `run_concurrent`, so a gated child dropping a handle releases its kernel-side translation-table entry immediately but the owner-side resource lives until the owner's store drops. Bounded (the owner's lifetime), honest (the table entry is gone, so the child can never reach it again), and on the post-M1 ledger for when the vendored wasmtime grows an intake-safe drop.
* **F4.** Polling a *different* store's drive future from inside a host call is sound and field-proven: fibercompile's `pump` runs `drive_children() + drive_services()` from inside a `compile` host call, on the fiber stack, with the **checkout discipline** (`ChildSlot::Polling` / `SRun::Polling`) guaranteeing the nested pass skips whatever task is currently being polled (`fibercompile.rs:24-27, 193-205`). `CURRENT_PARENT` is saved/restored around the nested pass.

**Consequence (the central re-entry rule).** Because of F1+F2, the kernel can never enter an owner's store "from outside" at an arbitrary moment — the store is mutably borrowed by the owner's own drive future for the task's whole life. Therefore the only legal entry point into a live owner's store is **from inside the owner's own drive future**. This is also why the lock discipline of §3.4 is OS-level and cannot be anything else: a wasmtime store is single-entry, so only the kernel — the thing that polls drive futures — can serialize entries into an owner's store. No owner could implement that serialization in its own guest code even if it wanted to. The gate is designed accordingly:

* A task with live grants on it (an *owner task*) gets a widened drive future: instead of "one call to `main`", it is **`run_concurrent` over (main + a kernel-side gate intake)**. The intake is kernel code living inside the owner's `run_concurrent` closure; on every poll of the owner's drive future it (a) drains the owner's `GateQueue`, starting each entry as a concurrent guest call on the handler's method via the closure's accessor (the only place wasmtime permits starting calls — `task.rs:690-692` is the exact precedent), and (b) polls the in-flight gate-call subtasks alongside `main`. Tasks with no grants keep today's `call_async` drive shape — **zero delta for the exclusive-ownership path.** The kernel does not use `run_concurrent` today; bringing it up on bare metal (no_std) is M1's largest single work item and Risk R2.
* **"The owner does not need to be scheduled," stated precisely.** What must happen for a child's call to execute is one poll of the owner's *drive future* by the drive loop — a kernel-side turn for the owner's **store**. No guest instruction of the owner's `main` runs unless `main` itself is independently runnable; the intake costs the owner no guest fuel and exists whether `main` is parked on `read-line`, parked forever, or already returned-pending-children. (Calls on a *wrapper* handler do run owner guest code — the wrapper's method bodies — but as concurrent calls inside the intake, never as a scheduling dependence on `main`.) This is categorically different from a pump *process* (the rejected netd, Appendix A): there is no guest server loop, no protocol, no liveness dependence on the owner's program logic. The one honest cost is scheduling latency: a queued call waits for the next drive pass. Which leads to:
* **The fast path (inline first-poll).** When the gate shim's future is first polled inside the child's poll, the kernel checks the owner's registry slot:
  * **Checked in (`Running`)** → the shim may *nested-poll the owner's drive future right now*, from inside the child's poll. This is entering a different store (F3 not violated) and is exactly the F4/fibercompile shape: check the owner out (`Polling`), poll once with the gate call freshly enqueued, check back in, save/restore `CURRENT_PARENT`. An eagerly-completing call (smoltcp has the bytes buffered) completes within the child's own poll — the "first-poll-inline" discipline SPEC already prescribes for host calls. If the call parks inside the provider, the shim parks too and the slow path takes over.
  * **Checked out (`Polling`)** → we are inside the owner's own poll chain (e.g. the gate call originates from a child being pumped from inside the owner's poll by fibercompile, or the owner is an ancestor in the current nesting). Nested entry here would be re-entry into a store already mutably borrowed up-stack — **forbidden**; the `Polling` sentinel is precisely the guard, doing for the gate what it already does for the compile pump. The shim enqueues and parks; the call executes on the next regular pass. Correctness never depends on the fast path.
  * **`Done`** → the gate is severed; typed failure (§6).
* **Quiescence summary** (the rule, quotable): *an owner's store is enterable iff its registry slot is checked in; entry is always one checked-out poll of its drive future; the in-store concurrency between `main` and gate calls is wasmtime's own (subtasks inside one `run_concurrent`), never two embedder entries at once.* Single boot core today; on multi-core this rule becomes "one poller per store at a time," which is also where SPEC's open multi-core task rule lands — recorded, not designed here.

### 3.4 The lock and the wait queue

Locking is **OS-level** — settled, not provider-pluggable — in two layers:

* **Store serialization (OS-mandatory, free).** Inside one store, wasmtime interleaves concurrent calls only at await points; code between awaits is atomic with respect to other callers. Plus the checkout rule above: one embedder entry at a time. The OS gives every owner this baseline for free; no owner can opt out of it, and (per §3.3) no owner could implement it itself — store-entry serialization is physics, and the kernel owns it.
* **The gate lock (OS-implemented, two domain granularities).** Each owner carries a gate lock partitioned into **lock domains**, in exactly two OS-defined granularities:
  * **`instance`** — one domain per owner store: every gate call serializes per-poll against every other. The safe default, and the v1 default for `net.l4.over-l2` (smoltcp's interface + socket-set is one data structure; its poll model tolerates poll-interleaved callers but not intra-poll concurrency, which `instance` per-poll locking gives exactly).
  * **`token`** — one domain per granted handler: calls through different grants interleave; calls through the same grant serialize per-poll. This is what lets two telnet sessions' `send`s proceed without queueing against each other once l4 is measured ready for it (a follow-up, not v1).

  A gate call must hold its domain's lock **for each poll** of its execution inside the owner, releasing at each park point — the lock is held *per-poll, never across a wait*, which Eo9's async/poll-shaped APIs make natural (a `recv` parked on RX holds nothing; a `send` mid-poll holds its domain for that poll only). Contended admission — two calls wanting the same domain's poll — is resolved one-winner-FIFO: losers sit on the domain's **wait queue**, which is just their parked `GateCallFuture`s (§3.5), woken in order when the domain frees. Executor-suspended, no spinning, no timers (event-driven liveness: enqueue and lock-release both ring wakers; a `liveness: stranded runnable` line during gate traffic is a bug, per the SPEC backstop doctrine).

  The domain choice is declared where sharing is wired — the spawn grant or the init config clause (`lock=instance|token`, default `instance`, §5.3). Policy *finer* than these two domains (per-port serialization, rate limits, fairness between grantees) is not a lock feature: it is owner code — a wrapper handler that admits or defers in its own method bodies (§4), at the cost of poll-cadence wakeups. The OS lock vocabulary stays at exactly two granularities.

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
* **InFlight** → the call **runs to completion inside the owner and the result is discarded** (the cell's other end is gone). This is SPEC "Kill and linearity" verbatim: anything the killed task transferred away (the `own<buffer>` inside a gated `send`) belongs to the transferee — the owner — which completes or aborts on its own schedule and drops the now-unreceivable result. The owner's linear memory is never torn: a call either never started or completes on the owner's own terms. We deliberately do **not** use upstream subtask-cancellation in v1 (less vendored surface to trust; recorded as a refinement). Orphan duration is bounded by the API's own deadlines (l4's `accept` carries one; SPEC's "everything that parks, parks bounded" makes unbounded provider waits a pre-existing liveness bug, not a gate problem).
* In both cases, the grant's teardown enqueues **owner-side handle drops** for every translation-table entry the grant owned (the granted handler itself, listeners, in-flight buffers' owner entries); these execute as kernel-internal gate operations on the owner's next pass, running the resource's destructors — which is where a per-session connection gets its FIN driven (§7, a strict improvement over the D44 close-gap workaround, because the owner *stays alive to pump it*). A wrapper handler's destructor is owner code like its methods: dropping the session wrapper is exactly where telnetd's per-session cleanup lives.

### 3.7 Fuel

**Settled: owner-pays.** Gate-call execution inside the owner burns the **owner's** pool (effectively-infinite, quantum-sliced like every task — `FUEL_QUANTUM` preemption bounds any one pass). Doctrinal account: sharing is the owner's implementation choice, so serving is the owner's metered cost; an owner that wants to throttle a grantee does it the §4 way — a wrapper that defers — not with fuel accounting. The principled end state — caller-donated fuel, conserved down the tree per SPEC — is deferred with the rest of guest-directed `resume` (the E5 limitation), which this design neither worsens nor fixes.

---

## 4. Scoping is owner code: wrapper handlers

**Settled: there are no kernel scope tables, no ID tokens, no provider `scope` functions.** The kernel routes calls to a handler and nothing else. Everything that smells like policy — *which* connection a session may use, *which* hosts a child may dial, read-only views, quotas, multiplexing — is a program the owner writes, per SPEC's "Policies are programs." (The first draft's kernel-side token/scope machinery is recorded in Appendix A.)

The pattern, in three rules:

1. **Full sharing is passing your own handle.** An owner whose fused stack gave it an `l4-impl` may grant exactly that handle to a child. The child gets everything the owner's instance can do. This costs zero new code and is the right shape when the child is as trusted as the owner.
2. **Scoped sharing is minting a wrapper.** An owner that wants to restrict implements the API's root-handle contract in its own wasm — a resource type whose methods delegate to the owner's real handle with whatever policy the owner intends, closing over whatever state names the scope (the accepted `tcp-connection` for a telnet session; an allowed-prefix list for a "may only `connect` to 10.0.0.0/8" view). It mints one wrapper instance per grantee and passes *that* at spawn. The wrapper is ordinary guest code: inspectable, testable in usermode, composable, wrong only in ways its owner can debug. SPEC's multi-instance rule already keeps one slot's handles from leaking into another's — every named import mints its own abstract root-handle type — so wrapper handles and real handles never unify in a consumer.
3. **The consumer cannot tell.** A wrapper implements the same WIT contract, so the uniformity invariant of §1 extends all the way down: fused, gated-full, gated-wrapped are indistinguishable from inside the consumer. Attenuation is invisible by construction, exactly like `only` at compose time.

What the kernel contributes to confinement is exactly §3.2's translation table — per-grant handle namespaces, unforgeable reps — which is soundness of *routing*, not policy. A child holding a wrapped l4 can name only the handles its own gated calls produced; what those calls are willing to produce is the wrapper's business.

Cost honesty: a wrapper interposes owner guest code on every call, so a wrapped grant pays the wrapper's method bodies (owner fuel, §3.7) on top of the two boundary crossings of §3.2. For policy that is pure delegation-with-a-check this is nanoseconds of guest code; for policy that defers callers (fairness, throttling) it is poll-cadence wakeups — the price of programmable policy, paid only by owners who choose it.

---

## 5. Wiring: spawn grants, factories, and the init config

### 5.1 Parent→child: spawn grants — no export surface

The `$`-algebra is literally untouched: no compile option, no rider exports, no change to sealing or the kind judgment. An owner shares by *possessing a handler and passing it at spawn*:

```wit
// eo9:exec/task additions — executor surface, the locus where granting already lives
record gate-grant { slot: string, handler: /* type-erased at this boundary, §3.1 */ }

variant grant-error { not-a-handler(string), wrong-interface(string), grant-limit }

// spawn(..., grants: list<gate-grant>)  — satisfied slots behave per §3.1;
// the handler moves out of the spawner's table (ownership transfer at the API
// boundary — the component-argument/detach precedent, shellexec.rs:151)
```

Handlers flow only downward (spawn arguments), so every grantee is a descendant of the owner — the ancestry invariant of §6 holds by construction. **v1 rule: `detach` of a task holding or granted-by a live gate is refused with a typed error** (outliving your gate's owner is exactly the half-dead state this design exists to make unrepresentable).

**Both deployment shapes of the same components** (this is the uniformity invariant made concrete — the components are byte-identical; only configuration differs):

```
# Shape A — exclusive, fully fused (today's D44 shape; remains the hot-path choice):
  net.virtio $ net.l4.over-l2 $ net.text $ eosh        ── one task, one NIC claim,
                                                          zero-cost in-task calls

# Shape B — shared, gated (this design):
  owner:   net.virtio $ net.l4.over-l2 $ telnetd       ── one task, one NIC claim;
           telnetd mints a session wrapper per accept     telnetd's own l4 use stays fused
  child:   spawn( net.text $ eosh,
                  grants: [ l4 ← session wrapper ] )   ── N concurrent session tasks,
                                                          each seeing only its connection
```

### 5.2 The broker case: factory exports in the blessed namespace

When the sharer and the consumer are strangers — init wiring a long-lived net-stack service to telnetd's import — the spawner-passes-a-handle move doesn't apply: init possesses no l4 handle of the service's. For exactly this case, and only this case, a component **exports a factory**: a function in the API package's blessed factory namespace that mints a handler on demand.

```wit
// eo9:net/l4-factory — the blessed factory interface, in the API's own package
// (every shareable API gets one sibling: eo9:fs/fs-factory, eo9:disk/disk-factory, …)
interface l4-factory {
    use l4.{l4-impl, l4-error};
    get: func() -> result<l4-impl, l4-error>;
}
```

* The factory returns the API's standard root resource (`l4-impl`) — APIs are resource-rooted per SPEC, so there is nothing factory-specific to consume; the returned handler is granted to the consumer exactly as a spawn-passed one (§3.1), and the kernel calls `get` **once per wiring**, at grant time, as an ordinary gated call into the service's store.
* The factory body is owner code, so the §4 rule carries over verbatim: a factory that returns the raw stack handle shares fully; a factory that mints a wrapper per `get` scopes per consumer. "Policies are programs" — the broker case changes who initiates the wiring, not where policy lives.
* **Nothing exports a factory by accident.** Providers are not universally shareable. *Settled at M1 (owner ruling, area/41 respin):* the blessed factory lives **natively in each l4 provider** — over-l2, filtered, loopback and deny each implement `get()` returning their own root (a deny's factory grants the typed denied vocabulary; a filtered one's grants the filtered view, so policy flows through free). A serving composition is then a plain chain (`net.virtio $ net.l4.over-l2`), and the doctrine line is: **`$` seals — a provider-tailed chain serves; `&` retrofits a factory onto a composition that lacks one.** The standalone `l4.factory` tail component (the earlier spelling here) was built, proved the shape, and was retired by the respin. The decision to share remains a visible, deliberate `share` clause in the boot config — never the export's mere existence.

**The kind ruling: blessing-with-validation, not a new kind.** Exporting a factory does not mint a third component kind alongside binary and provider. The blessed factory namespace is **kind-neutral but validated**: the loader structurally checks any `*-factory` export against the blessed shape (returns the API's root resource, no extra authority smuggled in), refuses malformed ones at load, and `describe` reports a first-class **`serves:`** line (`serves: eo9:net/l4`) so what a component offers to strangers is auditable at a glance — exactly as `describe` already reports kind, imports, exports, and arg signature. A binary may serve (telnetd could export a factory *and* have a `main`); a provider may serve; neither changes kind. If factory-only components proliferate, "service" can return later as a display label over the same validation — a presentation choice, not a kind.

### 5.3 Init's services config and the trust posture

```
# a hosting service: its composition exports the blessed factory; `share` names it
lan    = net.virtio $ net.l4.over-l2 $ l4.factory   share lan=eo9:net/l4 lock=instance   restart restart.always
# a consumer: `use` wires its import to a named share's factory
httpd  = httpd   use l4=lan   restart restart.always
console = eosh
```

(Grammar spelling illustrative; `share`/`use` are line-level clauses like `restart`. `share` names an interface and the compile locates the unique factory export serving it — ambiguity is a config error, refused pre-run.)

Init resolves each `use` edge by having the kernel call the named share's factory at wiring time and granting the returned handler in the consumer's spawn (§5.2). One honest wrinkle, stated rather than hidden: init's services are siblings under the registry, so the owner is not *literally* an ancestor of its grantees. The registry therefore treats every `use` edge as a **supervision edge with subtree-identical semantics**: the owner's death kills all its grantees (then policies apply, restarting the owner before its grantees — a topological restart order over the `use` DAG; cycles are a config error, refused pre-run). For M2, none of this is needed — telnetd spawns its own sessions, literal ancestry — so the registry-edge generalization is specified here but scheduled post-M2.

**Trust posture (settled, and already SPEC text):** services named in the init config get the boot grants — the kernel registry links the ambient kernel roots and the boot-granted operator roots into every service, exactly the set the console session's own children link. Operator-authored services are console-equivalent trust: **"a config line can do exactly what a console line can do"** — same operator, same authority — and the soundness invariant holds in its real form, a service never exceeding what its detacher could run in the foreground. A `use` edge adds nothing to this: the consumer gets one granted handler, which the serving side's factory chose to mint — never the registry's authority, never the owner's other capabilities.

**Shared-by-default: nothing (settled).** No deployment profile — usermode or kernel — ships any `share` line out of the box. The first candidates *worth* a deliberate `share` when an operator opts in: the l4 stack (this design's driver), the future host-unix-l4 provider (plan/09 entry 45's usermode-parity gap), and `eofs` once multiple writers exist.

### 5.4 The degenerate option: in-task sessions

The simplest concurrency shape needs none of this document: **the owner runs its subsidiary work itself, in its own task** — exactly how eosh runs commands today. A telnetd that drives its sessions as in-task concurrent activities (structured concurrency over its fused l4) shares nothing, exports nothing, spawns nothing, and pays zero gate overhead; it gives up per-session task isolation (one session's trap is the task's trap) and per-session resource limits, which is often fine. This remains the first option to reach for; spawn grants exist for when the isolation is wanted, and factories for when the parties are strangers. The design adds capability, not obligation.

---

## 6. Death and teardown

**The supervision tree is the death model — there is nothing else.** The owner is an ancestor (or supervision-superior, §5.3) of every sharer. Owner dies → the subtree dies. No re-introduction protocols, no reconnect states, no generation counters, no handle-revocation protocol: a sharer that could outlive its owner is unrepresentable, which is precisely why handlers only travel downward and detach-with-gates is refused. (This is the decisive argument that killed channels — Appendix A.)

Teardown ordering when the owner dies (kill, trap, or normal exit) — each step ordered by the invariant it protects:

1. **Sever first.** The gate registry marks every grant on the dying owner severed, atomically with the kill marking (same `KLock` critical section as `kill_task_tree`'s slot flip). From this instant no gate call enqueues; parked and in-flight calls complete with the interface's own error vocabulary — `denied`/`io("provider task ended")` where the interface has the arm (every Eo9 net layer does, per SPEC). Severed-before-anything is the only ordering the gate *needs*: the gate holds reps and `Val`s, never store pointers, so no use-after-free is reachable once severance precedes the store drops.
2. **Kill cascade.** The existing machinery, unchanged: `kill_task_tree` walks `PARENTS` to a fixed point (`shellexec.rs:456`); registry `use` edges extend the walk for init-level shares. Children's drive futures drop on next checkout observation, releasing their stores — and with them their gate shims and translation tables (kernel-side plain data; order-independent by step 1).
3. **Owner store drop.** The drive future drops; the store drops; resource destructors run — wrapper handlers' destructors are owner code and run here like any other — and `Drop for PciTables` quiesces the device: the plan/12 D62 quiesce-at-teardown precedent, verbatim — bus-mastering off and rings disarmed *before* DMA buffers free, on completion, trap, **and** kill, with the `pci: quiesced N device(s) at task teardown` line keeping the ordering visible in transcripts. Sequential telnetd (plan/09 entry 45, session 2) already proved NIC quiesce + re-claim across owner lifetimes; this design inherits that proof.
4. **Supervision policy.** The owner's own supervisor (telnetd's parent; init for services) observes the outcome and applies its restart policy — restarting the *whole subtree* fresh: new owner task, new NIC claim, new grants, new sessions. There is never a "reattach."

Caller-side death is §3.6. The remaining case — **owner's `main` returns while gated children live** — is the owner's choice like everything else here: the owner task's drive future completes only when `main` is done **and** no live grants remain (mirroring init's "console exited with services still running" logic); an owner that wants hard-stop semantics kills its children first, which it can, being their ancestor.

---

## 7. The worked example: concurrent telnetd

**Who owns what.** One task — `net.virtio $ net.l4.over-l2 $ telnetd`, spawned by init — owns the NIC claim and hosts the one smoltcp instance. telnetd's *own* l4 use (listen/accept) is the fused import: zero-cost, exactly as today. No factory anywhere: this is pure parent→child sharing (§5.1). Per accepted connection, telnetd mints a **session wrapper** (§4) — its own resource implementing the `l4-impl` contract, closing over the accepted `tcp-connection`, exposing exactly that connection and nothing else — and spawns a session child with the wrapper as its l4 grant. Per-session children are `net.text $ eosh` — **no NIC, no stack, two components** — and `net.text` runs unmodified: its `accept` simply yields the wrapper's pre-bound connection.

```
                 init (svc grant, restart policy)
                   │ spawn
                   ▼
        ┌─────────────────────────────┐
        │ telnetd task (OWNER)        │   one NIC claim, one smoltcp instance
        │  net.virtio ── l2 ──┐       │
        │  net.l4.over-l2 ◄───┘       │   fused; telnetd's own use is in-task
        │  telnetd (accept loop,      │
        │   session-wrapper minting)  │
        └───────┬─────────────┬───────┘
  grant: wrap₃  │             │ grant: wrap₄         … session N
                ▼             ▼
        ┌──────────────┐ ┌──────────────┐
        │ session 1    │ │ session 2    │   children: net.text $ eosh
        │ net.text+eosh│ │ net.text+eosh│   l4 import = kernel call gate
        └──────────────┘ └──────────────┘       to telnetd's wrapper
```

**Accept → spawn → first bytes:**

```
client            telnetd (owner)                 kernel gate            session child
──────            ───────────────                 ───────────            ─────────────
SYN ─────────────► fused l4.accept → conn₃
                   mint wrap₃ = session
                   wrapper over conn₃
                   spawn(net.text $ eosh,
                         grants=[l4 ← wrap₃]) ───► bind grant; wrap₃
                                                   moves into the grant   ── child boots
                                                   table                    net.text: l4.default()
                                                                          ◄─ wrap₃ (child rep)
                                                                          net.text: listen+accept
                                                   gate call: accept on
                                                   wrap₃ ──────────────► owner store: wrapper yields
                                                                         conn₃ (pre-accepted; immediate)
                                                                          ◄─ tcp-connection (child rep)
"hello\n" ───────► (NIC, smoltcp RX buffer)                               net.text: recv(conn, buf)
                                                   gate call recv:
                                                   fast path → owner
                                                   store → wrapper →
                                                   smoltcp has bytes
                                                   → returns ────────────► line to eosh; prompt back
◄──────────────────────────────────────────────── gated send ◄──────────  eosh output via net.text
```

**Two sessions concurrently** — the thing D44 could not do. Both children's `recv` calls sit in-flight inside the one owner store as concurrent calls; the `instance` lock interleaves them per-poll; a parked `recv` (no bytes) holds nothing:

```
drive-loop pass:   poll owner ──► intake: poll recv via wrap₃ → parked on RX
                                          poll recv via wrap₄ → parked on RX
                                          poll telnetd.main → parked in accept
   (NIC RX irq: bytes for conn₄ → owner runnable — an EVENT, never the backstop)
next pass:         poll owner ──► smoltcp ingests frame; recv via wrap₄ completes → cell₄
                   poll session₄ ──► GateCallFuture ready → eosh₄ gets its line
                   poll session₃ ──► still parked; session₃ unaffected
```

**Session death.** Client FIN → gated `recv` answers `none` → eosh exits → child task done → telnetd (its `task.wait` precedent) observes → grant teardown enqueues wrap₃'s owner-side drop → the wrapper's destructor (telnetd's own code) closes conn₃ and — because the owner is alive and scheduled — **the FIN actually gets pumped**, retiring the D44 throwaway-accept close workaround for gated sessions. Kill mid-transfer is §3.6: the in-flight call completes into the void; conn₃ is closed the same way.

**Whole-subtree restart.** telnetd traps or is killed → §6 ordering: gates severed (any racing session call gets `l4-error::denied`-class severance), sessions killed by cascade, NIC quiesced at owner store drop, init's `restart.always` respawns telnetd → fresh claim, fresh stack, port 23 listening again. Clients reconnect; nothing reattaches. (Remote `poweroff` keeps its D44 behavior: it propagates as the *session's* outcome to telnetd, which refuses it by policy — unchanged.)

**The degenerate alternative, for contrast (§5.4):** a telnetd that wanted concurrency without isolation could drive both sessions itself, in-task, over its fused l4 — no wrappers, no spawns, no gates, one trap domain. The gated shape is chosen here because per-session task isolation is the point of the exercise.

---

## 8. Performance notes (honest)

* **Per gate call vs fused:** a fused call is an inlined function call (the canonical-ABI copies optimized out — SPEC "Contract vs cost"); a gate call pays two host-call boundary crossings (caller's + owner's lower/lift of `Val`s), the kernel-side queue/translate bookkeeping, the wrapper's method body where one is interposed (§4), and — off the fast path — up to one drive-loop pass of latency (event-driven; the 10 ms `IDLE_WAKE_INTERVAL_NS` is a backstop, not the cadence, and a gate-induced backstop wake is by doctrine a bug). There are **no measurements yet**; producing the number (gate `recv` vs fused `recv`, QEMU and board) is an M1 exit criterion, and the spawn-trace machinery (`shellexec.rs:1253`) is the measurement precedent to extend.
* **Buffers don't multiply the cost:** `own<buffer>` transfer through the gate is a host-table entry move (§3.2); the dominant data-path cost — guest↔buffer byte copies — is identical in both shapes.
* **Why acceptable:** the gated path's natural grain is socket *operations* (a line, a segment batch), not bytes; per-op overhead in the µs range against ops that already traverse smoltcp + a NIC is in the noise for an interactive shell session — and was previously *impossible* concurrently at any price. (Batch-shaped WIT — one gated `submit/reap` per batch — is how high-IOPS APIs compose with the gate; recorded in the disk-IOPS ladder, not designed here.)
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
| R6 | Gate wakes leak into the backstop (liveness regression — work runnable while the core slept). | M2 transcripts must contain zero `liveness: stranded runnable/input/intx` findings during gate traffic; the detectors already exist (`mod.rs:430-460`) and the scripted gates already print transcripts. |
| R7 | Factory validation is bypassable or breaks the kind judgment (a malformed `*-factory` export loads; `describe` misreports; serving flips a binary's kind). | M1: `describe` on `… $ l4.factory` reports kind=provider-or-binary unchanged + `serves: eo9:net/l4`; a factory whose `get` returns a non-root type refused at load with the validation story; telnetd-with-`main` remains kind=binary regardless of any factory export. |

---

## 10. Milestones (honest sizing)

* **M1 — the call gate for ONE interface (`eo9:net/l4`), QEMU.** Kernel owner-task drive on `run_concurrent` with the intake (the big item — R2 first); gate registry, typed hand-written l4 shims (the `providers.rs` pattern; the generic `func_new`-dynamic gate is deliberately deferred); the `grants` spawn surface (typed for l4 in v1, §3.1); the `l4.factory` component + blessed-namespace validation + `serves:` in `describe`; tests: owner = `net.l4.loopback $ host-test` granting l4 to a child doing connect/send/recv through the gate, a wrapper-handler scoping test, plus R1/R3/R7 negatives, plus the gate-vs-fused measurement. **Sizing: the largest kernel-lane change since component-model-async bring-up itself; ~3 weeks if R2 is clean, +1–2 weeks if the vendored wasmtime needs concurrent-call patches.**
* **M2 — concurrent telnetd, QEMU.** telnetd's session wrapper + spawn-per-accept; net.text verified unmodified against a wrapped grant; `check-telnet-concurrent` scripted gate (R4/R5/R6); gated-session close path replacing the D44 pump trick. **Sizing: ~1–2 weeks on a clean M1; the harness (byte-paced typing, transcripts, kill scripting) all exists.**
* **M3 — board (Orange Pi 5 Plus, `net.rtl8125`).** Same compositions, real silicon: gate traffic under the DW-WDT and fibercompile interleaving (a session compile parked while another session serves — R1's field test), bench-LAN concurrent sessions, D44 security posture restated (cleartext, trusted LAN). **Sizing: ~1 week if M2 is clean; board-lane margin applies; never touches the serial-loader port rules (boards/BOOT.md), never `/dev/cu.usbserial*`.**

Post-M3 ledger (explicitly deferred, not dropped): generic dynamic gate shims + the type-erased generic `grants` surface; `token` lock domains for l4; registry `use` edges for init-level shares; caller-donated fuel; grant-overrides-root-provider per-spawn linkers; subtask-cancellation for orphaned calls; usermode parity (blocked on the host l4 provider exactly as plan/09 entry 45 records); the "service" display label if factory-only components proliferate (§5.2).

---

## 11. Workarounds and assumptions

* **Pinned substrate:** wasmtime 45 (vendored, patched per `kernel/vendor/README.md`); all store-discipline reasoning in §3.3 is against that version's documented behavior (`task.rs`, `spike_cm_async.rs`) and must be re-validated on any vendor bump.
* **M1 shims are hand-written typed registrations** for l4 only — both the gate shims and the `grants` spawn surface; the generic dynamic gate assumes async-dynamic host functions (`func_new`'s concurrent sibling) — unverified on the vendored build, hence deferred.
* **Single boot core assumed** throughout (the `KLock`/checkout machinery's existing assumption); the gate lock is where the multi-core entry rule will land.
* **Detach × gates refused** in v1; init-level `use` edges (supervision-edge generalization) specified but post-M2.
* **Known gaps inherited, not worsened:** the l4 close gap (improved for gated sessions, §7, but the fused-exclusive shape still needs the D44 pump trick); per-task text capture (a session child's own stdout still lands on serial); usermode l4 parity.
* **Security posture unchanged:** telnet remains cleartext/unauthenticated, trusted-LAN/dev only, said loudly everywhere it was said before.

---

## Appendix A — rejected alternatives (one line each)

All rejected in design review; recorded so they are never re-litigated by accident:

* **Channels (the Message-API service design)** — "channels as a synchronization primitive is a code smell": they re-introduce serialization/framing where WIT is already the contract, force OS-level backpressure policy, and let an endpoint outlive its peer — the supervision tree makes every half-dead state unrepresentable instead.
* **A pump/server process (netd)** — a scheduled guest loop that must be live and burning quanta for anyone to progress, with its own restart story interposed on every consumer; the gate serves with zero owner-guest scheduling.
* **Hosting exports / the `eo9-host:` rider prefix** — the kernel reaching past `$`-sealing into a fused component's internals; the spawner passing a resource it *possesses* needs no export surface at all.
* **Kernel scope tables and the ID-token convention** — scoping policy in the kernel duplicates, poorly, what owner wasm expresses exactly; policies are programs, the kernel does routing only.
* **Provider-exported `scope` functions** — same verdict from the provider side: attenuation is a wrapper the owner mints, not API surface every provider must design.
* **A new `service` component kind** — forces exclusivity rulings (telnetd is binary *and* serves) or converts kinds into flag lattices; blessing-with-validation keeps two kinds and adds one validated namespace plus a `serves:` line in `describe`.
* **True handle transfer** (move the live `tcp-connection` between stores) — pinned impossible by the D44 investigation: an accepted connection is live state inside the owner's linear memory, not a host-table entry; the gate moves the *call* to the state instead of the state to the caller.
* **Service mesh / discovery registry** — ambient names are ambient authority; everything reachable-by-name violates possession-is-authority and makes the import list lie.

---

### Critical Files for Implementation

- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/shellexec.rs — the child registry, checkout discipline, spawn path, and kill cascade the gate extends
- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/providers.rs — `KernelState` (grant bindings live here) and the host-shim pattern the gate shims follow
- /Users/wy/code/eo9/crates/eo9-runtime/src/task.rs — the `run_concurrent` drive shape and store-borrow findings the owner-task drive future is built against
- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/fibercompile.rs — the nested-entry / checkout precedent the fast path reuses (and R1's reference behavior)
- /Users/wy/code/eo9/kernel/eo9-kernel/src/wasm/svc.rs — init's service plumbing and restart machinery the `share`/`use` config grammar extends
