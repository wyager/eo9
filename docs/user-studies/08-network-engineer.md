# User study 08 — network/systems engineer

## Session metadata

- **Date:** 2026-05-31
- **Branch / worktree:** `docs/study-08` (worktree of master at `5985249`)
- **Participant persona:** a network/systems engineer (~12 years: kernel network stacks,
  embedded TCP/IP, datacenter ops; fluent in C/Rust, lives in tcpdump and `ip route`; no
  WebAssembly background). Cares about: where the NIC driver lives, what the TCP stack is,
  how layers are mocked in tests, performance, MTU, IPv6.
- **Methodology:** the participant was a role-played persona run as a separate session with
  no access to the repository, its documentation, or any tools — it saw only what the
  facilitator pasted into the conversation and replied conversationally. Every command shown
  to the participant was actually executed by the facilitator in the study environment;
  outputs are verbatim, trimmed only for length. Failures and breakage were shown as they
  happened, not cleaned up.
- **Environment:** release build of the `eo9` CLI from this checkout on an Apple Silicon
  macOS host; a throwaway store (`--store` pointed inside the worktree, seeded with the 50
  bundled components on first run); for the bare-metal segment, the aarch64 kernel image
  built from the same checkout, run under `qemu-system-aarch64 -M virt` with
  `cargo xtask qemu aarch64 pci net` (a modern virtio-net PCI function backed by QEMU
  user-mode networking).
- **Shape:** Round 1 — the pitch, the layered WIT design, and the usermode demos; the
  participant's first reactions. Round 2 — answers + the bare-metal demos (the real NIC
  driver and the three-layer composed stack); reactions. Round 3 — verdict and structured
  wrap-up.

## Round 1 — the pitch, the WIT, and the usermode demos

### The pitch given

Eo9 is a capability-secure OS on the WebAssembly Component Model. A program's imports are
its permissions; granting authority is a composition algebra (`provider $ program`); deny
by default. Networking is **not one capability** — it is three:

- `eo9:net/l2` — link layer: interfaces, MAC addresses, whole Ethernet frames.
- `eo9:net/l3` — network layer: IP addresses, routes, raw IP datagrams.
- `eo9:net/l4` — transport layer: TCP connections/listeners and UDP sockets.

Each layer is its own capability with its own root handle. A fetcher imports only `l4`; a
DHCP client only `l2`; a ping tool only `l3`. Any layer can be granted, withheld, or mocked
on its own. A TCP/IP stack is not OS magic — it is *ordinary middleware in the algebra*: a
component that imports `l2` and exports `l4`. The NIC driver is also a component: it
imports `eo9:pci` and exports `l2`.

### The WIT (shown to the participant, key excerpts)

From `wit/net/net.wit` — the layer split and the deliberate type isolation:

```
/// eo9:net — networking, split into independently grantable layers:
///
///   * `l2` — link layer: network interfaces, MAC addresses, whole Ethernet frames.
///   * `l3` — network layer: IP addresses, routes, raw IP datagrams (per protocol).
///   * `l4` — transport layer: TCP connections/listeners and UDP sockets.
///
/// Each layer is its own capability with its own root handle, so a program imports
/// exactly the level it speaks (a fetcher imports only `l4`, a DHCP client only `l2`,
/// a ping tool only `l3`), any single layer can be granted, withheld, or mocked on its
/// own, and a higher layer implemented over a lower one (an `l4`-over-`l3` TCP/IP
/// stack, an `l3`-over-`l2` IP layer) is ordinary provider middleware in the algebra,
/// not OS magic. The layers deliberately share no types: each is self-contained, so a
/// pure-`l4` world never even names link- or network-layer concepts.
```

`l2` (abridged): `resource l2-impl` (the root handle), `mac-address`, `interface-info
{ name, mac, mtu, up }`, `l2-error { denied, no-such-interface, link-down,
frame-too-large, io(string) }`, `resource l2-interface`, and async `list-interfaces`,
`open-interface`, `send-frame`, `recv-frame` — whole frames, owned-buffer round-trip.

`l3` (abridged): `ip-address { v4(...), v6(...) }`, `ip-prefix` (CIDR), `route
{ destination, gateway, interface-name }`, `l3-error { denied, no-route, unreachable,
address-unavailable, protocol-unsupported, packet-too-large, io(string) }`,
`resource raw-socket` bound to one IP protocol number, async `addresses`, `routes`,
`open-raw`, `send-datagram`, `recv-datagram`. The provider owns IP header
construction/parsing — payloads cross the interface, never raw headers.

`l4` (abridged): `socket-address { address, port }`, `l4-error { denied, unreachable,
connection-refused, connection-reset, timed-out, address-in-use, address-unavailable,
not-connected, message-too-large, io(string) }`, resources `tcp-connection`,
`tcp-listener`, `udp-socket`, and async `connect`, `listen`, `accept`,
`listener-address`, `peer-address`, `send`, `recv`, `bind-udp`, `udp-address`,
`send-to`, `recv-from`.

Per layer there are stub worlds: `net.lN.none` (absence — exports the optional flavor
answering `none`), `net.lN.deny` (refusal — every operation fails `denied`), plus
`net.l4.loopback` (a self-contained in-memory transport: TCP and UDP work between sockets
created through this provider instance, nothing outside is reachable — the standard
test/mock L4, needs no lower layers at all), and the TCP/IP middleware's configuration
interface:

```
/// Compose-time configuration entry for net.l4.over-l2 (the TCP/IP middleware): binds the
/// static IPv4 addressing the stack uses on its link. Addresses are dotted-quad strings
/// (`"10.0.2.15"`); a malformed address is a configure-time error, never a trap. An
/// unconfigured provider keeps its documented default — QEMU user-mode networking's
/// layout (10.0.2.15/24, gateway 10.0.2.2) — so plain composition still works.
interface l4-over-l2-config {
    use l4.{l4-impl};
    configure: func(address: string, prefix-length: u8, gateway: string)
        -> result<l4-impl, string>;
}
```

The two real network components in the tree (both wasm components, both in `guest/`):

- **`net.virtio`** — the NIC driver: a virtio-net driver written in Rust (909 lines,
  no_std), compiled to a wasm component. Imports `eo9:pci/pci` (config space, BARs, DMA
  via `alloc-dma`, polled virtqueues), exports `eo9:net/l2`.
