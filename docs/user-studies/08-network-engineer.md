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

### Participant reactions (round 1)

*(to be filled after the participant session)*

## Round 2 — bare metal: the real NIC driver and the three-layer stack

*(to be filled after the metal demos)*

## Findings

*(to be completed)*
