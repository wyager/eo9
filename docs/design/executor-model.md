# The executor model: detached programs, services, and single-owner devices

Status: **APPROVED with rulings (2026-06-01); v1 (usermode) in implementation.** The owner
answered the open questions of §8; the rulings below amend the proposal and are what v1
implements. The WIT lives at `wit/svc/svc.wit` (no longer proposed).

## Owner rulings (2026-06-01)

| # | Question | Ruling |
|---|---|---|
| A | Names | **`eo9:svc` + `init`** — "executor" stays a role word. |
| B | Default child grant? | **Explicit grant only.** `detach` is never in the default child environment; the CLI grants it via `--svc` (to the shell) or `eo9 init` (to init and its console). |
| C | Restart policy in v1? | **Required, and it is a policy component** ("policies are programs", SPEC): `detach` takes a restart-policy component; the standard policies ship as the stubs `restart.never`, `restart.always`, `restart.backoff` (configured). The original §2 proposal (no restart in v1, flag-based later) is superseded. |
| D | `exit` at the console once init exists | **Restarts the console; halting is explicit.** In usermode v1: init restarts its console while services are still running and exits when the console exits with none left. A `poweroff`/`shutdown` builtin is the metal (v2) follow-up. |
| E | Usermode service lifetime | **Bound to the root process, as a root-process configuration** — the CLI binds the registry to the `eo9` process; the kernel (v2) binds it to the machine; embedders choose. No host daemon. |

Two design refinements that fell out of ruling C (see also `docs/design/policy-components.md`):

* The policy is **runtime-passed** (a component argument to `detach`) and is instantiated
  per decision — the cold-path binding from the policy-components doc. The registry
  validates it at detach time: it must be a provider exporting `eo9:svc/restart-policy`,
  and it must import nothing (impure policies are refused with `invalid-policy`, naming
  their residuals).
* `failure-history` carries **outcome classes** (`success`/`failure`/`trapped`/`killed`)
  plus rendered detail strings, not full typed payloads: policies decide on classes; the
  payloads stay with the service record for humans.