- **`net.l4.over-l2`** — the TCP/IP stack: smoltcp 0.12 wrapped as middleware (1100 lines
  of glue). Imports `eo9:net/l2` + `eo9:time/time` (TCP retransmission/ARP timers) +
  `eo9:entropy/entropy` (ephemeral ports, TCP ISNs), exports `eo9:net/l4`.

### Usermode demos (every command run for real)

The environment: `cargo xtask build-guest`, `cargo build --release -p eo9`, a throwaway
store. First run seeds 50 bundled components, including the whole net family
(`net.l2.none/.deny`, `net.l3.none/.deny`, `net.l4.none/.deny`, `net.l4.loopback`,
`net.l4.over-l2`, `net.virtio`) and the example programs (`sockcheck`, `l2check`,
`l4check`).

**Demo 1 — what the transport-layer test program imports:**

```
$ eo9 describe sockcheck
component: sockcheck (store object 9bb0f779…)
kind: binary
imports:
  eo9:io/buffers@0.1.0 (required)
  eo9:net/l4@0.1.0 (required)
  eo9:rt/diagnostics@0.1.0 (required)
exports:
  (none)
main arguments:
  --payload <string>
```

`sockcheck` speaks only `l4`. It never names a frame, a MAC, or an IP route. (What it
does: ephemeral-port listen, duplicate-bind must fail `address-in-use`, connect to a dead
port must fail `connection-refused`, two connections queued on the backlog before either
is accepted (FIFO order, streams not crossed), bidirectional TCP echo, then a UDP
round-trip between two ephemeral sockets — a small conformance test any l4 provider must
pass.)

**Demo 2 — deny by default (and a wart):** running it with no network granted:

```
$ eo9 sockcheck --payload "ping eo9"
eo9: error: cannot spawn sockcheck (store object 9bb0f779…): spawn failed: component
imports instance `eo9:net/l4@0.1.0`, but a matching implementation was not found in the
linker: instance export `tcp-listener` has the wrong type: resource implementation is
missing (this component may have been built for an older eo9 — try `eo9 store reseed`)
exit=3
```

Refused before it runs (exit 3, correct) — but the message is a raw linker error and the
advice (`store reseed`) is wrong; the actual fix is composing an l4 provider. (The same
situation for a *filesystem*-needing program prints a friendly "pass `--fs-root <dir>`"
message. Net never got that treatment.)

**Demo 3 — the in-memory transport (mock L4) end to end:**

```
$ time eo9 -c 'net.l4.loopback $ sockcheck --payload "ping from the study"'
ok: echoed(80)
0.27s total (cold compile)        # second run: 0.14s (cache hit)
exit=0
```

All of sockcheck's TCP listen/accept/backlog/echo + UDP runs against the in-memory
transport. No host sockets exist anywhere; nothing outside the provider instance is
reachable. `echoed(80)` = 4×payload + 4 bytes of leg prefixes verified intact.

**Demo 4 — the middleware's shape:**

```
$ eo9 describe net.l4.over-l2
component: net.l4.over-l2 (store object 1d0846be…)
kind: provider
imports:
  eo9:io/buffers@0.1.0 (required)
  eo9:net/l2@0.1.0 (required)
  eo9:time/types@0.1.0 (required)
  eo9:time/time@0.1.0 (required)
  eo9:entropy/types@0.1.0 (required)
  eo9:entropy/entropy@0.1.0 (required)
  eo9:rt/diagnostics@0.1.0 (required)
exports:
  eo9:net/l4@0.1.0
  eo9:net/l4-over-l2-config@0.1.0
configure arguments:
  --address <string>
  --prefix-length <u8>
  --gateway <string>
```

The TCP/IP stack is a value in the algebra: it *imports* a link layer and a clock and
entropy, *exports* transport sockets plus its own configuration interface.

**Demo 5 — typed denial through the layered stack (no trap):**

```
$ time eo9 -c 'net.l2.deny $ net.l4.over-l2 $ sockcheck --payload "ping"'
error: net("L4Error::Denied")
0.23s total
exit=1

$ eo9 -c 'net.l4.deny $ sockcheck --payload "ping"'
error: net("L4Error::Denied")
exit=1
```

Denying the *link* layer underneath the TCP/IP middleware surfaces as the program's own
typed failure at the *transport* layer — sockcheck's first listen gets `denied`, smoltcp
never sees a frame, nothing traps. Denying l4 directly looks identical to the program.

**Demo 6 — the wiring is inspectable:**

```
$ eo9 describe --wiring net.l2.deny net.l4.over-l2 sockcheck
wiring:
$ compose (provider satisfies: eo9:net/l2)
  provider: net.l2.deny [provider]  exports: eo9:net/l2, eo9:net/l2-deny-config  imports: eo9:rt/diagnostics
  consumer: $ compose (provider satisfies: eo9:net/l4)
    provider: net.l4.over-l2 [provider]  exports: eo9:net/l4, eo9:net/l4-over-l2-config  imports: eo9:io/buffers, eo9:net/l2, eo9:time/time, eo9:entropy/entropy, eo9:rt/diagnostics
    consumer: sockcheck [binary]  imports: eo9:io/buffers, eo9:net/l4, eo9:rt/diagnostics
```

**Demo 7 — `env`: how the session treats each import (and two warts):**

```
$ eo9 -c 'env sockcheck'
imports, as this session treats them:
  required eo9:io/buffers@0.1.0 — always available (carries no authority)
  required eo9:net/l4@0.1.0 — missing — would be refused at spawn; compose a provider (e.g. `net.none $ …`) or grant `net` to the session
  required eo9:rt/diagnostics@0.1.0 — missing — would be refused at spawn; compose a provider (e.g. `rt.none $ …`) or grant `rt` to the session
```

Two problems, told to the participant straight: the suggested `net.none` does not exist
(the per-layer split renamed it `net.l4.none`), and the `eo9:rt/diagnostics` line is
wrong — the runtime always links the diagnostics sink, the program runs fine (Demo 3),
and there is no `rt.none` to compose.

**Demo 8 — `only` attenuation on net (compose-time refusal naming the import):**

```
$ eo9 -c 'only eo9:io/buffers,eo9:rt/diagnostics $ sockcheck --payload "ping"'
error: `only` refused: the program still requires eo9:net/l4@0.1.0, which the allow-list
does not include (allow it, compose a provider for it, or drop the requirement)
exit=3

$ eo9 -c 'only eo9:io,eo9:rt,eo9:net $ net.l4.loopback $ sockcheck --payload "ping"'
ok: echoed(20)
exit=0

$ eo9 -c 'only eo9:io,eo9:rt,eo9:time,eo9:entropy $ net.l4.over-l2 $ sockcheck --payload "ping"'
error: `only` refused: the program still requires eo9:net/l2@0.1.0, which the allow-list
does not include (allow it, compose a provider for it, or drop the requirement)
exit=3
```

The third one is the interesting one: the middleware satisfied the program's `l4`, so the
*composition's* residual requirement is the middleware's `l2` — and `only` refuses on
exactly that, before anything runs.

**Demo 9 — configuring the TCP/IP stack's addressing (bakes into the artifact):**

```
$ eo9 -c 'net.l2.deny $ net.l4.over-l2 --address 192.168.7.2 --prefix-length 24 --gateway 192.168.7.1 $ sockcheck --payload "ping"'
error: net("L4Error::Denied")
exit=1
```

The static IPv4 addressing is a compose-time decision baked into the artifact (the config
interface is sealed away in the result); the configured chain still refuses typed over a
denied link.

**Demo 9b — the wart found immediately after:** a malformed address:

```
$ eo9 -c 'net.l2.deny $ net.l4.over-l2 --address not-an-ip --prefix-length 24 --gateway 192.168.7.1 $ sockcheck --payload "ping"'
error: spawn failed: compose-time configuration (`bind`) failed: error while executing at wasm backtrace:
    0:  0x3d8e3 - <unknown>!<wasm function 2>: wasm trap: wasm `unreachable` instruction executed
exit=3
```

The WIT contract for this interface says, verbatim: "a malformed address is a
configure-time error, never a trap." It traps, with an unsymbolized backtrace.

**Demo 10 — a DNS client over the loopback-only transport:**

```
$ eo9 -c 'net.l4.loopback $ l4check'
error: net("L4Error::Unreachable")
exit=1
```

`l4check` (a DNS resolver test that wants to reach 10.0.2.3) over the in-memory transport
gets a typed `unreachable` — not a hang, not a trap.

**Demo 11 — what usermode can NOT do (told straight):** there is no host net root
provider in usermode — no provider wraps the host's sockets — and the real NIC driver
needs the PCI capability, which usermode never grants:

```
$ eo9 -c 'net.virtio $ l2check'
error: spawn failed: component imports instance `eo9:pci/types@0.1.0`, but a matching
implementation was not found in the linker: instance export `pci-impl` has the wrong
type: resource implementation is missing
exit=3
```

So in usermode today: mocks, deny/none stubs, the loopback transport, and the full
middleware over mock links — but **no packet ever leaves the machine**. Real networking
is bare-metal only (Round 2). Also a raw linker error again.

**Demo 12 — the l3 layer:** only `net.l3.none` and `net.l3.deny` exist. No real l3
provider, no l3-over-l2 middleware, no ICMP/ping example. The layer is specified
(addresses, routes, raw sockets per protocol) but nothing implements or consumes it.

### Participant reactions (round 1), condensed, their words where quoted

- Opening: "I came in expecting to roll my eyes at 'OS on WebAssembly' and I'm not rolling my
  eyes."
- **Best thing shown (their ranking): the `only` residual-requirement refusal (Demo 8).** "The
  middleware ate the program's `l4`, so the thing that leaks out the bottom is the
  middleware's `l2`, and your tooling computes that and names it before anything runs. I have
  spent real chunks of my life trying to answer 'what does this container/VM/box actually
  need to reach' with iptables counters and tcpdump after the fact. You're answering it
  statically."