Owner direction this responds to (2026-06-01): a standardized API for detaching programs and
running them in the background ("a shell command that wants to launch a child and then exit
passes the child off to a service provided by programs like eosh"); a boot-time "executor"
program that runs long-lived services (serial-console eosh, SSH servers, window servers,
daemons) and offers that same API to them; the whole thing user-inspectable. Related ruling:
NICs have a single owner — programs that need link access get either the physical NIC
exclusively or a virtual NIC mapped onto it; most programs should only ever import L4.

Spec/plan references: SPEC "Execution APIs" (fuel donation, environments-as-data, kill and
linearity, schedulers are ordinary programs), SPEC "Composition and the `$` operator"
(substitution → provider; supervision → executor), wit/exec (component-algebra / compile /
task), plan/04 (runtime), plan/12 (kernel boot + child registry), study 08 F7/F9 and study 09
C3 (the findings that motivated this).

---

## 1. What exists today, and the gap

Every child today is **foreground**: eosh composes an environment onto a command, compiles,
spawns, and `wait`s; the kernel's drive loop pumps children while the shell is parked in that
wait; killing a parent cascades to its descendants; when the foreground program exits, the
session (or the machine) is done. Three consequences the studies surfaced:

* a program cannot outlive the command that started it — there is nowhere for an SSH server,
  a window server, or a long-running driver to live (study 09 C3);
* nothing pumps a provider between its consumer's calls — the TCP stack is frozen unless the
  foreground program is mid-call (study 08 F7);
* device sharing is undesigned because there is no entity that could own a device on behalf
  of several programs (study 08 F9).

The gap is **one missing concept**: a child whose lifetime is not tied to its spawner. Eo9
already has everything else — a child registry, a drive loop, kill-cascade, fuel accounting,
composition provenance for inspection.

## 2. The detach API (`eo9:svc`) — "run my child for me"

### Shape (PROPOSED WIT)

```wit
package eo9:svc@0.1.0;

/// Hand a composed child to a longer-lived executor and walk away.
/// This is a CAPABILITY: holding it is the authority to consume machine
/// resources beyond your own lifetime. It is not part of the default child
/// grant; it is granted explicitly (see Open Question B).
interface detach {
    use eo9:exec/component-algebra@0.1.0.{component};
    use eo9:exec/args@0.1.0.{named-arg};

    /// How the service's terminal output is kept.
    enum log-policy {
        /// Keep a bounded in-memory/on-store ring the inspector can read.
        capture,
        /// Discard output (the service is expected to use its own fs/net).
        discard,
    }

    variant detach-error {
        /// The composition still requires capabilities that detach does not
        /// supply, named. Compose them in before detaching (see "capability
        /// soundness" below).
        not-closed(list<string>),
        /// A service with this name already exists.
        name-taken(string),
        /// The executor's service cap is reached.
        exhausted,
        internal(string),
    }

    /// Hand `c` (with `main` args `args`) off to run under the executor's
    /// drive loop, registered under `name`. Returns immediately. The caller
    /// may exit; the service keeps running.
    detach: func(c: component, args: list<named-arg>, name: string,
                 logs: log-policy) -> result<string, detach-error>;
}

/// Inspect and control detached services. Separable from `detach` so that
/// "may look" and "may start things that outlive you" are different grants.
interface services {
    use eo9:exec/task@0.1.0.{program-outcome};

    enum service-state { running, blocked, finished }

    record service-info {
        name: string,
        state: service-state,
        /// The composition tree it was detached with (the wiring view) —
        /// inspection of a running daemon's exact capability set.
        wiring: string,
        /// Present once finished.
        outcome: option<program-outcome>,
        /// Fuel consumed so far (the cost meter).
        fuel-used: u64,
    }

    list:   func() -> list<service-info>;
    status: func(name: string) -> option<service-info>;
    /// Read the captured log (offset/len window).
    log:    func(name: string, offset: u64, max-len: u32) -> option<list<u8>>;
    /// Kill a running service; returns its final outcome. Removing a
    /// finished service's record is `clear`.
    stop:   func(name: string) -> option<program-outcome>;
    clear:  func(name: string) -> bool;
}
```

### Capability soundness — the rule that makes this safe

A detached child runs with **exactly what its detacher composed into it, and nothing of the
executor's**. Concretely, `detach` refuses (typed `not-closed`) any composition whose residual
required imports are not in this short list:

* `eo9:text/text` — satisfied by the executor's **log-capture provider** (that is the
  executor's one legitimate contribution, and it is what makes services inspectable);
* the `eo9:rt/*` runtime-contract riders (diagnostics, configured) — satisfied as on every
  target today.

Everything else — fs, net, entropy, time, exec, pci — must already be sealed into the
composition by the detaching program, using capabilities **it** possesses. The executor never
lends its own environment to a detached child, so handing a child off can never escalate it
beyond what the detacher could have run in the foreground. (A later revision can add an
explicit `detach-with-env(c, args, env: component, …)` where the parent passes an environment
*value* it owns — SPEC already defines environments as passable data — but v1 keeps the
simplest sound rule: close it yourself, then hand it over.)

Two consequences worth stating:

* **Determinism/inspection carries over.** The detached component is a value; its wiring tree
  is recorded; `services.list()` shows the exact capability set of every running daemon. A
  daemon you cannot explain is a daemon that cannot exist.
* **Kill-cascade gets one exception.** Today, killing a task kills its descendants. A
  successful `detach` *reparents* the child to the executor's registry before the caller
  exits, so it is no longer a descendant of its creator. That is the entire semantic delta to
  the existing task model, and it is the point.

### Fuel

Detached services draw fuel from the **root drive loop** (the host's idle loop), exactly as
the kernel's preemption-demo children already do — fuel-sliced, preemptible, Ctrl-C-able
(per-service `stop` rather than the console key). Fuel-conservation accounting still holds:
the root is the source of all fuel; `fuel-used` in `service-info` is the cost meter. A later
supervisor (§4, stage B) can impose budgets per service; v1 does not.

## 3. The boot program (`init`) and the boot pipeline

`init` is an **ordinary Eo9 binary** — no private powers — that is granted `eo9:svc/detach` +
`eo9:svc/services` plus whatever capabilities its boot config needs to compose into services.
Its whole job:

1. read a service list (the boot config),
2. compose each entry (the config is shell-syntax compositions, so the same algebra and the
   same `only`/`configure` forms),
3. `detach` each one,
4. then either exit (v1: the registry keeps the services alive) or stay resident as the
   restart/ordering policy holder (stage B).

```text
# /etc/init.cfg — PROPOSED, shell syntax, one service per line
console  = eosh                                   # serial-console shell (text → real UART)
sshd     = net.virtio $ net.l4.over-l2 $ sshd     # owns the NIC exclusively (see §6)
metrics  = time.monotonic $ fs.eofs $ metricsd
```

**Boot pipeline change (metal):** today `kmain → runner::boot → eosh (foreground) → power
off when it exits`. Proposed: `kmain → runner::boot → init → init detaches `console` (+ the
rest of the config) → the kernel drive loop runs while any service lives → power off when
the service set is empty` (or when a `poweroff` service action says so). The interactive
serial console becomes *a service like any other* — which means `exit` in eosh can mean
"restart the console" (init policy) rather than "halt the machine", and a wedged console can
be stopped/restarted from another service (e.g. over SSH) without rebooting.

When no config exists (baked store, no storedisk), init's built-in default is exactly one
service: `console = eosh`. **Boot behavior is therefore unchanged out of the box.**

**Usermode:** `eo9 shell` keeps services alive for the life of the `eo9` process (there is no
host daemon and should not be one); a new `eo9 init [config]` runs the same init program in
the foreground as the long-lived process — that is how you run "Eo9 as a service host" on a
Unix box. The browser blob: `detach` exists but the page's lifetime is the tab's; services
die with the tab (documented, not worked around).

## 4. Where the registry lives: host first, supervisor later

The owner's vision is that the run-my-child API is "a service provided by programs like
eosh." There is a structural constraint to reconcile with that: a **binary cannot export
interfaces** (binary or provider, never both), and provider instances are per-composition
(fusion shares implementation, never state) — so a shared, long-lived registry cannot today
be a guest program that other programs call into. Cross-task capability traffic is exactly
what the (still-unspecified) **Message API** is for.

Proposal — **two stages, one WIT**:

* **Stage A (v1): the host implements `eo9:svc`**, the same way it implements
  `eo9:exec/task` today. The registry is the existing child registry plus a name table and a
  log buffer; the drive loop, kill-cascade, fuel slicing, and Ctrl-C machinery are reused
  unchanged. `init` and `eosh` are ordinary clients of the API.
* **Stage B (post-Message-API): policy moves into a guest supervisor.** The host keeps only
  the raw mechanics it irreducibly owns (the spec's words: the compiler, timer interrupts,
  the idle loop — i.e. the drive loop and fuel source). Restart policy, dependency ordering,
  per-service budgets, and the registry's *contents* migrate into a resident supervisor
  program reached over the Message API. **`eo9:svc`'s WIT does not change** — the same
  pattern as configure's forwarding→alias+bind migration: the interface is the contract, the
  realization improves underneath it.

This honors "no private powers" where it matters (init/eosh/sshd are all unprivileged
clients; anyone can write a different init) while not blocking the feature on designing the
Message API first.

## 5. Inspectability

* **eosh builtins** (thin wrappers over `eo9:svc/services`):
  `ps` (list: name, state, fuel, one-line wiring summary), `log <name>`, `stop <name>`,
  `start <name> = <expr>` (compose + detach in one form), `clear <name>`.
* **`describe <service>`** renders the full wiring tree the service was detached with — the
  capability audit of a *running* daemon, for free, because compositions carry provenance.
* **Logs** are the captured `eo9:text` output, readable via `services.log` (eosh: `log sshd`)
  and — on metal with a storedisk — persisted under `/services/<name>/log` so they survive
  reboot.
* **Outcomes** of finished services are kept (typed `program-outcome`, same as foreground)
  until `clear`ed; a crashed daemon shows `abnormal(trapped(reason))` with the full panic
  message, same as everywhere else.
* `eo9 ps` (CLI) renders the same view in usermode.

## 6. Worked example: the single-owner NIC

Principle (owner ruling): **a physical NIC has exactly one owner.** Three configurations,
all expressible with existing algebra plus the pieces above:

**(a) Exclusive — works today, stays the simple case.**

```text
sshd's composition:   pci.filtered --allow [nic] $ net.virtio $ net.l4.over-l2 $ sshd
                      └────────────── the whole stack is private to sshd ──────────────┘
```

One service owns pci→driver→stack. No other program can touch the NIC (the pci grant is
explicit and single).

**(b) Shared link — the virtual-NIC switch (v1 of sharing).**

The entity that owns the physical NIC offers *virtual* NICs as a **root provider**, exactly
the way the kernel already owns the physical UART and offers per-task `eo9:text`:

```text
   init's boot config:   nic-owner = pci.filtered --allow [nic] $ net.virtio $ l2.switchd
                                                                       │
              host/registry plumbing: switchd's exported frames ──────┤ (stage A: host-side
                                                                       │  switch; stage B: a
   sshd:        [virtual l2 root] $ net.l4.over-l2 $ sshd  ◄──────────┤  guest switch over
   webd:        [virtual l2 root] $ net.l4.over-l2 $ webd  ◄──────────┘  the Message API)
```

Each consumer's residual `eo9:net/l2` import is satisfied at spawn with its own virtual NIC
(own MAC, own frame queues); the switch muxes/demuxes onto the one real link. In stage A the
switch lives host-side (a root provider, like the UART mux); in stage B it can be the
`l2.switchd` guest service itself, once cross-task channels exist. Programs cannot tell the
difference — they import `l2` either way.

**(c) Most programs: L4 only, no link access at all.**

```text
   client:   net.l4.over-l2 $ client     — composed over a virtual NIC by whoever starts it
   or:       [shared l4 root] $ client   — stage B: one shared TCP/IP stack as a service
```

The owner's "most won't! they should just need L4" is the default posture: ordinary programs
import `eo9:net/l4`, and *which* link/stack that rides on is their composer's decision, not
theirs.

This also answers study 09's "where does a long-running driver live": **a driver that should
outlive a command is just a detached service** (`nic-owner` above), and its device claim
lasts exactly as long as the service does — the existing teardown/quiesce path runs when it
is stopped.

## 7. What changes where

| Piece | Change | Stays the same |
|---|---|---|
| **WIT** | New package `eo9:svc` (detach + services), PROPOSED above. | `eo9:exec` untouched. |
| **SPEC** | A "Services and detachment" subsection under Execution APIs: the reparenting rule (kill-cascade exception), the closed-before-detach rule, the log-capture contract, single-owner devices. One sentence in the Shell section for the new builtins. | Everything else. |
| **Usermode runtime** (plan/04) | Service registry beside the task table; root drive loop keeps pumping while services live; text-capture provider; `eo9 ps`/`eo9 init`. | Task/compile/fuel machinery. |
| **Kernel** (plan/12) | Boot runs `init` instead of eosh directly (default config = eosh, so observable behavior unchanged); registry/log storage (storedisk-backed when present); drive loop keeps running after the console exits while services live. | Child registry, drive loop, kill-cascade, Ctrl-C, quiesce — all reused. |
| **eosh** (plan/10) | `ps`/`log`/`stop`/`start`/`clear` builtins; `start name = expr` syntax. | Foreground `$`/`&`/`only`/`let`/`save` unchanged. |
| **Browser** (plan/18) | `eo9:svc` registered; services bounded by tab lifetime (documented). | Everything else. |
| **init** (new, plan/17 or its own area) | A ~small ordinary guest program: parse config, compose, detach. | — |
| **Net** (plan/09) | Stage A switch = host root provider; `l2.switchd` guest version is stage B. | Drivers, l4 middleware. |

**Staged plan**

* **v1 (usermode):** `eo9:svc` WIT + host registry + log capture + eosh builtins + `eo9 init`.
  Acceptance: start a service from eosh, exit eosh, `eo9 ps` shows it running, `log`/`stop`
  work; a detach with unsatisfied imports is refused with the typed error.
* **v2 (metal):** kernel registry + boot-runs-init + storedisk-persisted logs/config.
  Acceptance: boot with a config that starts `console` + a daemon; the daemon survives the
  console exiting and restarting; `ps` from the console shows both; reboot → config re-applied.
* **v3 (sharing):** the host-side l2 switch root provider + per-service virtual NICs.
  Acceptance: two services with sockets through one physical NIC, each with its own MAC/IP.
* **v4 (post-Message-API):** supervisor program + guest switch; registry policy migrates;
  WIT unchanged.

## 8. Open questions for the owner

| # | Question | Options | Recommendation |
|---|---|---|---|
| A | **Names.** The API package and the boot program. Note: SPEC already uses "executor" as a *role* word (interpreting/native executor), so naming the program "executor" overloads it. | (1) package `eo9:svc`, program `init` · (2) package `eo9:exec/services`, program `executor` · (3) other | (1) — `init` is universally understood for the boot role; `eo9:svc` keeps detach separable from the exec grant |
| B | **Is `detach` part of the default child grant?** i.e. can anything eosh runs daemonize, or only programs explicitly granted it? | (1) in the default grant · (2) explicit grant only | (2) — outliving your creator is real authority (it consumes the machine after you are gone); `only`-style explicitness is the Eo9 way. eosh itself holds it; commands get it when granted |
| C | **Restart policy in v1?** | (1) none — a finished/crashed service just sits inspectable until cleared · (2) a `restart: always` flag per service | (1) — restart policy is precisely what should live in the stage-B supervisor; doing it host-side first means migrating it later |
| D | **`exit` at the metal console once init exists** | (1) restarts the console service (machine keeps running) · (2) keeps meaning power-off | (1), with `poweroff` as an explicit eosh builtin / init action — "exiting the shell" and "halting the machine" are different intents |
| E | **Usermode service lifetime** | (1) bound to the `eo9` process (services die when it exits) · (2) a detached host daemon | (1) — a host daemon is un-Eo9-like and a packaging/ops burden; `eo9 init` in tmux/systemd is the Unix-native answer |