- **Second best: the layered denial (Demo 5).** "Error propagation across layered abstractions
  is where this stuff usually falls apart. That worked." Also approved of the TCP/IP stack as
  ordinary middleware ("the stack is *always* really a library that someone pretends is part
  of the OS — you stopped pretending") and of choosing smoltcp over writing one.
- **Biggest objection: "You demoed a networking system in which no packet exists."**
  Everything in round 1 was the algebra over mocks; the pitch is about networking; "judgment
  reserved until Round 2."
- **The configure trap (Demo 9b) "is the one that would page me."** "Your own WIT says,
  verbatim, 'a malformed address is a configure-time error, never a trap.' It traps… What it
  tells me is not [small]: your interface contracts are documentation, not enforcement. So now
  when I read the rest of the WIT… I have to ask which of those are also aspirational."
- **"Your tooling lies."** The wrong `store reseed` advice, the nonexistent `net.none`
  suggestion, the wrong rt/diagnostics claim: "An error message that confidently gives wrong
  advice is worse than a raw error, because someone at 3am will *follow it*."
- **"L3 is vapor, and that undermines the layering claim."** The one real data point jumps
  l2→l4 in a single component. "'The layers compose' is currently a statement about your type
  system, not your system."
- **Their four hard questions for round 2:** (1) *Who pumps smoltcp?* — if the middleware only
  runs inside l4 calls, "TCP is broken in ways that won't show up in an echo test"; show a
  transfer surviving a 10-second application sleep. (2) *Per-program stack instances vs one
  NIC* — two composed programs means two stacks behind one NIC, both 10.0.2.15; "who muxes
  frames?… This collision is where capability OSes historically get ugly." (3) *How does DHCP
  meet compose-time config?* — "there's no path from runtime-learned addressing into a sealed
  compose-time config. As specified, this architecture supports static addressing only."
  (4) *What does a frame cost?* — per-packet boundary crossings, owned-buffer copies, no
  batching; "I want a number at 1G. Even a rough one."
- Smaller asks: congestion control, socket buffer sizes, what lives in `io(string)`, whether
  v6 is plumbed anywhere, polled-virtqueue cadence and RX-overflow behavior.
- **What would convince them in round 2:** the same `sockcheck` binary passing unmodified on
  metal ("that's your mock-fidelity proof"); ARP and a TCP handshake visible from both ends
  ("tcpdump on the far side, not just your program's stdout"); the sleep-mid-transfer test;
  the 9b trap fixed or a failing test for it.

## Round 2 — answers, bare metal, and the packet capture

### Answers to the round-1 questions (from source, given to the participant straight)

- **Who pumps smoltcp? Nobody, in the background.** The middleware advances the stack only
  inside l4 calls: each operation loops "poll smoltcp → flush queued frames to l2 → pull up
  to 4 frames from l2 → poll again" until the operation completes or its deadline passes
  (constants in `guest/stubs/net-l4-over-l2/src/lib.rs`: 4 s receive, 6 s connect, 1.5 s
  send-flush, max 4096 pump rounds). Between l4 calls the stack is frozen — no ACKs, no
  retransmissions, no timers. A program that sleeps mid-stream leaves the peer talking to
  nobody. There is no scheduler thread; the execution model is run-to-completion (a known
  gap). The sleep-mid-transfer test the participant asked for cannot be shown: no program in
  the tree does it.
- **Per-program stack instances vs one NIC: unsolved, masked by scope.** Every composition
  instantiates its own middleware and driver; two networked programs would both claim the
  same virtio-net function — no mux, no arbitration, no netd-equivalent (GAPS: "no
  machine-global device claiming"). Today the metal executor runs one program to completion
  at a time, so the collision cannot happen yet — by accident of scope, not by design.
- **DHCP vs compose-time config: static IPv4 only, today.** No DHCP client exists.
  Architecturally a middleware *may* learn its addressing at runtime over its l2 import (the
  sealed configure is a static override, not the only possible source), but that is what the
  architecture permits, not what exists. The repo has a parked decision called "compose-time
  vs run-time provider parameters."
- **What does a frame cost? Nobody knows.** No iperf, no pps number; the gaps doc itself
  lists the missing compose/compile/run timing split. From source: one owned-buffer
  round-trip per frame (no batching in the WIT), DMA buffer → l2 buffer → middleware queue →
  socket buffer copies, receive queue 32 frames, receive batch 4/pump, TCP buffers 16 KiB
  fixed per direction, UDP 8 rx / 4 tx slots, max 16 sockets per stack instance.
- **Smaller answers:** congestion control = smoltcp 0.12's default, not configured or
  exposed; `io(string)` = debug-formatted Rust strings from the layer below; **IPv6 = WIT
  types only** — the middleware parses dotted-quad only and returns `unreachable` for any v6
  destination; polled virtqueues = 16-entry rings, 8×2048-byte posted receive slots, polling
  only inside `send-frame`/`recv-frame` with a bounded spin (receive bound: 2,000,000
  host-call iterations before reporting "nothing"), device drops frames when the consumer is
  slow.
- **The round-2 asks dispositioned:** tcpdump-on-the-far-side — done (below);
  sockcheck-unmodified-on-metal — **impossible today, sockcheck is not in the kernel's baked
  store** (logged as a finding); sleep-mid-transfer — no such program exists; fixing the
  configure trap — facilitator is not the maintainer, goes in the findings as fix-now.

### The bare-metal session

Environment: `cargo xtask qemu aarch64 pci net` — QEMU `virt`, GICv2, 512 MiB, a modern
virtio-net PCI function backed by QEMU user-mode networking (slirp), serial on stdio,
input scripted through an expect driver that timestamps every prompt round-trip. The `pci`
boot token is the opt-in grant that lets compositions claim PCI devices.

**Boot:** 12.5 s from `cargo xtask qemu` to the `eosh>` prompt (includes xtask's
kernel-build check and QEMU start). The kernel banner reports a 22-component baked store —
including `net.virtio`, `net.l4.over-l2`, `l2check`, `l4check` — and W^X heap, and that
compositions are "fused and compiled on-target."

**Metal demo 1 — `env` at the metal prompt (and a wart):**

```
eosh> env
capabilities granted to this shell:
  text     PL011 serial console
  fs       the baked-in read-only store image (program names under /bin)
  exec     spawn programs as children
programs started from this shell receive:
  text     PL011 serial console (shared with the shell)
  time     generic timer + PL031 RTC
  entropy  counter-seeded splitmix64 (a stub, not a CSPRNG)
  fs       the same read-only store image view (programs under /bin, /session)
  exec     spawn programs as children (the full session environment is inherited, every generation)
  ...
```

The wart: this boot was started with the `pci` grant (the kernel command line says
`cmdline: pci`, and the very next demo proves children can claim the NIC through it) —
but `env` does not mention pci at all. The one capability that makes this boot different
from a default boot is invisible in the capability report.

**Metal demo 2 — the NIC driver is a wasm component in /bin:**

```
eosh> ls /bin
… net.virtio.wasm  l2check.wasm  net.l4.over-l2.wasm  l4check.wasm …   (22 entries)
ok: listed(22)

eosh> describe net.virtio
kind: provider
imports:
  required eo9:pci/pci (eo9:pci/pci@0.1.0)
  required eo9:text/text … eo9:io/buffers … eo9:rt/diagnostics …
exports:
  eo9:net/l2 (eo9:net/l2@0.1.0)
```

**Metal demo 3 — the ARP round-trip through the wasm NIC driver:**

```
eosh> net.virtio $ l2check
net.virtio: virtio-net 52:54:00:12:34:56, queues rx/tx 16/16
l2check: interface virtio0 (52:54:00:12:34:56, mtu 1500)
l2check: 10.0.2.2 is at 52:55:0a:00:02:02
ok: resolved("52:55:0a:00:02:02")
        [9.8 s prompt-to-prompt: compose + on-target Cranelift compile + run]
```

`l2check` (a program that imports only `l2` + text) broadcast an ARP request for the
gateway and got the reply, through a virtio-net driver that is itself a wasm component
(909 lines of no_std Rust): config space, BARs, DMA, virtqueues — all via the `eo9:pci`
capability, compiled to native code *on the machine* by the kernel's own Cranelift.

**Metal demo 4 — real DNS through three composed wasm layers:**

```
eosh> net.virtio $ net.l4.over-l2 $ l4check
net.virtio: virtio-net 52:54:00:12:34:56, queues rx/tx 16/16
ok: resolved("example.com is 172.66.147.243; tcp 10.0.2.2:9 -> L4Error::TimedOut")
        [47.6 s prompt-to-prompt]
```

The whole path: `l4check` (imports only `l4`) → `net.l4.over-l2` (smoltcp, imports `l2`)
→ `net.virtio` (imports `pci`) → a real virtio-net device → QEMU slirp → the real
internet. The DNS answer is a real Cloudflare A record (it changed across runs:
172.66.147.243, 104.20.23.154 — real round-robin). The TCP probe to the gateway's discard
port surfaces as a typed `L4Error::TimedOut`, not a hang. The composition was fused and
compiled on-target.

The number that hurts: **47.6 s**, nearly all of it on-target Cranelift compilation of the
three-layer composition. Re-running the *identical* composition in the same boot:
**45.6 s** — the compile cache does not hit for fused compositions (re-confirming user
study 03's finding #13). The variance across three runs was 47.6 / 61.7 / 45.6 s.

**Metal demo 5 — typed denial on metal: BLOCKED (a docs overclaim found live):**

```
eosh> net.l2.deny $ net.l4.over-l2 $ l4check
error: cannot resolve `net.l2.deny` (/bin/net.l2.deny.wasm): FsError::NotFound
```

STATUS.md says: "with `net.l2.deny` underneath the same program gets a typed denial in
under a second" — in the bullet about *metal*. But `net.l2.deny` is not in the kernel's
baked store (it never was; checked back to the commit that landed the metal sockets demo).
The typed-denial-on-metal claim cannot be reproduced at the metal prompt. (It does work in
usermode — Round 1 Demo 5 — and in the integration suite via the library API.) Also: the
failed resolve renders as raw `FsError::NotFound` debug text.

**Metal demo 6 — `only` attenuation on metal:**

```
eosh> only eo9:io,eo9:rt,eo9:text,eo9:time,eo9:entropy $ net.virtio $ net.l4.over-l2 $ l4check
error: `only` failed: required imports outside the allow-list: eo9:pci/pci@0.1.0
```

Compose-time refusal naming exactly the residual import — the NIC driver's PCI
requirement. (Wording inconsistency vs usermode noted: "`only` failed: required imports
outside the allow-list: …" here vs "`only` refused: the program still requires …, which
the allow-list does not include (allow it, compose a provider for it, or drop the
requirement)" there.)

**Metal demo 7 — configured static addressing, on metal, compiled on-target:**

```
eosh> net.virtio $ net.l4.over-l2 --address 10.0.2.15 --prefix-length 24 --gateway 10.0.2.2 $ l4check
net.virtio: virtio-net 52:54:00:12:34:56, queues rx/tx 16/16
ok: resolved("example.com is 172.66.147.243; tcp 10.0.2.2:9 -> L4Error::TimedOut")
        [41.2 s prompt-to-prompt]
```

The static IPv4 addressing was passed as configure flags at the metal prompt, baked at
compose time, compiled on-target, and the stack came up on it.

**Session end:** `exit` → `eosh: session ended, outcome = ok(exited)` → PSCI SYSTEM_OFF →
QEMU exits cleanly.

**Facilitator note on the scripted console:** in the first metal session, the command sent
right after the 47-second compile lost most of its characters (the serial echo shows
`net` and nothing else; the shell then waited forever for the rest of the line). This is
the documented plan/12 D49 pacing issue, but it is also a real operational papercut: the
input that gets dropped is exactly the input typed while/right after the machine is busy
compiling. The second session used 1-character-per-40 ms pacing and 5 s settles and hit no
drops.

### The packet capture (the participant's "tcpdump on the far side")

A third boot, identical to `cargo xtask qemu aarch64 pci net` except QEMU was invoked
directly with `-object filter-dump,id=f0,netdev=eo9net,file=….pcap` added (xtask has no
capture option — facilitator observation), running `net.virtio $ l2check` then
`net.virtio $ net.l4.over-l2 $ l4check`. Every frame between the virtio NIC and the world,
read back with `tcpdump -r … -nn -e -tttt`:

```
05:53:11.766990 52:54:00:12:34:56 > ff:ff:ff:ff:ff:ff  ARP Request who-has 10.0.2.2 tell 10.0.2.15
05:53:11.767018 52:55:0a:00:02:02 > 52:54:00:12:34:56  ARP Reply 10.0.2.2 is-at 52:55:0a:00:02:02
   [l2check's run: request → reply in 28 µs]

05:53:45.415302 52:54:00:12:34:56 > ff:ff:ff:ff:ff:ff  ARP Request who-has 10.0.2.3 tell 10.0.2.15
05:53:45.415312 52:55:0a:00:02:03 > 52:54:00:12:34:56  ARP Reply 10.0.2.3 is-at 52:55:0a:00:02:03
05:53:52.198282 10.0.2.15.63851 > 10.0.2.3.53:  3593+ A? example.com. (29)
05:53:52.213992 10.0.2.3.53 > 10.0.2.15.63851:  3593 2/0/0 A 104.20.23.154, A 172.66.147.243 (61)
05:53:58.892974 52:54:00:12:34:56 > ff:ff:ff:ff:ff:ff  ARP Request who-has 10.0.2.2 tell 10.0.2.15
05:53:58.893006 52:55:0a:00:02:02 > 52:54:00:12:34:56  ARP Reply 10.0.2.2 is-at 52:55:0a:00:02:02
05:54:05.632975 10.0.2.15.63852 > 10.0.2.2.9: Flags [S], seq 3776827187, win 16384,
                options [mss 1446,wscale 0,sackOK,eol], length 0
05:54:05.633202 10.0.2.2.9 > 10.0.2.15.63852: Flags [R.], seq 0, ack 3776827188, win 0, length 0
   [l4check's run through the smoltcp middleware]
```

**The good news:** the wire behavior is clean. Proper ARP, a well-formed DNS query from an
ephemeral port, a real SYN with sane options (MSS 1446, window 16384 — matching the 16 KiB
socket buffer — wscale 0, SACK permitted). Three component boundaries don't mangle the
protocol.

**Wire-visible bug 1 — every fresh ARP resolution stalls ~6.7 seconds.** The ARP reply for
10.0.2.3 arrives 10 µs after the request — but the DNS query that was waiting on it doesn't
go out for another 6.78 s. Same again for the gateway: reply at :58.893, SYN at :05.633,
6.74 s later. The wire answers in microseconds; the stack sits on the answer for ~6.7
seconds. Mechanism (from source): the driver's `recv-frame` with nothing waiting spins its
full 2-million-host-call bound (~1.7 s) before reporting "nothing"; the middleware's pump
round does up to 4 of those back to back; and the packet waiting on the ARP entry is only
re-dispatched on the *next* pump round — so each quiet-wire moment costs one full
multi-second pump round. Contrast: `l2check`, which does ARP itself directly over l2 with
no middleware, has a 28 µs round trip.

**Wire-visible bug 2 — the wire says connection-refused; the program says timed-out.** The
SYN to the gateway's port 9 was answered with an RST in 227 µs. The program reported
`L4Error::TimedOut`. The middleware's connect path *does* handle RST→`connection-refused`
(smoltcp socket state Closed maps to it) — but the ~6.7 s ARP stall had already consumed
the entire 6-second connect deadline before the SYN even left the machine, so the
operation was declared timed out and the socket aborted with the RST sitting unread in the
receive queue. **In usermode, the loopback transport reports the same situation correctly
as `connection-refused` (sockcheck Demo 1), so the mock and the real stack disagree about
error semantics — exactly the mock-fidelity divergence the participant predicted.**

### Participant reactions (round 2), condensed, their words where quoted

- "This is the round that mattered… the capture did what captures always do — told the truth
  the program output couldn't."
- **The existence proof is accepted.** "A 909-line wasm component claimed a PCI function,
  negotiated virtio features, set up DMA rings, and resolved ARP in 28 microseconds… So it's
  a networking system now. Barely, slowly, with bugs — but packets left the machine, and the
  architecture they crossed is the one in the pitch."
- **"The two bugs are one bug."** "The ARP stall and the wrong error are… two symptoms of the
  same root cause: *the stack only advances inside consumer calls, and the pump loop is rigid
  instead of reactive*… Look at the numbers the capture gives you: 28 µs through the driver
  alone, 6.7 seconds through the middleware. The wire is never the problem. The driver is
  never the problem. The architecture *between* calls is the problem."
- **"The mock-fidelity failure I predicted happened, on the first real test."** Loopback says
  `connection-refused`, metal says `timed-out`, same stimulus. "This is why 'same binary
  against both providers' is the most valuable test in the project — and it's the one test
  that can't run, because sockcheck isn't in the baked store."
- **"The capture was more honest than the program."** l4check printed `ok: resolved(…)` and
  looked fine; the capture showed a 7-second resolution and a misreported error. "This should
  change how the project tests itself: capture-based assertions, not just output-based ones."
- **"Third strike on tooling truthfulness."** Usermode `env` wrong twice, metal `env` hiding
  pci, the status doc claiming a demo that can't run. "For a capability OS, the introspection
  being wrong isn't a polish issue. The pitch is 'your authority is inspectable.' If the
  inspector lies, there is no pitch."
- **Their ordered to-do list for the maintainer** (reproduced verbatim in the findings table
  below): (1) fix the pump loop; (2) "wire truth beats the clock" — drain the receive queue
  before declaring timeout; (3) put sockcheck and the deny/none stubs in the baked store;
  (4) find out why identical compositions miss the compile cache; (5) one work item: the
  tools must tell the truth; (6) the configure trap; (7) then the architecture: background
  pump (and until it exists, put freeze-between-calls semantics in the WIT contract), device
  arbitration *before* multi-programming, DHCP-inside-the-middleware as the proof of runtime
  addressing, then a benchmark. Deprioritize without guilt: IPv6 ("but stop listing it as
  present — types aren't support"), the socket constants, l3.
- **"The pattern to watch":** "Twice now the docs claimed something the system doesn't do —
  'never a trap' traps, 'works on metal' isn't in the store… The fix is mechanical: every
  claim in the status doc is either a CI-run demo or labeled aspirational."
- Bottom line: "Round 1 showed me a type system I liked. Round 2 showed me it survives
  contact with hardware — and showed me precisely where it doesn't… the problems are real,
  but they're *located*, and nothing I saw breaks the model itself."

## Round 3 — the participant's verdict and structured wrap-up (their words, lightly condensed)

**The verdict.** "For production networking — no… But there is one thing I'd genuinely use it
for right now: **a deterministic test harness for networked program behavior**. If I'm
writing a protocol client and want to verify how it behaves under denial, unreachable,
refused, port-exhaustion — today I do that with network namespaces, iptables tricks, and
prayer. `net.l4.deny $ prog`, `net.l4.loopback $ prog`, and `only` give me that
declaratively, reproducibly, in 200 ms. That's real, with one asterisk: the mock's error
semantics have a known divergence from the real stack… until that's fixed the harness can
teach my program lies." Second usable thing: the static authority analysis ("I have no
equivalent tool, and I've wanted one for years"). Would they keep watching? "Yes, without
hesitation… Projects fail from unlocated problems and dishonest self-assessment. This one
has neither." What it is not: "anything requiring throughput, concurrency, dynamic
addressing, multi-program networking, unattended operation, or iteration speed on metal…
It's not yet a networking system you'd deploy. It's a networking architecture you can
finally test."

**Top 3 pain points**
1. **The pump model** — "this single thing caused both wire-visible bugs, makes the real
   stack ~5 orders of magnitude slower than the wire underneath it (28 µs vs 6.7 s), and
   corrupts error semantics. Everything else is downstream of this."
2. **On-target compile cost** — "45–60 s per composition run, identical compositions don't
   cache. Metal is unusable for iteration — by the maintainer, by me, by anyone."
3. **The tooling and docs don't tell the truth** — "each instance is small; the pattern is
   what kills trust in a system whose entire pitch is inspectability."

**Top 3 missing things**
1. **Background execution** — "without it, TCP is only nominally TCP."
2. **The multi-program networking design** — "must exist *before* concurrency ships, not
   after."
3. **The conformance harness on metal** — sockcheck in the baked store + capture-based
   assertions; "the test that would have caught both round-2 bugs before I did. It's also the
   cheapest of the three."
   Honorable mentions: DHCP, benchmarks, l3 — "all real, all gated behind the three above."

**Mis-designed (by their judgment) vs merely unfinished**
- *Wrong by design:* **time is absent from the interfaces** — `recv-frame` doesn't specify
  blocking behavior; `connect`/`recv` have no deadline parameter and no cancellation; "every
  provider invents hard-coded constants… and those constants compose into emergent
  multi-second behaviors nobody designed — that's literally what the capture shows. Wait
  policy and deadlines belong to the caller… Fix this now, while each interface has exactly
  one implementation." **Busy-wait as the wait primitive** — "a bounded spin of host calls is
  not a wait… This is not 'scheduler not done yet' — it's the wrong primitive independent of
  the scheduler." Minor: the `io(string)` escape hatches — "where typed errors go to die."
- *Merely unfinished:* the scheduler itself; l3; IPv6 ("types only — stop implying
  otherwise"); DHCP; device arbitration (with the sequencing constraint); the baked store /
  missing components / env bugs / error rendering / configure trap / compile cache; the
  socket constants ("should move into the compose-time config mechanism that already exists").
- *On probation:* **sealed compose-time addressing** — "If [a middleware learning its address
  at runtime] can't be built within the model, this moves to the mis-designed column. The
  proof either way is DHCP-inside-the-middleware. Build it."

**Genuinely impressed** (verbatim list): compositional residual authority ("a capability no
system I use has"); typed denial through layered middleware ("the layering claim actually
cashing out"); the driver as an unprivileged component ("driver-as-capability-bounded-
component is real, not slideware"); clean wire behavior ("the architecture doesn't mangle
the protocol"); the demo's honesty ("not a technical property, but it's why I believe the
other four").

**Preconditions for re-evaluating** (checkable, their order): (1) sockcheck passes unmodified
on metal, in CI; (2) loopback and metal report the same typed error for SYN→RST; (3) the
ARP-reply-to-dependent-packet gap under 50 ms (today: 6,700 ms); (4) identical composition
re-run under 2 s prompt-to-prompt; (5) the malformed-address configure returns a typed error,
not a trap; (6) `env` truthful on both targets; (7) "a throughput number they measured
themselves, published with the harness, even if it's embarrassing. I trust projects that
publish bad numbers." For a hardware round: a background pump demonstrated by a transfer
surviving a 10-second application sleep with data acknowledged during the sleep, and an
answer to "which physical NIC should I buy."

**The one question for the owner:** "**When two networked programs run concurrently over one
NIC, what is the design — and which side of it do you choose?** Per-program network identity
(every composition is its own host: own stack, own IP — then show me address allocation, the
L2 mux, and who answers ARP), or a shared stack provider (then show me what capability
attenuation *means* over shared state: can program A observe program B's connections? who
may bind port 80? what does `only` even compute when the resource is a namespace?)… Every
property I praised in this report — the static analysis, the typed denials, the layering —
was demonstrated on exactly one program at a time… That's not a gap in the implementation;
it's the central design decision of the project, still unmade."

## Findings

### Bugs / rough edges verified during the session

1. **Malformed configure address traps** (usermode, Demo 9b): `net.l4.over-l2 --address
   not-an-ip …` → raw wasm backtrace, despite the WIT contract stating "a malformed address
   is a configure-time error, never a trap." Contract violation on a first-party component.
2. **ARP-resolution stall ~6.7 s in the TCP/IP middleware** (metal, pcap): the driver's
   2,000,000-host-call receive spin (~1.7 s per empty poll) × the middleware's 4-frame
   receive batch means a packet waiting on a fresh ARP entry is delayed ~6.7 s although the
   reply arrived in 10 µs. Affects every fresh destination (the DNS query and the TCP SYN
   both paid it).
3. **Wire-visible RST misreported as `timed-out`** (metal, pcap): the ARP stall consumes the
   6-second connect deadline, so the refused path never runs. The loopback mock reports the
   same stimulus correctly as `connection-refused` → **the mock and the real stack disagree
   about error semantics** (mock-fidelity divergence).
4. **Missing-net spawn refusal is a raw linker error with wrong advice** (usermode, Demo 2):
   suggests `eo9 store reseed` (irrelevant); the fs equivalent has a friendly "pass
   `--fs-root`" message; net (and pci, Demo 11) never got one.
5. **`env` output is wrong/stale about net** (usermode, Demos 7/11): suggests composing
   `net.none` (does not exist post-layering; it is `net.l4.none`) and `rt.none` (never
   existed); claims `eo9:rt/diagnostics` "would be refused at spawn" while the runtime
   always links it and the program runs.
6. **Metal `env` does not show the pci grant** (metal, Demo 1): a `pci`-granted boot lists
   only text/time/entropy/fs/exec; the one capability that distinguishes the boot is
   invisible in the capability report.
7. **`net.l2.deny` (and every per-layer deny/none stub, and `sockcheck`) is not in the
   kernel's baked store** (metal, Demo 5): the typed-denial-on-metal demo that STATUS.md
   describes cannot be run at the metal prompt (`error: cannot resolve net.l2.deny … 
   FsError::NotFound`); checked back to the commit that landed the metal sockets demo — it
   never was in the store. The mock-fidelity test (sockcheck on metal) is likewise
   impossible today.
8. **No compile-cache hit for identical fused compositions on metal**: three runs of the same
   three-layer composition: 47.6 / 61.7 / 45.6 s, nearly all on-target Cranelift compilation
   (re-confirms user study 03 finding #13, now with a network-stack-sized cost attached).
9. **Typed errors render Rust debug text across the net surface**: `net("L4Error::Denied")`,
   `net("L4Error::Unreachable")`, `FsError::NotFound` (metal resolve failure).
10. **`only` refusal wording differs between targets**: usermode "the program still requires
    …, which the allow-list does not include (allow it, compose a provider for it, or drop
    the requirement)" vs metal "`only` failed: required imports outside the allow-list: …".
11. **Serial input dropped after heavy on-target compiles** (metal, scripted console): the
    line typed immediately after a 47 s compile lost most of its characters (known plan/12
    D49 pacing issue; bites hardest exactly when the machine was just busy).
12. **IPv6 is types-only**: the WIT carries v6 address variants everywhere, but the
    middleware parses dotted-quad only and returns `unreachable` for any v6 destination;
    nothing anywhere speaks v6.
13. **STATUS/GAPS on master lag the code**: both still describe the configure-of-
    resource-owning-providers (D21 bind entrypoint) as pending/unusable, but it landed (and
    Demo 9 / metal Demo 7 used it); STATUS still says "master at ca255c8" while master is 4
    merges past that. (The docs-refresh wave is listed as in-flight; noted for completeness.)

### Confusions observed

- The participant initially read "deny by default" as also covering the *quality* of the
  refusal; the raw linker error in Demo 2 made them ask whether the denial path was designed
  or accidental.
- "Who pumps the stack" had no documented answer anywhere (SPEC, STATUS, the WIT comments);
  it had to be answered by reading provider source. The freeze-between-calls semantics are
  observable behavior a consumer must design around, and are written down nowhere.
- Whether per-program stack instances share anything (MAC, IP, ports) is not addressed in any
  doc; the participant had to ask three times before getting "nobody owns the port namespace."

### What landed well

- The l2/l3/l4 split itself, with per-layer deny/none stubs and per-layer type isolation —
  the participant accepted the design without reservation.
- `only` computing residual authority through middleware (the "best thing in the demo" /
  "a capability no system I use has"), identically in usermode and on metal.
- Typed denial propagating through layers in the program's own vocabulary, never a trap.
- The TCP/IP stack as ordinary middleware; smoltcp rather than a homegrown stack.
- The NIC driver as an unprivileged wasm component: PCI probe, virtio negotiation, DMA rings,
  28 µs ARP round trip.
- Clean wire behavior through three component boundaries (sane MSS/window/SACK).
- `describe` / `describe --wiring` showing the middleware shape and the full composition tree.
- sockcheck as a transport-layer conformance test (listen/accept/backlog FIFO/stream
  isolation/UDP, with typed errors for every refusal case).
- Configured static addressing baking into the artifact, on both targets.
- The facilitator showing failures live (named explicitly by the participant as the reason
  they believed the rest).

### Triage table

Dispositions follow the no-drop rule: every finding is **Fix now**, **Tracked**, or **Owner
decision**.

| # | Finding | Disposition |
|---|---|---|
| F1 | Malformed configure address traps; WIT promises "configure-time error, never a trap" (finding 1) | **Fix now** — validate before the bind call / return the typed error; add a malformed-config test for every config interface. |
| F2 | ARP stall: driver receive spin bound (2M host calls) × middleware pump batch → ~6.7 s per fresh neighbor (finding 2) | **Fix now** — the participant's #1: re-poll the stack as soon as any frame arrives; cut the spin bound to microseconds; let "empty" return immediately. |
| F3 | RST misreported as timed-out; loopback and metal disagree on error semantics (finding 3) | **Fix now** — drain + classify the receive queue before declaring `timed-out` ("wire truth beats the clock"); **Tracked** — a mock-fidelity conformance test running the same binary against loopback and metal. |
| F4 | sockcheck + per-layer deny/none stubs missing from the kernel baked store; STATUS claims the metal typed-denial demo (finding 7) | **Fix now** — add to `KERNEL_STORE_COMPONENTS` (makes the STATUS claim true and unblocks F3's test); correct STATUS meanwhile. |
| F5 | Missing-net/pci spawn refusals are raw linker errors with wrong advice; `env` recommends nonexistent `net.none`/`rt.none` and misclassifies rt/diagnostics; metal `env` hides the pci grant; debug-text error rendering; `only` wording differs across targets (findings 4, 5, 6, 9, 10) | **Fix now** — one "the tools must tell the truth" pass (the participant's #5): friendly net/pci refusals naming the composing fix, truthful `env` on both targets, unified `only` wording, typed-error rendering. |
| F6 | Identical fused compositions never hit the metal compile cache; 45–60 s per run (finding 8) | **Tracked** (existing study-03 #13) — **bumped**: investigate the cache key for fused artifacts; this now blocks all metal networking iteration. |
| F7 | No background pump: stack frozen between l4 calls; sleep-mid-stream means silent TCP | **Tracked** (scheduler adoption, existing roadmap) + **Fix now (docs)**: document freeze-between-calls semantics in the WIT/SPEC as observable behavior until the scheduler exists. |
| F8 | Wait policy/deadlines/cancellation absent from the net WIT; every provider hard-codes its own constants (participant: "wrong by design") | **Owner decision** — interface-design call: caller-supplied deadlines/wait policy vs provider constants. The participant urges deciding "now, while each interface has exactly one implementation." |
| F9 | Multi-program networking undesigned: device arbitration, frame mux, port-namespace ownership (the participant's "one question for the owner") | **Owner decision** — per-program network identity vs shared-stack provider. Sequencing constraint either way: must be designed before multi-programming ships. |
| F10 | No DHCP / runtime-learned addressing; sealed compose-time addressing "on probation" | **Tracked** — DHCP-inside-the-middleware as the existence proof that runtime addressing fits the model (also resolves the participant's probation item). |
| F11 | IPv6 is types-only; docs/WIT imply more (finding 12) | **Tracked** — either wire v6 through the middleware or state "IPv4 only" wherever the types appear. |
| F12 | No throughput/pps numbers; no compose/compile/run timing split | **Tracked** (existing instrumentation item) — participant ordering: only meaningful after F2. |
| F13 | l3 has no provider and no consumer | **Tracked** (existing) — participant: fine to deprioritize, "but admit it's speculative" in the pitch. |
| F14 | Socket count / buffer sizes are hard-coded constants | **Tracked** — expose through the existing compose-time configure mechanism. |
| F15 | `io(string)` catch-all in every net error variant | **Owner decision** — keep as pragmatic escape hatch vs enumerate further; participant calls it "where typed errors go to die." |
| F16 | Serial input dropped after heavy compiles (finding 11) | **Tracked** (existing plan/12 D49) — note added that it correlates with compile activity, not just paste speed. |
| F17 | Capture-based test assertions (pcap-level, not just program-output-level) | **Tracked** — new test-suite work item (area 13); QEMU filter-dump + assertion on frame timing/content; xtask could grow a `pcap` flag. |
| F18 | STATUS/GAPS lag the landed bind-entrypoint work (finding 13) | **Fix now** — already in flight (docs-refresh wave); verify it covers the D21 landing. |

## Facilitator observations

- The cloned-build-cache trick (APFS `cp -c` of the main checkout's target dirs into the
  worktree) made the whole study practical: build-guest + the eo9 release build + the kernel
  image completed in under a minute total. A cold worktree would have spent most of the
  session compiling.
- The brief's planned demo `net.l2.deny $ net.l4.over-l2 $ l4check` on metal could not be
  run at all (finding 7); the facilitator only discovered this live, mid-session, exactly as
  a user would. The same goes for the participant's sockcheck-on-metal request. Demo
  preparation against STATUS.md is not sufficient — the kernel store contents are the actual
  contract.
- The participant's single most valuable contribution — the pcap analysis — required
  bypassing xtask (QEMU invoked manually with `-object filter-dump`). xtask's `qemu` command
  has no capture option; adding one (`cargo xtask qemu aarch64 pci net pcap=<file>`) would
  make the capture-based testing the participant asked for nearly free.
- Boot-to-prompt is 0.4 s when QEMU is invoked directly; the 12.5 s figure users see from
  `cargo xtask qemu` is almost entirely cargo's no-op build check. Worth printing the split
  or saying so, since "boots in 12 seconds" undersells the kernel by 30×.
- The expect-script UART pacing rules from plan/12 D49 were necessary and sufficient: 1
  char / 40 ms with 5 s settles after prompts had zero input drops across two sessions; the
  faster 5-chars / 30 ms pacing lost a line right after a long compile.
- Every number and output in this report is from a real execution in this checkout; nothing
  is reconstructed from documentation. The pcap, the QEMU logs, and the participant
  transcripts were retained as session artifacts (not committed).

