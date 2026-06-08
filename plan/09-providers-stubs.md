# 09 — Standard stub providers (`guest/stubs/*`)

## Scope
The hand-written stub/virtual providers from the spec's "Standard stubs" lists — small wasm components, one
crate each, composable with `$`/`&`/`with`.

## Spec references
"The capability algebra" (none/deny/stubs table and rules), per-API "Standard stubs" lines, "Environments
and the `&` operator" (the deterministic-environment example), Security (time.fuzzy).

## Deliverables (priority order)
1. `*.none` for every API (exports the `-optional` flavor answering `none`) — tiny, mechanical to write by
   hand, needed by `only`'s story and the loader rule.
2. Deterministic set: `fs.memfs`, `time.frozen`, `time.monotonic-stub`, `entropy.seeded`, `disk.mem` —
   together these make the deterministic environment of integration milestone I2.
3. Attenuators/refusers: `net.deny`, `net.loopback`, `fs.readonly` (imports fs, re-exports it read-only —
   first real middleware provider), `text.null`, `time.fuzzy` (jittered/quantized).
4. Later (needs Message API): `text.capture`.
- Each stub: targets its stub world from `wit/` (plan 02), takes `configure` args where the spec implies
  config (e.g. `entropy.seeded --seed`, `fs.memfs` size), ships with a compose-and-run test against an
  example program.

## Dependencies
02, 07 (provider-authoring support). Consumed by 10, 13, and the I2 milestone.

## Milestones
Match the priority order above; (1)+(2) unblock I2.

## Decisions

1. **Layout and build flow.** One small crate per stub under `guest/stubs/<api>-<stub>`, package name
   `eo9-stub-<api>-<stub>`, listed in `GUEST_COMPONENTS` so `xtask build-guest` componentizes and validates
   it like the examples. Each crate is `no_std` and runs `wit_bindgen::generate!` directly against the
   repo-level `wit/<api>` package (`path: "../../../wit/<api>"`), so the stub worlds are consumed from the
   interface source of truth with no per-crate WIT copies; `eo9-guest` is depended on for the guest runtime
   profile (allocator + panic handler) and the provider helpers (see plan/07, Decisions).
2. **Shipped (v0).** The `.none` stub for every API — `disk.none`, `entropy.none`, `fs.none`, `net.none`,
   `perf.none`, `text.none`, `time.none` — plus `entropy.seeded` and `perf.null`. (`perf.null` is not in the
   priority list but is synchronous and trivial, so it shipped alongside.) Verified with
   `wasm-tools component wit`: every shipped stub imports **nothing** and exports exactly its stub world's
   interfaces (`eo9:X/types` + `eo9:X/X-optional` for the `.none`s; types + API + config interface for
   `entropy.seeded` and `perf.null`).
3. **State and handle convention.** A provider's exported resource types are tokens; the state they refer to
   lives in a `static` (`eo9_guest::provider::ProviderState`), bound by `configure`. `configure` returns a
   fresh handle to that state and `default()` mints another handle to the *same* state — the spec's
   "`default()` hands out exactly the handle `configure` produced" is read as capability identity (same
   state/authority), since an `own` handle cannot be handed out twice. Using a provider before `configure`
   traps (the contract violation is the embedder's, not the program's).
4. **`entropy.seeded` PRNG.** SplitMix64 over the configured seed (hand-written, no dependencies);
   documented as reproducible-but-not-cryptographic.
5. **Deferred: every stub whose interface has `future`-returning operations** — `fs.memfs`, `disk.mem`,
   `time.frozen`, `time.monotonic-stub`, `net.deny`, `net.loopback`, `fs.readonly`, `text.null`,
   `time.fuzzy` (and `text.capture`, which additionally waits on the Message API). Reason (escalated to the
   planner): with the pinned toolchain a wasm guest provider cannot implement a plain
   `func(...) -> future<T>` export. wasm-tools 1.250 enforces "the `async` canonical option requires an
   async function type", so only `async func` exports (e.g. `configure`) may be async-lifted; a
   synchronously-lifted export has no live Component Model task left after it returns, and futures are
   rendezvous, so there is nothing to deliver the value the stub would need to write (wit-bindgen requires a
   current task to park the pending write, and dropping a writable end unwritten traps). Host-side providers
   (area 08) are unaffected — the constraint is specific to providers compiled to wasm.
   **Proposal:** declare the API operations as `async func(...) -> T` instead of `func(...) -> future<T>`
   (callers keep the same concurrency via async-lowered calls/subtasks, and guest providers become ordinary
   async functions — no future plumbing at all); the deferred stubs are then mostly mechanical. Decision
   belongs to the planner/area 02.
6. **Async `configure` works as specified.** The config-interface exports async-lift, componentize, and
   validate (`--features cm-async`); actually invoking them needs the host runtime's CM-async support
   (area 04), same as the examples that await futures.
7. **Round 2 (branch `area/09-stubs-2`).** Decision 5's escalation was resolved by the async-operations
   migration (plan/02-wit.md, decision 12): blocking operations are now `async func(...) -> T`, so a guest
   provider implements them as ordinary async trait methods — compute immediately (the deterministic stubs)
   or await its own imports (the attenuators). Shipped: `fs.memfs`, `disk.mem`, `time.frozen`,
   `time.monotonic-stub`, `net.deny`, `fs.readonly`, `text.null`, `time.fuzzy` — same crate layout and
   conventions as round 1, no changes to `eo9-guest` beyond refreshing the provider-module docs.
8. **Verified import lists** (`wasm-tools component wit` on the built components):
   `time.frozen`, `time.monotonic-stub`, `text.null` import nothing; `disk.mem`, `fs.memfs`, `net.deny`
   import only `eo9:io/buffers` (structurally required: the exported API's signatures use the buffer
   resource, so the world elaborates that import); `fs.readonly` imports `eo9:fs/fs`, `eo9:fs/types`, and
   `eo9:io/buffers`; `time.fuzzy` imports `eo9:time/time` and `eo9:time/types`. Attenuators share the
   underlying provider's root-handle type, per the stub-world design (plan/02, decision 7).
9. **Behavioural choices the WIT leaves open** (documented in each crate's docs): memfs — `/`-separated
   paths with `.`/`..` normalization, create-requires-existing-parent, truncate clears, Unix unlink
   semantics for open files, reads return what is available, writes zero-fill gaps and extend, remove only
   deletes empty directories, open-exec snapshots contents (immutability by copying); disk.mem — fixed-size
   device, out-of-range whenever the full range does not fit (no partial I/O); time.frozen —
   `resolution() = u64::MAX`, sleep returns immediately; time.monotonic-stub — each observation answers then
   advances by the step, sleep advances by the requested duration, `resolution()` reports the step;
   time.fuzzy — field-wise floor quantization, `resolution() = max(underlying, granularity)`, sleep rounds
   the duration up to the granularity; net.deny — connect/listen/bind-udp fail `denied`, the
   connection/listener/socket resources are uninhabited; fs.readonly — open with write/create/truncate,
   create-directory, remove, and write fail `read-only`, everything else forwards.
10. **Still deferred.** `net.loopback`: a correct loopback needs `accept`/`recv` to suspend until the
    matching `connect`/`send` arrives in another concurrently-running export task of the same (fused)
    instance. Expressing that requires an intra-provider waker registry plus wit-bindgen's
    `inter-task-wakeup` feature (a change to the shared guest dependency pins) and host-side support for
    concurrent tasks within one instance (area 04) — neither verifiable from this area; a non-blocking
    approximation would be semantically wrong, and a yield-spin loop would be a hack. Escalated: either
    approve enabling the feature once the host side exists, or keep net.loopback queued behind area 13's
    execution harness. `text.capture` still waits on the Message API (eo9:message).
11. **`fs.overlay` — implemented and built.** Implements SPEC.md "Overlay filesystems": a middleware
    provider importing two `eo9:fs/fs` instances under the named slots `upper` and `lower` (the
    `with <a> as upper, <b> as lower $ fs.overlay` shape) and exporting one `eo9:fs/fs` — reads resolve
    upper-first and fall through to lower on not-found (`open`(read)/`stat`/`open-exec`; `list-directory`
    unions both layers, upper winning on collisions), writes route to lower
    (`open`(write)/`write`/`create-directory`/`remove`); the overlay never mutates `upper`. It exports its
    own `eo9:fs/types`, so the root handle is a compound capturing both underlying roots; open files and
    immutable handles are per-layer-tagged enums so each `read`/`write`/`exec-read` dispatches back to the
    layer that served the open (a write through a read-opened upper file is forwarded so the upper's own
    policy answers — typically `read-only`). The crate keeps its own `wit/overlay.wit` package (deps
    symlinked to the shared `wit/`), which needs the named-import syntax: this is what motivated the guest
    workspace's wit-bindgen git pin (plan/07 Decisions 9–10). Binding-layout notes for future two-slot
    providers: the slot modules generate at the crate root (`crate::upper`, `crate::lower`); the two slots
    share the imported `eo9:fs/types.fs-impl` and the `eo9:io` buffer resource, but each slot has its own
    nominal `file`/`immutable-handle`/error/record types. `fs.immutable` is not separately needed —
    `fs.readonly` already provides read-only-over-an-imported-fs; the future programs/coreutils overlay
    composes read-only program content as the overlay's `upper`.
12. **Two-slot wiring needs a per-slot root-handle decision (escalation).** The overlay component builds,
    validates, and describes correctly (integration test `overlay_component_exposes_upper_and_lower_slots`
    covers the surface incl. renaming the named slots), but composing two *independent* component leaves
    into its slots is ill-typed today: the world's two `fs` imports `use` the single imported
    `eo9:fs/types`, so both slots' `fs-impl` is the *same* imported resource type, while every standalone
    fs provider (`fs.memfs`, `fs.deny`, …) exports its *own* fresh `types` resource. Verified empirically:
    `rename(memfs,fs→upper/lower)` then any wiring order (`$` partial, `&` env then `&`/`$`) fails with
    eo9-component's `Internal("encoding produced a component that failed validation")` — and the overlay
    binary's import types confirm the `(eq imported-types.fs-impl)` constraint on both slots, so this is
    inherent to the WIT shape, not an encoder bug (though eo9-component could diagnose it before encoding —
    minor follow-up). The end-to-end test (`readwrite_through_the_overlay_round_trips`) is committed
    `#[ignore]`d, ready to enable. Options for the planner: (a) for the real Phase-2 use (the standard
    programs overlay over `--fs-root`), link both slots host-side in the runtime/shell from one host
    `eo9:fs/types` instance — no WIT change, but the runtime must learn to link two named fs slots;
    (b) move `fs-impl` out of `eo9:fs/types` into the `fs` interface (or otherwise give each fs import its
    own root-handle type) so independent component leaves wire cleanly — a cross-area WIT change (area 02)
    that would also touch every existing fs stub; (c) only ever feed the overlay layers that share a types
    lineage (attenuators over one base) — too restrictive to be the answer. Until one lands, `fs.overlay`
    ships as a built, validated component with its semantics implemented but not yet composable from
    independent component leaves.

13. **fs stubs after the root-handle move (plan/02 D15) — and the remaining layering blocker is
    configuration, not typing.** `fs.memfs`/`fs.readonly`/`fs.none`/`fs.overlay` were updated mechanically:
    the exported `fs` interface's `Guest` trait now carries `type FsImpl`, `fs.readonly` mints its own root
    token (it no longer re-exports the underlying provider's handle), and `fs.none` names the *imported*
    `eo9:fs/fs.fs-impl` (a types-only use) in its `fs-optional` export. `fs.overlay` drops the shared-types
    workaround: each slot mints its own root-handle type and the two-leaf composition validates. What still
    cannot run end to end is configuring the leaves: a provider's config interface is dropped by the
    composition that wires it into a slot (its handle type is tied to its own instance, so it cannot tunnel
    through the overlay to the consumer either), so an unconfigured `fs.memfs` leaf traps on first use. The
    behavioral round-trip test stays `#[ignore]`d on that reason. Options for the planner: default
    configurations for the stubs (the pending owner decision on unconfigured-provider semantics would close
    this for memfs, whose configure takes no arguments), a configuration-free static fs leaf for tests, or
    compose-time configuration that survives slot wiring.
14. **Default configurations (the owner's option-C ruling): a runtime panic is never the outcome of an
    unconfigured provider.** Stubs with a sensible default now self-bind it lazily on first use and document
    it: `entropy.seeded` → seed `0xE09`; `time.frozen` → 2000-01-01T00:00:00 UTC with monotonic origin 0;
    `time.fuzzy` → 1 ms granularity; `fs.memfs` → the empty filesystem (identical to what its nullary
    `configure` creates). `configure`/provider flags override the default exactly as before, so all existing
    configured behavior is unchanged. Plain compositions (`time.frozen $ hello`, `entropy.seeded $ rng`,
    `fs.memfs $ readwrite`) therefore run deterministically out of the box — covered by the new
    `default_configuration` integration suite — and the `fs.overlay` behavioral round-trip with two
    unconfigured memfs leaves now runs end to end (the configuration half of Decision 13 is closed; its
    compose-time-configuration-of-leaves option remains open for providers that genuinely need arguments).
    Rule for future stubs: if no sensible default exists (e.g. a net provider needing an address), the
    failure must be a clear pre-run/typed refusal — never a trap; implement it as a typed error from the
    API operations (or a loader-visible required-config marker once one exists), not a panic.
15. **Per-layer net stubs, and `net.l4.loopback` as the standard transport mock (plan/02 D17).** The old
    `net.none`/`net.deny` stubs are replaced by one none/deny pair per layer — `net.l2.none`/`.deny`,
    `net.l3.none`/`.deny`, `net.l4.none`/`.deny` — so absence and refusal can be expressed at exactly the
    layer a program imports, plus `net.l4.loopback`: a self-contained in-memory transport (TCP + UDP
    between sockets created through the same instance, loopback addresses only, canonicalized to
    127.0.0.1/::1, port 0 binds an ephemeral port). Loopback semantics chosen for single-task test flows:
    `connect` requires a listener and completes immediately by queuing the server end on the listener's
    backlog (so listen → connect → accept works sequentially); the provider never blocks — `accept` with
    an empty backlog and `recv`/`recv-from` with nothing queued fail with a typed `io` error saying so;
    a dropped peer reads as EOF (0 bytes) after the queue drains and `connection-reset` on send;
    `recv-from` truncates to the destination buffer like real UDP. Default configuration per D14: the
    documented default is the empty loopback network, self-bound lazily, identical to what the nullary
    `configure` creates. The new example `sockcheck` (TCP both-ways echo + UDP round-trip against any l4
    provider) plus the `net_loopback` integration suite cover `net.l4.loopback $ sockcheck` end to end and
    `net.l4.deny $ sockcheck` failing in the layer's own vocabulary; all per-layer stubs joined the
    soundness corpus. Follow-ups: an l4-over-l3 middleware provider (smoltcp-style) and a real l2/l3
    backend over virtio-net (kernel root provider) are the planned next consumers; eosh-side docs/examples
    still reference `net.none`/`net.deny` in comment strings only.

16. **`disk.virtio` — the first real device driver, as an ordinary provider component (2026-05-29, branch
    `area/08-virtio-blk`).** `guest/stubs/disk-virtio` imports `eo9:pci/pci` (plus `eo9:text/text` for one
    diagnostic line) and exports `eo9:disk/disk`: a modern (virtio 1.0, `disable-legacy=on`) virtio-blk
    driver — capability walk through configuration space, common/notify/device-config windows through
    `open-bar`/`bar-read`/`bar-write`, exactly `VIRTIO_F_VERSION_1` negotiated, one 16-entry split virtqueue
    in `alloc-dma` buffers, requests kicked through the notify register and completed by polling the used
    ring (the kernel PCI provider has no interrupt delivery yet; virtio is fine with that). The exported disk
    is byte-addressed over the 512-byte-sector device: reads fetch covering sectors and copy out the range,
    writes read–modify–write partial edge sectors, ranges that fall outside the capacity fail with
    `out-of-range`, zero-length accesses up to the capacity succeed (the same contract as `disk.mem`).
    Decisions: (a) it lives in `guest/stubs/` under the stub naming convention (`eo9-stub-disk-virtio` →
    shell name `disk.virtio`) because that is what seeds/composes cleanly everywhere today — "the standard
    providers directory also holds drivers" is the recorded reading, and a rename to a dedicated drivers/
    area can happen wholesale later; (b) no configure interface — the documented default claims the first
    virtio-blk function visible through the granted capability on first use, and "exactly this device" is
    `pci.filtered`'s job composed in front; (c) like `fs.eofs`, every pci import is driven eagerly
    (`poll_eager`), so the exported operations complete in a single poll and the driver works under
    consumers that poll their disk import eagerly; (d) device errors surface as typed `io(...)` disk errors
    naming the failing step, never traps; (e) v0 limits, recorded: one request in flight at a time, a 64 KiB
    bounce buffer per request, no FLUSH (durability is QEMU's writeback cache for now), no MSI/INTx. Verified
    on QEMU aarch64 metal (plan/12 D50): the full composed stack `disk.virtio $ fs.eofs $ ls / readwrite /
    cat` runs compiled on-target, and data written through it survives a QEMU power cycle.

17. **`net.virtio` — the virtio-net sibling of `disk.virtio` (2026-05-29, branch `area/09-virtio-net`).**
    A second device driver as an ordinary provider component, `guest/stubs/net-virtio`
    (`eo9-stub-net-virtio` → shell name `net.virtio`, ~137 KB raw): imports `eo9:pci/pci` (plus `eo9:text`
    for one diagnostic line) and exports `eo9:net/l2` — the single interface `virtio0`, its MAC address,
    and whole-frame send/receive. The probe and bring-up reuse `disk.virtio`'s shape verbatim (capability
    walk, common/notify/device-config windows, modern device id 0x1041 preferred with the transitional
    0x1000 accepted when it carries the modern capabilities); the deltas are network-specific:
    `VIRTIO_NET_F_MAC` is negotiated alongside `VIRTIO_F_VERSION_1` so the device-config window carries a
    stable MAC, two virtqueues are built (receive and transmit, 16 entries each, both rings sharing one
    `alloc-dma` page), eight 2 KiB receive buffers are pre-posted and re-posted as they are consumed, and
    every frame crosses the rings behind the 12-byte virtio-net header (zeroed on transmit — no offloads —
    and stripped on receive). `recv-frame` is a bounded poll that reports a typed `io` error when nothing
    arrives (never a hang); an oversized frame fails with `frame-too-large`; all device weirdness is a
    typed error, never a trap. DMA addresses come only from `alloc-dma`/`dma-address`, so the containment
    story is identical to the disk driver: two explicit grants (the `pci` boot token and the xtask `net`
    flag) before the driver can touch anything. The new `l2check` example (`guest/examples/l2check`)
    proves both directions end-to-end by ARP-resolving the QEMU user-net gateway: it lists the interface,
    broadcasts a who-has-10.0.2.2 request, and waits for the reply. Verified interactively on QEMU aarch64
    metal: `net.virtio $ l2check` (compiled on-target) prints the probe line
    (`net.virtio: virtio-net 52:54:00:12:34:56, queues rx/tx 16/16`) and resolves
    `10.0.2.2 is at 52:55:0a:00:02:02` → `ok: resolved(...)`. Remaining for the driver track: interrupt
    delivery once the PCI provider grows it, multi-frame receive batching if a consumer ever needs it, and
    the planned consumers — an l3-over-l2 / l4-over-l3 middleware stack (smoltcp-style) so programs that
    speak sockets can run over this link layer.

18. **`net.l4.over-l2` — the TCP/IP stack as ordinary middleware (2026-05-29, branch
    `area/09-net-l4-over-l2`).** The layered-net design's payoff: a provider component that imports the
    link layer (`eo9:net/l2`), the clock (`eo9:time/time`, for TCP/ARP timers and operation deadlines) and
    entropy (`eo9:entropy/entropy`, for ephemeral ports and ISNs), and exports transport sockets
    (`eo9:net/l4`), so a program that speaks only l4 gets working TCP and UDP by composition —
    `net.virtio $ net.l4.over-l2 $ program` on metal, any mock l2 in tests. The engine is **smoltcp
    0.12.0** (guest-workspace-only dependency, `default-features = false`, features `alloc`,
    `medium-ethernet`, `proto-ipv4`, `socket-tcp`, `socket-udp`); the provider drives its l2 import
    eagerly (the same single-poll convention as `net.virtio`'s pci and `fs.eofs`'s disk imports) and each
    exported operation pumps frames between the link and the stack until it completes or its deadline
    passes — nothing suspends mid-operation and nothing blocks forever (deadlines: 4 s receive, 6 s
    connect, 1.5 s send-flush, all backed by a hard pump-round cap so a frozen test clock cannot hang an
    operation). Bounds: 16 sockets, 16 KiB TCP buffers per direction, 8×1536 B / 4×1536 B UDP queues,
    32-frame receive queue. Errors stay typed end to end: the l2 layer's `denied` surfaces as the l4
    `denied`, everything else maps to `timed-out` / `connection-refused` / `io(...)`. **Defaults:** QEMU
    user-mode networking's layout — 10.0.2.15/24, gateway 10.0.2.2 — bound lazily on first use
    (plan/09 D14); address overrides need an `l4-over-l2-config` interface in `wit/net`, recorded as the
    follow-up (wit/ is deliberately untouched by this branch). The `l4check` example (imports only
    `eo9:net/l4`) sends a UDP DNS query for `example.com` to 10.0.2.3 and reports the answer plus the
    typed outcome of a TCP connect to 10.0.2.2:9. Verified: usermode, the no-traffic chain
    `entropy.seeded $ time.monotonic-stub $ net.l2.deny $ net.l4.over-l2 $ l4check` ends in the program's
    own `denied` failure in 0.24 s (tests/eo9-integration/tests/net_l4_over_l2.rs); on metal,
    `net.virtio $ net.l4.over-l2 $ l4check` compiled on-target and answered
    `ok: resolved("example.com is 172.66.147.243; tcp 10.0.2.2:9 -> ConnectionRefused")` — real DNS through
    three composed wasm layers and slirp, and the refused SYN reported as a typed error. Component sizes:
    192 KiB (middleware), 83 KiB (l4check). Remaining for the track: the config interface above, DHCP and
    IPv6 (smoltcp features exist, deliberately off), an l3 export over the same engine, listener/accept
    coverage beyond the basic path, and riscv64 metal coverage once that arch has a PCI provider.

19. **`pci.filtered` — the allow-listed PCI attenuator (2026-05-29, branch `area/12-pci-interrupts`).** The
    stub world the WIT always specified (`eo9:pci/filtered`: import `pci`, export `pci` + `filtered-config`)
    now has its provider: `guest/stubs/pci-filtered` forwards every operation to the underlying capability on
    wrapped device/bar/interrupt/dma resources, filters `enumerate` down to the configured allow-list of
    device addresses, and refuses `open` outside it with `denied`. The root handle is the underlying
    provider's own (`pci-impl` still lives in the types interface; the filtering lives in the exported
    operations, not the handle). Unconfigured, the documented default is the empty allow-list — nothing is
    visible, every open is `denied`, nothing traps (the option-C rule, D14). Baked into the kernel store
    (21 entries). Verified on QEMU aarch64 metal (boot with `pci`): plain `lspci` → 3 devices;
    `pci.filtered $ lspci` → `devices(0)` — the attenuator composed, compiled on-target, and filtered
    everything out. **Known gap:** the configured path (`pci.filtered --allow [{…}] $ lspci`) is not yet
    reachable from the shell — the compose-time configuration binder bakes only scalars, strings, and enums,
    and `allow` is a `list<device-address>`; the shell run fails with that typed error. Follow-ups, owner to
    pick: extend the binder's configure baking to lists/records (area 03), or respell the allow-list as a
    string in wit/pci. Until then the deny-all default is the usable behavior, and the wrapped-forwarding
    plumbing is in place for either resolution.

20. **Disk flush/size in the stubs; the middleware's config entry exists but cannot be baked yet
    (2026-05-29, branch `area/02-wit-roundout`).** `disk.mem` reports its configured size and
    flushes as a no-op; `disk.virtio` now also negotiates `VIRTIO_BLK_F_FLUSH` when the device
    offers it and issues a real two-descriptor `VIRTIO_BLK_T_FLUSH` request from `flush` (a
    device that does not offer the feature is write-through by definition, so flush is then a
    successful no-op); `fs.eofs` reads the device size from `disk.size` and forwards the engine's
    commit-boundary flushes to `disk.flush`, so durability now rides on the real device.
    `net.l4.over-l2` exports `eo9:net/l4-over-l2-config` and applies configured addressing on
    first use (defaults unchanged: 10.0.2.15/24, gw 10.0.2.2) — but actually *baking* that
    configuration through `configure(…)`/shell argument application is refused today because
    `eo9:net/l4` declares its own resources and compose-time configuration of resource-owning API
    providers is the parked plan/03 D13 design (same class as `fs.memfs`/`disk.mem` configs); the
    integration test pins the typed refusal so the upgrade is visible when the binder learns it.
    Configure-arg baking also has no `option<…>` support, which is why the config takes exactly
    three required parameters.

21. **TCP listen/accept depth: sockcheck exercises the server-side surface, and two middleware
    accounting bugs found by inspection are fixed (2026-05-30, branch `area/15-tail-batch`).**
    The `sockcheck` example now covers the listen/accept semantics every l4 provider must share:
    listen is its *first* transport operation (so a denied/down link fails in the listen path —
    pinned by a new integration test composing `sockcheck` over `net.l2.deny $ net.l4.over-l2`),
    a duplicate bind of the listener's port must refuse with `address-in-use`, a connect to a
    dead port must refuse with `connection-refused` (never hang), two connections queued on the
    backlog before any accept must come back FIFO with their streams un-crossed (proved by
    distinct payloads), and an accepted connection must be able to talk back to its client.
    `net.l4.loopback $ sockcheck` runs all of it in-memory (`echoed(40)` for a 9-byte payload);
    integration coverage lives in net_loopback.rs + net_l4_over_l2.rs. Three findings:
    (a) **surfaced by the new test** — sockcheck used to bind its listener to a literal
    `127.0.0.1`, which the loopback stub canonicalizes but the middleware refuses as
    `address-unavailable` (it has no loopback interface), so the same program could not even
    reach the middleware's listen path; the portable server spelling is binding the
    *unspecified* address (0.0.0.0), which every l4 provider accepts and resolves to its own
    local address — sockcheck now does that for TCP and UDP, and the doc comments say why.
    Two more `net.l4.over-l2` bugs found by inspection while writing the tests (not reachable
    without a live link, but real): (b) `accept` added its replacement listening socket without
    incrementing the live-socket count, so the documented MAX_SOCKETS bound under-counted by one
    per accept; (c) the `address-in-use` check inferred "port taken" from sockets in the `Listen`
    TCP state, so a listener whose socket was mid-handshake or established-but-not-yet-accepted
    did not count as holding its port. Listener-held ports are now tracked explicitly
    (`listening_ports`) and released on listener drop. What is still *not* covered: the
    middleware's happy-path listen→accept (needs a real or hairpin l2 — only the metal
    `net.virtio` path could exercise it today), and accept-with-empty-backlog timeout semantics
    over a live link.

22. **Configure validation errors are discarded by the bind entrypoint (user study 08, finding F1 —
    2026-05-31, branch `area/09-net-fixes`).** Study 08's participant configured the TCP/IP middleware
    with `--address not-an-ip` and got an unsymbolized wasm trap, although `l4-over-l2-config`'s own
    WIT doc promises "a malformed address is a configure-time error, never a trap." Root cause is
    *not* in the middleware — its `configure` validates dotted-quads and returns the typed
    `result<l4-impl, string>` error exactly as the contract says. The error is discarded downstream,
    in machinery this area does not own: the synthesized `eo9:rt/configured.bind` entrypoint has
    signature `func()` (no error channel), so eo9-component's binder reads `configure`'s result
    discriminant and lowers the error case to `unreachable` (configure.rs `bind_body`), and the
    usermode executor renders that bind failure from the raw wasmtime error without consulting the
    diagnostics slot (eo9-runtime task.rs). What this branch ships: an integration test
    (net_l4_over_l2.rs `a_malformed_configure_address_never_lets_the_program_run`) pinning the safety
    property that does hold — a malformed address never spawns the program and the refusal is
    attributed to compose-time configuration — so the gap cannot widen into "bad config silently runs
    with defaults." The complete fix needs, in order: (a) wit/rt — `bind: func() -> result<_, string>`;
    (b) eo9-component configure.rs — lift `configure`'s error string through the binder and return it
    from bind; (c) the three executors (usermode task.rs, kernel shellexec.rs/runner.rs, browser
    execsurface.rs/store.rs) — render a bind error as "compose-time configuration refused: <reason>",
    no trap, exit/refusal class unchanged. Until then every provider config interface shares this
    failure mode, and per-provider workarounds (panicking with the message, falling back to defaults)
    are all worse than the gap; none is taken.

23. **The receive spin is dead; the consumer owns the wait (user study 08, findings F2 + F3 —
    2026-05-31, branch `area/09-net-fixes`).** The study's packet capture showed every fresh ARP
    resolution through the TCP/IP middleware stalling ~6.7 s although the wire answered in 10 µs, and a
    wire-visible TCP RST being reported as `timed-out` because the stall had already burned the connect
    deadline. Mechanism: `net.virtio`'s `recv-frame` with nothing waiting spun its full
    2,000,000-host-call bound (~1.7 s) before reporting anything, and reported that "nothing" as a
    typed *error*; the middleware's pump treated the error as end-of-round, so each quiet-wire moment
    cost multi-second pump rounds. Three changes: (a) **net.virtio** — the receive poll bound drops to
    2,000 host calls (~2 ms) and "nothing waiting" is now the WIT's own empty result
    (`bytes-received: 0`), not an error; runts/unusable completions are also empty results (wire noise,
    not failures). The driver no longer imposes a wait policy on its consumers — that was never its
    job. (b) **l2check** — treats the empty result as "poll again" with a 64-attempt budget (~100–200 ms
    ARP window). (c) **net.l4.over-l2** — `wait_until`'s deadline path pumps once more and re-checks
    before declaring `timed-out` ("wire truth beats the clock"): frames that already reached the device
    decide the outcome, so a SYN answered by an RST reports `connection-refused` even when the answer
    is processed late. Verified on metal (QEMU aarch64, `pci net`): the three-layer
    `net.virtio $ net.l4.over-l2 $ l4check` went from 60.8 s wall / `tcp 10.0.2.2:9 -> TimedOut`
    (deadline burned by the ARP stall) to 45.9 s wall / `tcp 10.0.2.2:9 -> ConnectionRefused` — the
    refused TCP probe now matches what `net.l4.loopback` reports for the same stimulus (the study's
    mock-fidelity acceptance criterion), and the residual wall time is the on-target compile (study
    finding F6, tracked separately). `net.virtio $ l2check` still resolves the gateway (the direct l2
    consumer keeps working under the short poll). The remaining modernization — converting the
    driver's receive to a PCI INTx interrupt wait like `disk.virtio` — stays the plan/12 D59 follow-up;
    the polled path these fixes leave behind is its fallback either way.

24. **The per-layer net stubs, the loopback transport, and sockcheck are in the kernel store (user
    study 08, finding F4 — 2026-05-31, branch `area/09-net-fixes`).** STATUS claimed the
    typed-denial-on-metal demo (`net.l2.deny` under the middleware) but none of the per-layer
    deny/none stubs, `net.l4.loopback`, or `sockcheck` were in `KERNEL_STORE_COMPONENTS` — the demo
    could not be typed at the metal prompt at all (`FsError::NotFound`), and the study's facilitator
    only discovered this live, mid-session. All eight are now baked in (store: 22 → 30 entries; aarch64
    image: 38.1 → 42.3 MB, +4.2 MB of components + precompiled artifacts), and the demos are verified
    on QEMU aarch64 (`pci net` boot, compiled on-target): `net.l2.deny $ net.l4.over-l2 $ l4check` →
    `error: denied`; `net.l4.loopback $ sockcheck --payload metal-ping` → `ok: echoed(44)` — the
    transport conformance test passing on metal against the same in-memory transport it passes against
    in usermode is the foundation of the mock-fidelity story (the full mock-vs-real conformance run
    stays tracked as study finding F3b). STATUS's networking section is corrected to say what is
    actually in the store and what each demo reports. Process note for demo prep: the kernel store
    list — not STATUS — is the contract for what can be composed at the metal prompt.

25. **`pci.deny` exists (study 09 finding 4 / plan/12 D62, 2026-06-01).** The deny world in
    wit/pci was specified from the start but the stub crate was never created. It now follows
    the net-l2-deny pattern: exports the full `eo9:pci/pci` surface where `enumerate`/`open`
    answer the API's own `denied` error and every operation on opened devices/BARs/vectors/DMA
    buffers is statically unreachable (uninhabited resource types). In GUEST_COMPONENTS, the
    kernel store (as `pci.deny`), and the eo9-components bundle; the integration test runs
    `text.null $ pci.deny $ lspci` against zero host providers and asserts lspci's own typed
    `denied` failure.

26. **`pci.filtered` is policy-driven; `pci.admit-address` and `pci.admit-vendor` are the standard
    policies (2026-06-01, plan/02 D24, "policies are programs").** The attenuator imports
    `eo9:pci/admit-policy` instead of carrying an allow-list configuration, so which devices a
    driver may see is decided by a composed, fused, wiring-tree-visible policy component:
    `pci.admit-address --allow "[{segment: 0, bus: 0, device: 1, function: 0}]" $ pci.filtered $ lspci`
    (fixed bus addresses — the original behavior) or
    `pci.admit-vendor --allow "[{vendor-id: 6900, device-id: 4096}]" $ pci.filtered $ lspci`
    (vendor:device identity — closes study 09's address-fragility finding: the grant follows what
    the device *is*, not where it sits). Both policies are pure (no capability imports), default
    to deny-all when unconfigured (never-trap rule), and live in the kernel store, so both forms
    run at the metal prompt compiled on-target (verified: 3 visible devices filter down to exactly
    0000:00:01.0 / 1af4:1000 on QEMU aarch64). `open` on the filtered view distinguishes
    `not-found` (no such device) from `denied` (present but refused by policy). Follow-ups:
    (a) the kernel's missing-capability hint maps *any* `eo9:pci/*` residual — including a
    missing admit-policy — to the "add the `pci` token" message, which is misleading when the
    missing thing is the policy middleware (kernel message fix, area 12); (b) the browser /bin
    does not carry the policy stubs yet (recorded, not done).

27. **`fs.filtered` + `fs.policy-subtree`: per-path filesystem attenuation as composed policy
    (2026-06-01, plan/02 D25).** The most-requested attenuator shape — finer than `--fs-root`,
    or `fs.readonly`'s all-or-nothing — now exists as ordinary middleware:
    `fs.policy-subtree --prefix "/docs" --access read-write $ fs.filtered $ program` allows
    operations under `/docs` and denies everything else;  `--access read-only` additionally
    turns mutations inside the subtree into the fs API's own `read-only` error while reads pass
    through. Both stubs are in GUEST_COMPONENTS and the kernel store. Security: both the
    middleware and the policy normalize paths before any prefix comparison (and the middleware
    forwards the normalized path), so `/docs/../secret.txt` is denied by the *policy gate* —
    test-pinned in tests/eo9-integration/tests/fs_filtered.rs (`path_traversal_cannot_escape_the_subtree`),
    along with allow/deny/read-only end-to-end over fs.memfs, deny-all-when-unconfigured, and
    purity. Follow-up: a metal interactive demo and the browser /bin additions ride with the
    later milestones of this branch.

28. **`net.l4.filtered` + `net.policy-ports`: the transport firewall as composed policy
    (2026-06-01, plan/02 D26).** A firewall is now ordinary middleware:
    `net.policy-ports --allow "[80, 443]" $ net.l4.filtered $ program` admits only endpoints
    whose port is on the list (connect remotes, listen/bind locals, and per-datagram send-to
    remotes), answering everything else with the layer's own `denied`. Pure, deny-all when
    unconfigured, in GUEST_COMPONENTS and the kernel store. Test-pinned over `net.l4.loopback`
    (tests/eo9-integration/tests/net_l4_filtered.rs): a permissive policy lets sockcheck's full
    TCP echo + UDP round-trip succeed with every endpoint gated; a restrictive or unconfigured
    policy surfaces as the program's own typed denial. (The end-to-end success test pins the
    loopback's deterministic ephemeral port sequence — documented in the test.) Follow-ups:
    a metal demo over `net.virtio $ net.l4.over-l2` (the firewall composes above the TCP/IP
    middleware unchanged — same l4-in/l4-out shape) and the browser /bin additions.

29. **Policy-swap store/bundle accounting and final verification (2026-06-01, milestone 4 of the
    policy-component swaps).** The kernel store grew 32 -> 38 entries (pci.admit-address,
    pci.admit-vendor, fs.filtered, fs.policy-subtree, net.l4.filtered, net.policy-ports): store
    image 26.5 MB (+~4.0 MB over the 32-entry baseline; precompiled artifacts dominate), aarch64
    kernel image 46.8 MB. All six new stubs are also in GUEST_COMPONENTS and the soundness
    corpus. Final metal smoke (QEMU aarch64, `pci` boot, everything compiled on-target): the fs
    read-only policy forwards reads of the kernel shell fs (`ls` -> listed(2)) and denies
    outside-prefix paths; the firewalled sockcheck full TCP+UDP run echoes over loopback
    (`echoed(24)`); the pci vendor policy still admits exactly one device. Recorded follow-ups:
    browser /bin additions for the six new stubs (the blob's store does not carry them yet);
    the eo9-bundled-programs refresh happens at merge time per the established convention;
    the kernel's missing-capability hint for a missing admit-policy (D26 follow-up (a)).

30. **`net.l2.switch` + `net.l2.echo` + `vnicheck`: the virtual-NIC switch (svc v3 — the
    single-owner-NIC sharing story; docs/design/executor-model.md §6, 2026-06-02).** The switch
    imports ONE upstream `eo9:net/l2` (the physical NIC's driver, or another switch — it
    stacks) and exports two virtual NICs as the **named ports** `port-a`/`port-b` — the first
    multi-export provider (fs.overlay's mirror image), expressible today: named interface
    exports encode with the `implements` relationship (wit/check.sh and the kernel-store
    precompile now handle that — the store precompiles `executable_bytes()`, the same
    stripped form every other compile path uses). Each port is a full l2 with one interface
    (`vnic-a`/`vnic-b`) and its own locally-administered MAC, derived deterministically from a
    configured base (`l2-switch-config`, validated locally-administered + non-multicast;
    documented default `02:e0:09:00:00:00`, ports = base+1/base+2 in the last octet).
    Switching policy, deliberate: sends are forwarded upstream immediately with the port's
    MAC overwriting the Ethernet source (no sibling/uplink spoofing at the Ethernet layer;
    ARP-payload-level anti-spoofing is a recorded follow-up); inbound demuxes by destination
    (broadcast/multicast to every port, own unicast only, unknown unicast **dropped, never
    flooded** — a consumer can never observe its sibling's traffic; no hairpin); bounded
    per-port rx queues (32 frames, drop-oldest = newest-wins), uplink drained only inside
    `recv-frame` (consumer-pull), each drain feeding both queues. Two-slot shape: more
    consumers stack switches (`switch $ switch $ …`); a configured port count is not
    expressible as one component today (a world's exports are static). Wiring is renames —
    `rename port-a link-a $ rename port-b link-b $ net.l2.switch` — and one `$` instantiates
    the switch once, wiring that single instance to every named slot it satisfies (sharing is
    real, not two switches). `net.l2.echo` is the deterministic frame-reflector fixture (ARP
    replies, UDP echo with checksum recompute, broadcast/unknown-unicast probe ethertypes,
    seen-source payload convention); `vnicheck` (named imports `link-a`/`link-b`) verifies the
    whole observable policy in `mode echo` (tests/eo9-integration/tests/vnic_switch.rs: 4
    tests — surface shape, policy suite, configured MAC derivation, bad-base typed refusal)
    and ARP-resolves the QEMU gateway through both ports in `mode arp` — the metal demo over
    `net.virtio` (one physical NIC, two virtual MACs on the wire; `cargo xtask qemu aarch64
    pci netdump` captures the pcap: both `02:e0:09:00:00:01/02` ARP exchanges, each reply
    unicast to its own port, the real MAC never appearing as a source). Kernel store 38 → 40
    (switch + vnicheck; the echo fixture stays usermode-only).

31. **The l4-over-switch limit, pinned (2026-06-02) — and lifted by honest awaits
    (2026-06-02).** As pinned: `net.l4.over-l2` riding a switch port composed and compiled
    but could not run — the middleware drove its l2 import eagerly (single-poll), and the
    switch, a guest whose exports make nested guest-to-guest calls upstream, does not
    complete eagerly under the CM-async callback ABI, so the middleware's poll saw the
    suspension and reported its typed `io` error. Same root cause as study 09's
    `pci.filtered $ disk.virtio $ fs.eofs` failure (GAPS: the suspended-subtask caveat).
    The matrix, updated after the async-first conversion (SPEC, "Boundaries are honestly
    async"; docs/spikes/eager-guest-forwarding.md has the mechanism):
    eager-poller → nested-guest-caller *was* the broken row — **fixed by converting the
    pollers to genuine awaits**, not by making callees eager; awaiting-caller →
    nested-guest-caller worked all along (vnicheck); awaiting-caller → host-leaf-caller
    costs nothing (the await resolves within the call — `net.virtio`'s pci awaits, measured
    identical on metal). `net.virtio` and `net.l4.over-l2` now await their imports
    (`eager()` deleted; both keep every operation deadline and pump/poll bound, and both
    take their state out of its `ProviderState` slot for the duration of an operation so no
    borrow is held across an await — a concurrent activation gets a typed busy error).
    The acceptance test is live (vnic_l4.rs, the typed-failure pin replaced by its own
    acceptance: two transport stacks with distinct IPs complete UDP round-trips over two
    switch ports), and the full payoff ran on metal — kernel store gains `vnic4check`
    (44 → 45) and the demo line is recorded next to its store entry (xtask):
    `net.virtio $ (rename port-a link-a $ rename port-b link-b $ net.l2.switch) $ (rename
    eo9:net/l4 left $ rename eo9:net/l2 link-a $ net.l4.over-l2) $ (rename eo9:net/l4 right
    $ rename eo9:net/l2 link-b $ net.l4.over-l2 --address 10.0.2.16 …) $ vnic4check --peer
    10.0.2.3 --peer-port 53 --mode dns` →
    `ok: verified("left=dns answered (61 bytes) right=dns answered (61 bytes)")` — one
    physical NIC, two virtual MACs, two IP stacks, real DNS on each. Timing on the
    `net.virtio $ net.l4.over-l2 $ l4check` metal demo, eager vs awaited builds, identical
    scripted sessions: operation phase 1.10 s → 1.09 s (no measurable runtime cost; the
    awaits resolve on completion events that arrived anyway), compile phase 29.9 s → 37.6 s
    (the async pump's larger generated state machine costs on-target cranelift time —
    single-sample, TCG-noisy, recorded for honesty not alarm). Per-service virtual NICs at
    the l4 level are unblocked at the composition level; cross-service sharing still waits
    on executor-model §6 stage A/B. Still true: `time.monotonic-stub` panics if its clock
    is *observed* unconfigured (the deny-path tests never reached it) — its lazy documented
    default remains a follow-up for whoever next touches the time stubs; the vnic tests
    configure it explicitly.
32. **`gfx.mem`, `gfx.none`, `gfx.deny`, and the `gpu.virtio` driver (2026-06-02).** The
    standard gfx environment mirrors the disk family: `gfx.mem` is the deterministic RAM
    framebuffer (configured WxH, documented default 640x480, never traps; present/read/clear
    with full bounds checks — out-of-bounds and bad-buffer are typed); `gfx.none`/`gfx.deny`
    are the absence/refusal pair. `gpu.virtio` is the third real device driver (sibling of
    disk.virtio/net.virtio, same probe/bring-up/INTx-with-polled-fallback machinery): claims
    the first virtio-gpu function (0x1af4:0x1050), negotiates VERSION_1 only, drives the 2D
    control queue (GET_DISPLAY_INFO → RESOURCE_CREATE_2D xrgb8888 at scanout 0's geometry →
    RESOURCE_ATTACH_BACKING over one alloc-dma framebuffer → SET_SCANOUT; per present:
    row-copy into the backing at the resource stride, TRANSFER_TO_HOST_2D of the damage rect,
    RESOURCE_FLUSH). `read` answers from the DMA backing — the driver's copy of what was
    presented — so the draw demo's checksum verifies the guest-side data path while QEMU's
    screendump (xtask `check-gpu`) verifies the host-side scanout independently; together the
    two cover the whole pipe. v1 bound: a single-allocation backing caps the mode at the
    provider's 4 MiB DMA limit (1024x768 fits; larger needs multi-entry attach-backing — the
    recorded follow-up). No configure interface: claim-first-on-first-use, like the siblings;
    device selection is `pci.filtered` composed in front.

33. **The storage chain converts to honest awaits; the suspension wall falls for storage
    (2026-06-02, branch `area/14-async-storage`, SPEC "Boundaries are honestly async").**
    `disk.virtio` and `fs.eofs` no longer drive their imports with the eager single-poll: every
    `eo9:pci` and `eo9:disk` call is genuinely awaited (the engine itself went async at the core —
    plan/14 D25), so a downstream that defers suspends the operation instead of failing it. What
    this changes in this plan's ledger:
    - The "provider suspended" `io` error class is gone from `disk.virtio` and `fs.eofs`
      (`pci.filtered`, `fs.filtered`, `fs.overlay` already awaited — verified, no change needed).
    - **D31's interrupt-under-interposition residual is resolved by construction**: the INTx `wait`
      is now awaited rather than single-polled, so a parking wait survives interposition — the
      driver no longer needs its callee chain to complete eagerly. The interrupt-retry and
      poll-spin bounds are retained, so a dead device still surfaces a typed error, never a hang.
    - Provider state for awaiting providers is the take/put `Slot` (`Empty | Busy | Ready`) rather
      than `ProviderState` — borrows never cross awaits, concurrent delivery gets a typed busy
      error, never a re-borrow trap. `disk.virtio`'s synchronous `size` reports 0 until the first
      awaited operation brings the device up; `fs.eofs` wakes it with one read and re-asks
      (plan/14 D25).
    - The l4-over-switch limit (D31) is the *net* lane's instance of the same wall and converts on
      `area/09-net-async` (the parallel branch); the storage acceptance is study 09's
      `pci.filtered $ disk.virtio $ fs.eofs $ cat` on metal.

34. **Cancellation cannot misattribute virtio completions: drain-before-reuse (2026-06-02,
    branch `area/09-cancel-guard`).** The async-storage reviewer's precision note was right:
    `disk.virtio`'s drop-guard resync consumes only completions *already posted* at drop time, so
    a request still in flight at the device when an operation is cancelled (its future dropped
    mid-await — reachable through any deferring pci interposition since the honest-await
    conversion) could complete later and be consumed by the *next* request's wait as its own; with
    the single shared descriptor chain and bounce buffers the device could also DMA torn state
    while the next request rewrites them. Audit verdict for `net.virtio`: the same class exists on
    the **transmit** path (a cancel landing in `send`'s notify await leaves a published — possibly
    unkicked — descriptor over the shared tx bounce buffer: the next `send` would put a corrupted
    copy of its own frame on the wire under the stale descriptor and consume the stale completion
    as its own), while the **receive** path is clean by construction (the used element is read,
    the cursor advanced, and the slot re-published entirely with synchronous DMA accesses before
    the only await; a cancel there can only lose a doorbell, and the published re-post stays in
    the avail ring where the next kick re-delivers it).

    The fix is **drain-before-reuse** (option B; chosen over per-request id tagging because
    tagging alone cannot make shared-buffer reuse safe — only the device *finishing* the old
    request does, and the used element posting is exactly that signal). Each driver keeps its
    free-running published/consumed cursor pair (`avail_index`/`used_index`), level between
    healthy operations; every request-submitting operation (`transfer`, `flush_device`, `send`)
    first settles any divergence — kick once (idempotent, and the cancelled request may never
    have been kicked), then consume the leftover completion with the normal bounded wait
    machinery, discarding it. The drop-guards keep their fast role (consume what is already
    posted; restore the slot; never touch the device beyond a synchronous cursor read). **The
    invariant, under any cancellation/drop timing: when an operation begins writing
    device-shared state, the device has posted completions for every previously published
    request on that queue** (rx pre-posting is exempt — its consumption is await-free), so no
    completion is ever attributed to a request other than the one that produced it. Documented
    side effect, not fixed: a cancel landing inside the INTx `pci::wait` drops the vector with
    the cancelled future and the driver completes later requests in polled mode — graceful
    degradation; a fresh bring-up re-requests the vector.

    Verification, honestly bounded: the drivers only run on metal (no usermode pci provider
    exists), the guest workspace has no test runner, and the cancel window cannot be hit
    deterministically from an SDK consumer (the cancel lands at whatever await the executor
    processes it on; against the kernel root every pci await resolves within the call), so the
    misattribution scenario itself is pinned by this analysis and the code audit rather than an
    executable test. The drain's no-op-on-every-normal-path is what the regression battery
    proves: the metal storage round-trip, the filtered chain, cross-boot persistence, ARP + DNS,
    and the full `cargo xtask ci` gate, all green on this branch. An executable
    cancel-mid-flight probe belongs to the async-hardening lane's matrix machinery once a
    cancellable metal consumer exists.

    **The pattern `gpu.virtio` must follow when it converts to honest awaits** (it is the last
    eager driver): (1) take/put slot + drop-guard restoring on every exit path; (2) per-queue
    free-running published/consumed cursors; (3) drain-before-reuse at the top of every
    submitting operation, kicking before waiting; (4) keep used-element consumption await-free
    so a cancel cannot land mid-consume; (5) accept interrupt-vector loss on cancellation
    (degrade to polled) or restructure to preserve the vector across the wait.

35. **`gpu.virtio` converts to honest awaits — the last eager driver falls (2026-06-02,
    branch `area/09-async-gpu`).** The conversion follows D34's pattern exactly, point by
    point: (1) the `ProviderState` closure state became the take/put `Slot`
    (`Empty | Busy | Ready`) with a `DriverGuard` that restores the driver — framebuffer
    allocation included, preserving the never-drop-DMA rule — on every exit path,
    cancellation included, resyncing the consumed cursor against completions already
    posted (a synchronous DMA read; `Drop` cannot await); (2) the control queue keeps its
    free-running `avail_index`/`used_index` pair; (3) `drain_stale` runs at the top of
    `command()` — the single submitting operation every gfx op funnels through — kicking
    once (idempotent; the cancelled command may never have been kicked) before consuming
    the leftover completion with the normal bounded wait, so the D34 invariant holds: when
    a command begins writing the shared descriptor chain and command/response page, the
    device has posted completions for everything previously published; (4) used-element
    consumption stays await-free; (5) a cancel landing inside the INTx `pci::wait` drops
    the vector with the future — later commands complete polled, graceful degradation. The
    eager `poll_eager`/"provider suspended" machinery is deleted; the unconditional
    ISR-ack-on-completion and the once-per-conversation polled-fallback notice (the gpu-
    freeze branch's fixes) are retained verbatim.

    **Deadline audit** (every parking await bounded, sibling table style): interrupt wait —
    `INTERRUPT_WAIT_RETRIES` (4) per command, each `pci::wait` kernel-bounded, fallback to
    polled; polled loop — `POLL_LIMIT` (50M host calls) → typed `io` error with device
    status; reset spin — 1000 iterations; `drain_stale` — reuses the wait machinery's
    bounds, one iteration per leftover command (≤1: queue depth is one); all other pci
    awaits — host calls the kernel bounds, resolving within the call against the root and
    bounded by the interposer's own deadlines under interposition.

    **The sync-`mode` consequence**: bring-up awaits, so the synchronous `mode` export can
    no longer probe on first use — before bring-up it answers a typed `io` error explaining
    the wake-up dance (the exact shape of `disk.virtio`'s `size` reporting 0, plan/14 D25).
    The draw example wakes device-backed providers with one awaited zero-area `clear`
    before asking for the mode (harmless on gfx.mem; against gfx.deny the wake-up itself
    answers the same typed `denied` the old first-`mode` did — the gfx.deny integration
    test passes unchanged, comment updated).

    Verification: `cargo xtask check-gpu` pixel-exact on both frames; scripted metal
    session — cold draw 13 s with exactly one `codegen: compiling` announce, repeat draw
    2 s with a session-cache hit and no second announce (timing unchanged from the gpu-
    freeze baseline), Ctrl-C landing after `codegen: compiled` (the driver run phase, an
    INTx wait in flight) → `pci: quiesced 1 device(s) at task teardown` + `abnormal:
    killed` → the next `gpu.virtio $ draw` re-claims and presents the correct checksum
    over INTx; full `cargo xtask ci` green. Same honesty bound as D34: the cancel-mid-
    flight misattribution window itself is pinned by the pattern and audit, not an
    executable test, until the hardening lane grows a cancellable metal consumer.

36. **ISR-ack alignment across the virtio drivers (2026-06-02, branch `area/09-async-gpu`).**
    The gpu-freeze branch's unconditional read-to-clear acknowledgement, examined for the
    siblings:
    - **`disk.virtio` — ported.** Its `wait_for_completion` acked only on the wait branch and
      in the polled loop; a completion already posted before (or between) interrupt waits
      returned without an ISR read, leaving the level-sensitive INTx asserted — the next wait
      then sees a stale delivery and retries spuriously (worst case: permanent drift into the
      polled fallback). The ack now runs unconditionally on the completed branch. Safe at
      this driver's queue depth of one: by the time the branch runs, every published
      request's completion has been consumed, so a pending assertion can only belong to the
      completion just consumed — there is no other in-flight request whose interrupt the
      read-to-clear could swallow; double-acking on the wait branch is idempotent.
    - **`net.virtio` — nothing to ack; the inverse fix instead.** The driver is purely polled
      (no `enable-interrupts`, no ISR window — interrupt receive is the plan/12 D59
      follow-up), so the stale-delivery shape cannot occur *in* it. But it carried the no-ack
      shape's mirror image: avail flags 0 invited the device to assert its level-triggered
      INTx on every rx/tx completion with nobody ever reading the ISR — a permanently wedged
      line, harmless to net.virtio itself but hostile to any device sharing the swizzled
      INTx (an interrupt-mode sibling would see endless stale deliveries and drift into its
      polled fallback). Both queues now publish `VIRTQ_AVAIL_F_NO_INTERRUPT` (virtio 1.0
      §2.6.7) at setup — the spec's polled-driver discipline; a hint, so a device that
      interrupts anyway costs nothing. The rx path needs no per-completion reasoning beyond
      this: its used-element consumption is await-free and flag state is set once at queue
      init, before any buffer is posted. When D59 converts receive to interrupt waits, the
      rx flag goes back to 0 and the converted driver acks like the siblings.
    Verification (the full battery, one boot with all three functions on one bus — the
    line-sharing scenario the net suppression exists for): disk round-trip
    (`ok: round-tripped(15)`, completion: INTx), the filtered chain (`pci.admit-vendor $
    pci.filtered $ disk.virtio $ fs.eofs $ cat` — INTx through the filter), net ARP + DNS
    (`l2check` resolved the gateway MAC; `l4check` resolved example.com), `gpu.virtio $
    draw` with the correct checksum on the shared bus; then a power-cycle boot reading the
    file back (cross-boot persistence, INTx). INTx waits genuinely served in both boots
    (the kernel's once-per-boot delivery line), zero polled fallbacks. Full `cargo xtask
    ci` green.

37. **Switch-over-switch stacking, retested after the conversions — runs, and pins the
    residual wall (2026-06-02).** The two-layer stack (`net.l2.echo $ rename port-a
    eo9:net/l2 $ net.l2.switch $ rename port-a link-a $ rename port-b link-b $
    net.l2.switch $ vnicheck`) composes (a port export renamed onto the default slot
    satisfies another switch's uplink; the unused sibling port drops), compiles, spawns,
    and **runs to a typed program outcome** — one layer further than the pre-conversion
    state, where the *middleware's* eager poll failed at the consumer-over-switch edge
    (D31's lifted wall stays lifted). The residual: `net.l2.switch` itself still drives
    its uplink with the `eager()` single-poll (its header even cites the now-deleted
    middleware pattern as precedent). Over a leaf upstream (echo, `net.virtio`) the
    eager poll completes — every existing switch test and the metal demos are
    unaffected — but an inner *switch* is a nested-guest-caller whose exports suspend,
    so the outer switch's first uplink operation reports its typed
    `io("list-interfaces: the upstream l2 provider suspended")` through vnicheck's
    failure channel. Pinned behaviorally in
    `tests/eo9-integration/tests/vnic_stacked.rs` (2 tests: the algebra-level seal, the
    typed run outcome). The fix is the established D31/D33 pattern — delete `eager()`,
    await the uplink, keep operations deadline-bound — owned by whoever next touches the
    switch (not taken here: the switch is in the kernel store and the browser /bin
    catch-up is concurrently rebuilding www assets; a byte change mid-flight doubles the
    refresh coordination for a fix that deserves its own focused pass).

    Recorded for that pass, so nobody expects the full vnicheck echo suite to go green
    on awaits alone: the switch's unconditional source rewrite collapses both outer
    ports onto the inner port's single MAC on the way upstream (a MAC-NAT with no
    reverse mapping). Echo's replies address that inner-port MAC, so the outer switch
    can demux them to at most one port — under default bases they all land on `port-a`
    (whose MAC coincidentally equals the inner `port-a`'s); under distinct bases they
    are unknown unicast at the outer layer and are dropped. Honest awaits make stacked
    switches *run*; stacked *fan-out* (distinct consumers behind a stacked port both
    completing request/reply flows) additionally needs a policy decision — e.g. a
    reverse mapping for rewritten sources, or a learn-don't-rewrite stance toward a
    downlink that is itself a switch — which is an owner-facing design question, not
    plumbing.

38. **`time.monotonic-stub` joins the option-C default-configuration rule (2026-06-02,
    closing D31's recorded follow-up).** Observing the stub's clock unconfigured used to
    panic (`ProviderState::with` on an unbound state — a never-trap convention violation,
    reachable by plain `time.monotonic-stub $ program`). It now self-binds its documented
    default on first use, exactly the `time.frozen` pattern: start 0 ns, step 1 ms per
    observation (`DEFAULT_START_NS`/`DEFAULT_STEP_NS` in the stub); `configure` (or the
    shell's `--start-ns`/`--step-ns`) still overrides. Tests in
    `default_configuration.rs` (+2: the unconfigured origin via `hello`, the configured
    override). With this, every configurable stub in the store follows the rule.

39. **The cancel-mid-flight probe is executable — and it root-causes why the window is
    closed on metal today (2026-06-02, D34's recorded follow-up).** `cancelcheck`
    (guest/examples; kernel store 50 → 51) is the consumer D34 said did not exist. Per
    attempt it starts a 1 MiB read, races a small concurrent read against it, and
    classifies precisely: the driver's take/put slot makes the concurrent read's typed
    busy error a *proof* the first read is mid-flight at that instant, and the cancel —
    the SDK's cancel-on-drop, a real `subtask.cancel`, the bindings' EVENT_CANCEL →
    destructors → `task.cancel` acknowledgment — lands without an intervening yield, so
    a busy-classified attempt cannot race shut. After every attempt both seeded regions
    are re-read and compared byte-for-byte: any leftover completion credited to a later
    read (the misattribution D34's drain-before-reuse forbids) is a typed `corruption`
    failure carrying the offset. Usermode pins the machinery
    (tests/eo9-integration/tests/cancel_probe.rs over `disk.mem`: no traps, no hangs,
    honest all-miss classification); on metal the full chain ran end-to-end —
    `disk.virtio $ cancelcheck --attempts 25` at the eosh prompt, on-target compile,
    real virtio-blk → `ok: probed("attempts=25 hits=0 eager=25 data-miss=0")`, zero
    corruption across all 50 verification sweeps.

    `hits=0` is itself the finding. Two layers close the window, and we eliminated one
    to expose the other: QEMU single-threaded TCG completes virtio-blk requests
    synchronously under the queue-notify write, so the `disk` flag now gives the scratch
    disk its own iothread (xtask; completions post asynchronously, like hardware) — and
    the hit rate stayed zero, because the kernel's `eo9:pci/pci.wait` **blocks
    host-side** (masked-`wfi` inside the host call, pci_provider.rs — documented as
    deliberate when every driver was an eager poller, D16/D18, with the suspending wait
    deferred "until the async disk/net bridge lands"). That rationale is now stale: the
    drivers await honestly (D33), and the host-side block halts *every* task between a
    driver's request publish and its interrupt, so no canceller can be scheduled
    mid-flight — the window is structurally unconstructible on metal regardless of
    consumer or device timing. The runtime support that opens it is exactly the doc
    comment's own deferred plan, now unblocked: a task-suspending `pci.wait` that parks
    on the drive loop the way `time.sleep` does (the async-first doctrine applied to the
    kernel host APIs — owner-visible, kernel-lane work, not taken in this tail batch).
    The probe is already in the store with its verification sweeps waiting; when the
    wait parks, `cancelcheck` starts hitting with no further changes, on QEMU and on the
    Orange Pi alike.

40. **`net.l2.switch` awaits honestly — stacking runs point-to-point (2026-06-02, branch
    `area/09-switch-convert`).** The last `eager()` component converted per the async-first
    doctrine: every uplink call is a genuine await. The one-uplink/two-ports shape gets the
    sibling-no-wedge design: the uplink lives in a take/put slot with a claim flag (the
    l4 middleware's `opened` pattern — a concurrent first use can never open a second
    upstream interface, and the guard restores slot + claim on every exit path including a
    future dropped mid-await); while one port holds it parked, the sibling's `recv-frame`
    serves its own queue (whichever port pumps demuxes into BOTH queues) and answers
    "nothing waiting" when empty — the consumer owns the retry, per the l2 contract — while
    `send-frame`/`list-interfaces`/`open-interface` answer a typed busy error; the sync
    `info` (no error channel) reads link parameters cached at bring-up and never touches
    the uplink (the disk size-reports-0 shape). Deadline audit: every await is one upstream
    l2 op, bounded by the upstream's own contract (an idle link answers `bytes-received: 0`
    rather than waiting for traffic), with `DRAIN_BATCH` (16) capping awaits per drain — the
    switch never waits for a frame to arrive. Verified: vnic_switch 4/4 unchanged;
    vnic_stacked flipped from the D37 suspension pin to THREE tests — the algebra seal, the
    point-to-point SUCCESS (`vnicheck --mode through`: a full port-A exchange through two
    stacked switches — both source rewrites up, demux back down through both layers via the
    deterministic MAC derivation (layer N's port-a equals layer N+1's), sibling isolation
    through the stack, broadcast to both outer ports, unknown unicast to neither), and the
    fan-out limitation pinned typed (`--mode echo`: port B's reply collapses onto the inner
    port-a MAC and demuxes to A — the D37 MAC-NAT/reverse-mapping owner question, untouched
    here by directive). vnicheck gained the point-to-point modes `through` and `arp-a` (the
    A-half of `arp`) for stacked verification. Metal: the single-switch `--mode arp` payoff
    byte-for-behavior (both MACs, gateway resolved), and the NEW two-switch smoke —
    `net.virtio $ inner $ outer $ vnicheck --mode arp-a` → `verified("mac-a=02:e0:09:00:00:01
    gw-a=52:55:0a:00:02:02")` — ARP through a stacked pair to the real gateway, clean
    poweroff. Full `cargo xtask ci` green.

41. **Bring-up claims are guarded from the instant they exist (2026-06-02, branch
    `area/09-bringup-guards`).** The switch-convert review noted the residual: every
    converted provider set its bring-up claim (`Slot::Busy` in disk.virtio/gpu.virtio/
    fs.eofs, `brought_up` in net.virtio, `opened` in net.l4.over-l2, `claimed` in
    net.l2.switch) *before* the first await of bring-up, but armed its restore guard only
    *after* bring-up completed — so a future dropped mid-bring-up leaked the claim and
    wedged the instance behind the typed busy answer forever (error returns restored;
    cancellation did not). Unreachable today (no shipped path cancels mid-bring-up), but
    live the day a bring-up step parks — and D39's `pci.wait` parking makes exactly that
    reachable: `cancelcheck`'s first-attempt cancel can land inside bring-up's INTx wait,
    which without this fix would wedge every subsequent attempt. The sweep covers SIX
    stubs (the review's five plus fs.eofs, whose mount is the deepest awaiter of all):
    each gains a `BringUpClaim` guard armed immediately after the claim transition and
    defused on success when the operation guard takes over; the explicit error-path
    clears collapse into the same mechanism (one restore for error-return and
    future-drop alike). Drop-ordering note: defuse runs in the same synchronous poll
    segment as bring-up's final await, so no drop window exists in the handoff. A
    drop-mid-bring-up is not constructible from the usermode harness (host providers
    complete eagerly; kill destroys the whole instance), so the pin is the audit plus
    the full suites; the executable probe arrives with D39 (cancelcheck over a parked
    bring-up). Verified: vnic_switch/vnic_stacked/vnic_l4/net_l4_over_l2/eofs/
    pci_filtered/gfx suites green (the deny suites exercise the error-path restore
    repeatedly), metal smokes per family, full `cargo xtask ci` green.

42. **`net.l2.bridge` — the 802.1D learning bridge; D37 resolves as separate providers
    (owner ruling 2026-06-03, branch `area/09-l2-bridge`).** The stacked-fan-out
    question (D37) is not a tuning knob on the switch: the owner ruled the candidate
    semantics — reverse-mapping MAC-NAT, learn-don't-rewrite, point-to-point-only,
    wider switches — are *separate providers*, and the first to ship is the classic
    802.1D bridge. The capability stance is the headline: the switch is an
    identity-ENFORCING attenuator (source rewrite = consumers cannot spoof); the
    bridge is a transparent segment that TRUSTS its ports (no rewrite = a bridge port
    carries the ability to claim any link-layer identity on the segment). Choosing
    which to compose is the composer's security decision; the WIT docs on both worlds
    say so loudly. The switch keeps its rewrite/point-to-point semantics untouched;
    learning-NAT and wider static switches stay deferred until a topology demands
    them.

    The provider (`guest/stubs/net-l2-bridge`, world `eo9:net/l2-bridge`): one
    upstream `eo9:net/l2` import + the named ports `port-a`/`port-b`, the switch's
    exact wiring shape, so compositions swap one provider for the other freely.
    Forwarding is classic 802.1D with the upstream as just another port: learn every
    ingress source (learn-then-lookup, so an eviction caused by the current frame's
    own source is visible to its own lookup); known unicast to the learned port alone
    — including local port-to-port delivery the upstream never sees; unknown unicast
    FLOODED to every other port (the deliberate opposite of the switch's drop policy;
    flooding is how learning converges); broadcast/multicast to every other port;
    never reflect to the ingress port. The learning table is bounded (64) with
    least-recently-LEARNED eviction instead of time-based aging — eviction follows
    source sightings, the same events that reset a real bridge's aging timer, but
    with no clock import the provider stays pure and deterministic (the documented
    trade: an idle station's entry survives until table pressure evicts it). No STP:
    composition wiring is a DAG, loops are unconstructible by the algebra. Delivery
    is atomic: a forward needing the upstream acquires it first, local copies enqueue
    only after the upstream send succeeds, so a typed failure means nothing was
    delivered. Advertised port MACs are a *suggestion* (consumers like the l4
    middleware source from `interface-info.mac`; the bridge never checks): derived
    base+1/+2 from `l2-bridge-config`, default `02:e0:09:00:01:00` — deliberately
    distinct from the switch's default so mixed stacks at defaults never advertise
    colliding MACs. Async discipline per D40/D41 from day one: honest awaits, the
    take/put uplink slot with claim flag and bring-up guard, sibling-no-wedge,
    bounded drains, typed busy errors.

    Verified, usermode (`vnic_bridge.rs` 6, `vnic_bridge_stacked.rs` 4, all green):
    the full policy suite over the echo fixture via the new `bridgecheck` example
    (custom consumer MACs carried unrewritten; flood-before/unicast-after learning;
    local delivery proven by upstream silence; broadcast + unknown-unicast flooding —
    the behavioral line vs. the switch, whose same probe `vnicheck` pins as
    delivered-to-NEITHER; MAC migration both directions); the bounded table both ways
    (the 65th distinct source evicts the least-recently-learned entry, observable as
    the probe flooding upstream; the 64-source control keeps it local); configured +
    default advertised-MAC derivation and the typed bad-base refusal. Stacking: the
    fan-out payoff — BOTH consumers' custom-MAC exchanges complete through two
    stacked bridges (the exact shape `vnic_stacked.rs` pins as the switch's MAC-NAT
    typed failure). The mixed matrix: switch-under-a-bridge passes the FULL
    `vnicheck --mode echo` suite verbatim (the switch's rewritten port MACs are just
    stations to the bridge; its drop policy still protects its consumers from the
    bridge's floods — each provider keeps its contract through the other);
    bridge-under-a-switch composes and runs but the switch's rewrite collapses every
    station behind the bridge onto one port identity, replies flood, and
    bridgecheck's typed check failure pins it (the switch enforcing its contract —
    fan-out compositions put the bridge on top).

    Verified, metal (QEMU aarch64, all compiled on-target): `net.virtio $ (rename
    port-a link-a $ rename port-b link-b $ net.l2.bridge) $ vnicheck --mode arp` →
    `verified("mac-a=02:e0:09:00:01:01 gw-a=52:55:0a:00:02:02 mac-b=02:e0:09:00:01:02
    gw-b=52:55:0a:00:02:02")`, with the `netdump` pcap showing both advertised MACs
    as Ethernet sources on the wire and ZERO frames sourced by the NIC's real MAC —
    transparency, photographed (slirp note: QEMU user-net answers ARP for arbitrary
    source MACs, no promiscuous quirk); the two-stack transport payoff over bridge
    ports → `verified("left=dns answered (61 bytes) right=dns answered (61 bytes)")`
    (384 KiB single-bridge composition compiled in 37.6 s, the 6-component dns one in
    200.8 s); and the metal fan-out the switch structurally cannot do —
    bridge-over-bridge with the FULL `--mode arp` (both ports through the stacked
    pair; the switch's stacked smoke is `arp-a`, port A alone) → both gateways
    verified, 74.1 s compile, clean poweroff. Kernel store 51→52
    (`net.l2.bridge`); `bridgecheck` is usermode-only (GUEST_COMPONENTS, not the
    kernel store). Full `cargo xtask ci` green per milestone.

43. **`gpu.virtio $ draw` latency benchmarked — the graphics pipeline is not the cost
    (2026-06-04, branch area/09-draw-bench).** Owner TODO. Phase-marked runs on QEMU
    aarch64 (docs/spikes/draw-latency.md has the full tables): cold = 12.0 s of which
    11.4 s is the announced on-target codegen of the 244 KiB composition (session- and
    storedisk-cached; 2.1 s for the 136 KiB gfx.mem composition — tracks size); warm =
    389 ms of which 339 ms (87%) is spawn/instantiate machinery before the program's
    first instruction, scaling near-proportionally with fused-component size; the
    entire device conversation (full-frame clear + 1.2 MB present with per-row DMA +
    TRANSFER/FLUSH + INTx + 1.2 MB readback) totals ~13 ms warm, so the per-row-DMA
    batching idea is measured irrelevant at 640x480 and `present` already transfers
    only the damage rect. A cocoa `display` window changes nothing. Native baseline:
    the identical workload is 130 ms warm / 590 ms cold in release usermode — TCG is
    a 3x (warm) to 23x (compile) multiplier, so on real hardware warm draw is sub-100 ms
    and cold is ~half a second. Recommendation recorded (not implemented, kernel lane):
    profile the 339 ms spawn path — hash-equality memcmp, pre-instantiation validation,
    per-spawn linker construction — if warm spawn latency ever matters beyond TCG demos.
    No gpu.virtio or draw changes warranted.

44. **`net.text` — the socket-backed text provider: shell over the network as pure composition
    (2026-06-07, branch area/09-telnetd).** Owner directive: "target shell-over-network. Telnet is
    fine, no need for SSH yet." The design goal was telnetd-accepts-then-hands-the-connection-to-a-
    composed-child; the investigation pinned why that exact handle plumbing is impossible today, and
    what the Eo9-shaped alternative is:

    *The handle-transfer finding (the load-bearing design decision).* A component-typed `main`
    argument (plan/04 D14) transfers because a component value is **passive bytes** moved between two
    host-side exec tables. An accepted `eo9:net/l4.tcp-connection` is **live state inside the
    spawner's own store**: the l4 provider is composed wasm (`net.l4.over-l2`'s smoltcp instance in
    the spawner's linear memory) — there is no host net provider, no host table to take the handle
    from, and no cross-store call brokering (that is the Message API, deliberately not yet designed).
    Wasm task stores are isolated; one NIC is claimable by one task at a time. Consequences, pinned:
    a per-connection child task cannot receive the spawner's connection; concurrent network sessions
    in separate tasks would each need their own NIC+stack. **Chosen mechanism:** the connection never
    crosses a task boundary — `net.text` owns listen+accept *inside the fused session task*
    (`net.virtio $ net.l4.over-l2 $ net.text $ eosh` is one task), and the supervisor (`telnetd`,
    plan/10) serves sessions *sequentially*, one fused task per session, respawning the same compiled
    image. Bounded-concurrency-of-4 from the area brief is therefore deferred to the Message API (or
    a host-side per-task text broker): recorded here so it is not silently dropped.

    *The stub.* `guest/stubs/net-text` (crate-local world `eo9:net-text/net-text`, the over-l2
    cross-package precedent: imports `eo9:net/l4`, exports `eo9:text/{types,text,net-text-config}`).
    `configure(port: u16)` — additive `net-text-config` interface in `wit/text` (port 0 is a
    configure error); unconfigured default port 23, bound lazily on first `read-line` (option-C
    plain-composition rule). One session per instance: after accepting, the listener is **dropped**,
    so extra connection attempts are refused by the transport itself (TCP RST — immediate and
    deterministic). Refuse-with-a-message was considered and rejected for v1: l4 `accept` carries a
    fixed multi-second deadline and no quick-poll flavor, so interleaving accept with the session's
    recv would add up to one accept-deadline of input latency per idle cycle; recorded as a possible
    follow-up if the WIT ever grows caller-supplied deadlines (the F8 open call).

    *NVT.* Refuse-all telnet negotiation (WILL→DONT, DO→WONT, negatives never answered — the RFC 854
    loop rule; subnegotiations skipped; IAC IAC literal; everything else of telnet unimplemented and
    documented as such: no ECHO/SGA/NAWS, no urgent data — clients local-echo, which is what nc and
    line-mode telnet do anyway). CR LF / CR NUL / bare CR / bare LF all end a line; 4 KiB line cap;
    lossy UTF-8. `write` is sync in the text WIT while the transport is async, so output is buffered
    (64 KiB cap, drop-and-flag-once) and delivered at `read-line` boundaries — exactly right for a
    prompt-driven consumer (eosh writes its prompt immediately before reading); a write-only program
    composed over net.text would never connect, documented.

    *Session end — and the l4 close gap (workaround, ledger-worthy).* Peer close → `read-line`
    answers `none` → eosh exits cleanly. The line `exit` is intercepted at the NVT layer: goodbye,
    FIN, then `none` — because once the consumer exits, nothing ever runs the provider again to
    perform the close handshake, and a task-store teardown sends no FIN (the client would hang).
    Flushing the FIN itself needs a pump, and l4 has **no explicit close/flush operation** — the
    workaround is a bounded throwaway `accept` on an ephemeral listener (its deadline pumps the
    FIN/ACK exchange). GAP recorded: `eo9:net/l4` wants a `close: async func(tcp-connection)`
    (graceful shutdown as a first-class await); until then every l4 consumer that must close cleanly
    after its last real operation needs this trick. Remote `poweroff` is *not* intercepted: it
    propagates as the fused task's outcome and the supervisor refuses it (plan/10).

    **SECURITY, said loudly here as in the WIT and the crate header: cleartext, unauthenticated.
    Whoever reaches the port owns the session. Trusted-LAN/dev tool only; SSH explicitly deferred.**

45. **Shell-over-network verified end to end under QEMU; usermode and board lanes assessed
    (2026-06-07, branch area/09-telnetd; completes D44 and plan/10 entry 20).** xtask grows two
    pieces: the bare `telnet` qemu token (implies `net`; adds `hostfwd=tcp:127.0.0.1:5555-:23` to
    the slirp netdev, so the guest's port 23 is `nc localhost 5555` from the host — loopback-bound:
    the unauthenticated session must never be reachable beyond the dev machine) and `check-telnet`, the
    scripted gate modeled on `check-gpu` (piped serial + a host-side TCP client in place of the QMP
    socket; D49 byte-paced console typing; per-step timeouts; transcripts printed).

    Verified, metal (QEMU aarch64, `pci` boot grant, `telnetd --sessions 2` typed at the serial
    prompt; the fused 4-component session compiled on-target once, spawned per session) — the
    repo's FIRST inbound-TCP validation over a live link (the D21 gap, now exercised: slirp SYN →
    smoltcp listen/accept through net.virtio):

    * session 1: host connect → greeting first on the wire (`eo9 net.text: cleartext telnet
      session - unauthenticated; trusted networks only`), then the eosh banner and prompt;
      `hello` over the socket → `ok: greeted` + the next prompt (the child's own stdout lands on
      the serial console — the recorded per-task text gap); a CONCURRENT second connection while
      session 1 is live is refused by the transport (listener dropped after accept → smoltcp RST →
      slirp closes the host side; 0 bytes seen, no prompt served); `exit` → goodbye line → FIN →
      host sees EOF (the D44 close-handshake pump, working).
    * session 2: fresh task, NIC re-claimed (PCI quiesce-on-teardown + reset-on-bring-up held
      across sequential sessions), independent greeting/banner/prompt, `exit` closes cleanly.
    * telnetd narrates each session on serial, refuses-by-policy nothing it shouldn't, exits
      `ok: served(2)`; console exit powers the machine off; `cargo xtask check-telnet` exits 0.

    *Usermode parity: deferred, recorded.* The `eo9` CLI links no l4 root provider (plan/11: "disk
    and net are still not linked") and the only self-contained l4 is `net.l4.loopback`, which by
    design nothing outside the process can reach — a usermode telnetd would serve sessions no
    external client can connect to. Parity waits on a host unix-l4 provider (or l2-over-tap);
    nothing in net.text/telnetd is kernel-specific (they speak only eo9:net/l4 + text + fs + exec).

    *Board notes (RTL8125, Orange Pi 5 Plus lane).* Nothing above l2 changes: swap `net.virtio`
    for an RTL8125 l2 driver claiming through eo9:pci and the same
    `<l2-driver> $ net.l4.over-l2 $ net.text $ eosh` serves the bench LAN (addressing via
    `net.l4.over-l2 --address …` for the bench layout). On a real LAN the D44 security posture
    stops being theoretical: port 23, cleartext, unauthenticated — bench/trusted-LAN only, and the
    bench bring-up must not touch the serial-loader port rules (boards/BOOT.md).

    *Also fixed while validating:* the greeting is now PREPENDED to net.text's pending output at
    accept time, so it precedes the banner/prompt the shell buffered while no connection existed
    (first transcript had it after the prompt — wire order now reads greeting → banner → prompt).

46. **`net.rtl8125` — the RTL8125 2.5GbE driver: the board's trained PCIe links carry real
    packets (2026-06-08, branch area/09-rtl8125; the convergence lane of plan/12's Orange Pi 5
    Plus bring-up).** A guest component importing `eo9:pci/pci` (+ text for one diagnostic
    line), exporting `eo9:net/l2` — net.virtio's real-silicon sibling, with every discipline
    carried over verbatim: the take/put driver slot + cancellation-safe guard, the bring-up
    claim (D41), drain-before-reuse on the transmit ring (D34, the `tx_published`/`tx_consumed`
    cursor pair), bounded polls everywhere (reset, PHY OCP, link wait, tx completion, the
    study-08-F2 short receive poll), typed errors never traps, and the "empty result = nothing
    waiting" receive contract.

    *Device model (the citation rule, every constant sourced).* The driver follows mainline
    Linux r8169 — which drives the RTL8125 family with the **legacy 16-byte descriptor rings**
    (opts1/opts2/addr), not the vendor driver's 32-byte v3 format — cross-checked against
    Realtek r8125 and OpenBSD rge(4) where the 8125 diverges (IMR/ISR at 0x38/0x3c dword,
    TxPoll_8125 doorbell at 0x90, GPHY_OCP MDIO window at 0xb8, the 2.5G advertisement in PHY
    OCP 0xa5d4 bit 7, PHYstatus 0x6c bit 10 for 2500M). All of that lives in
    `crates/eo9-rtl8125`, a pure no_std crate in the host workspace: register map, descriptor
    encode/decode, GPHY command words — 10 host unit tests pin the encodings (the
    eofs/eosh-core precedent: the component is a thin I/O shell, the bit arithmetic is
    host-tested). Rings are 32/32 in one `alloc-dma` page (256-byte alignment checked, typed);
    receive slots are 32 × 2 KiB with `RxMaxSize` = slot size so frames never span slots;
    transmit pads to the 60-byte minimum in its bounce buffer rather than trusting hardware
    padding (the first acceptance frame is a 42-byte ARP). Bring-up: MAC read → soft reset →
    IMR 0 + ISR ack (the polled driver's ISR-suppression discipline in this device's dialect:
    with the mask clear the NIC never asserts INTx) → PHY autoneg (10/100 + 1000FD + 2500FD)
    with a bounded link wait that does NOT fail bring-up — link state is typed
    (`info.up`/`link-down`) and re-read by later operations, so a slow negotiation just means
    a retried check. Promiscuous OFF; **multicast = none for v1** (recorded: ARP/IPv4 need
    broadcast + unicast only; all-multi is one RxConfig bit away when IPv6 ND arrives).
    Deliberately omitted, recorded as first suspects if board traffic misbehaves: the
    reference drivers' MAC-OCP errata pokes / MCU patch tables / EEE tuning, and the 8125B's
    RX_PAUSE_SLOT_ON.

    *INTx assessment (the rk3588_pcie design-note follow-up): polled v1, demux deferred.*
    Wiring SPIs 245/250 is not the small path: each DW controller folds all four pins onto ONE
    edge-rising SPI demuxed by an APB status read (+ hiword mask register), so the kernel
    would need per-(segment, line) mask/record state where the shared swizzle model carries
    four global level-triggered gpex lines, an APB read in the IRQ path, and edge-ack
    semantics the level-oriented mask/unmask contract does not express — a real kernel
    sub-lane with QEMU-path regression risk (`pci_intx` stays `WIRED = false` on the board;
    the provider answers `unsupported`; the QEMU INTx path is untouched — disk.virtio's
    interrupt round-trip stays green). The driver is polled with honest bounds by design and
    by measurement parity with net.virtio; the kernel INTx demux is the recorded follow-up
    alongside D59's interrupt-receive conversion.

    *What QEMU can and cannot validate.* QEMU has no RTL8125 model (rtl8139 is its newest
    Realtek NIC), so emulated coverage is exactly: composition shape (l2 sealed, pci the
    residual), `pci.deny` underneath surfacing as the consumer's typed failure (both pinned in
    tests/eo9-integration/tests/net_rtl8125.rs), and the live metal probe — verified at the
    QEMU aarch64 eosh prompt: `net.rtl8125 $ l2check` compiled on-target (9.3 s) and refused
    typed, naming the identity (`no RTL8125 (10ec:8125) function is visible…`), prompt intact
    after. Everything past the probe is board-validated by the planner (this lane never
    touches the serial port).

    *Board deliverables.* The opi5plus **minimal** store grows from {hello, lspci} to the
    acceptance set {hello, lspci, net.rtl8125, l2check, net.l4.over-l2, l4check, net.text,
    telnetd, eosh} — 9 components, 10.6 MiB store, 22.2 MiB image (the full image carries all
    56). The acceptance compositions run at the serial eosh prompt (boot grant `pci` in
    bootargs; the headless `program=` runner links only kernel root providers, so composed
    stacks are the shell's job — same shape as check-telnet). Three examples grew the minimal
    arguments the bench needs, defaults preserving QEMU behavior exactly: `l2check
    --gateway <ip>` (ARP target, default 10.0.2.2), `l4check --resolver <ip> --probe <ip>`
    (DNS server / TCP-probe host, defaults 10.0.2.3 / 10.0.2.2), `telnetd --nic <name>
    --address <ip> --prefix-length <n> --gateway <ip>` (the NIC the session stack composes
    over, default net.virtio, plus static addressing baked into net.l4.over-l2's configure;
    address+gateway travel together, prefix defaults to 24). The bench ladder (owner's LAN:
    board 10.20.3.70/24, gateway 10.20.3.1, Mac client 10.20.3.108):

    * (a) link layer: `net.rtl8125 $ l2check --gateway 10.20.3.1` → the router's MAC.
    * (b) transport: `net.rtl8125 $ (net.l4.over-l2 --address 10.20.3.70 --prefix-length 24
      --gateway 10.20.3.1) $ l4check --resolver 10.20.3.1 --probe 10.20.3.1` → an A record
      for example.com + a typed TCP outcome. (No DHCP exists anywhere in the stack — static
      config is the design, not a gap.)
    * (c) the prize: `telnetd --nic net.rtl8125 --address 10.20.3.70 --gateway 10.20.3.1`,
      then `telnet 10.20.3.70` from the Mac (port 23; cleartext, trusted-LAN only per D44).

    Also in this lane (bench tooling): the serial-loader sender's mid-transfer stall alarm now
    fires on **no ack progress for >10 s regardless of which loop holds control** (one
    progress clock checked on every pass of both loops, wall-clock so a host sleep trips it on
    wake) plus a serial `write_timeout` so a post-sleep wedged port driver cannot hang the
    sender inside `write()` — the 2026-06-07 incident was per-loop-entry deadlines being
    re-armed forever (boards/opi5-serial-loader/tools/send_image.py).

    Verified: full `cargo xtask ci` green per milestone; `cargo test -p eo9-rtl8125` (10 ring/
    PHY-word tests); the two usermode composition tests; the QEMU typed-refusal probe; bundle
    refreshed (77 components); both board images rebuilt. QEMU regression sweep, all green:
    `check-telnet` (telnetd's bare invocation unchanged — two sessions, concurrent refusal,
    clean closes), the three arch demos canonical (aarch64/riscv64/x86_64 `demo` boots run to
    their power-off lines), and live at the QEMU metal prompt both `net.virtio $ l2check`
    (bare — 10.0.2.2 resolved, net.virtio byte-identical in this lane) and
    `net.virtio $ l2check --gateway 10.0.2.2` (the new typed option through eosh). Board
    acceptance is the planner's bench run — recorded here as pending.

    *Board round 1 (2026-06-08, planner bench) → the OOB ownership fix.* Wins: PCIe bring-up
    clean, PHY autoneg completed (`link up 2500 Mb/s`), factory MAC read (c0:74:2b:f8:22:33),
    rings programmed, the composed stack compiled on-target in 5.5 s. Blocker: transmit
    descriptors were never consumed (typed `io` after the bounded poll) despite link-up and
    bus mastering — the recorded first-suspect confirmed: **the RTL8125 powers up under its
    embedded MCU's OOB/management ownership and ignores host descriptor rings until the
    driver takes ownership.** The fix (every step cited in `crates/eo9-rtl8125`, references
    fetched verbatim — mainline v6.12 `r8169_main.c` and OpenBSD `if_rge.c`, which agree on
    every ownership register): RealWoW off (MAC OCP 0xc0bc=0x00ff), quiesce (accept bits
    cleared, RXDV gate set — byte 0xf2 bit 3 == MISC bit 19, StopReq + FIFO-empty wait on
    MCU 0xd3 bits 5/4), soft reset, `NOW_IS_OOB` cleared (MCU 0xd3 bit 7), the link-list
    handover (0xe8de bit 14 cleared + LINK_LIST_RDY waits on MCU bit 1, the three link-list
    parameters c0aa/c0a6/c01e), then `rtl_hw_start_8125(_common)`: coalescing off
    (INT_CFG0/INT_CFG1 + the 0xa00 block), Rdy_to_L23 off, UPS off, **the
    legacy-descriptor-format select (MAC OCP 0xeb58 bit 0 cleared — without it the chip
    parses the rings as the vendor 32-byte format; the other prime TX suspect)**, the cited
    tuning writes with the A-vs-B arms picked by XID (`(TxConfig>>20)&0xfcf`; 0x609=A,
    0x641=B — the board parts), the 0xe098/0xe00e start handshake, RXDV gate released. Ring
    bases now write the high dword first (the reference's in-tree ARM note). Still omitted,
    recorded: EPHY (PCIe SerDes) tuning tables, EEE MAC config, CPlusCmd, firmware MCU patch
    blobs. Settles are bounded dummy register reads (the component holds no time
    capability). Core-crate tests grew to 13 (MAC OCP words, XID decode, ownership bits).

    *Liveness backstop, first real hit (kernel-lane reproduction context).* During the
    failed round-1 TX wait the kernel's stranded-runnable detector fired once
    (`liveness: stranded runnable: a child or service was runnable across an entire idle
    backstop (n=1)`). The driver-side wait pattern that produced it: `send-frame`'s
    completion poll is a loop of **pure synchronous host calls** — `dma-read` of the
    descriptor's opts1 dword, 50M iterations at the time — with no await that ever suspends
    (bar/dma calls resolve inline in the kernel root, and first-poll-inline retires them on
    first poll), so the executor had no interleave point for the entire bound while a
    sibling stayed runnable. The driver now bounds TX polls at 2M calls (seconds, not a
    minute; still ≫ the µs-scale consume time) — but the structural fact stands for the
    kernel lane: a guest hot-spinning on synchronous host calls is invisible to cooperative
    scheduling until its activation returns, and only preemption (plan/05) or a host-call
    budget covers it. recv's empty poll has the same shape at 2k calls (~ms), below the
    backstop's window.

    *Board round 2 (2026-06-08) → the DMA-coherence fix (kernel-side).* The OOB fix held
    (the new diagnostic printed `xid 0x641 8125B+ link up 2500 Mb/s`) but the typed TX
    failure repeated. The planner's triage was right that the failure is INBOUND (outbound
    config/MMIO provably worked); the root cause sits one layer below the bridge hypothesis:
    **mainline `rk3588-base.dtsi` carries no `dma-coherent` on any pcie node — RK3588 PCIe
    masters are not cache-coherent** — while the kernel's `alloc-dma` buffers are ordinary
    cacheable heap with no maintenance. The driver's descriptor writes (OWN bits included)
    sat in dirty D-cache lines; the NIC's ring fetches read stale DRAM zeros and never saw
    a descriptor to consume — and RX would have failed the same way, silently. This was the
    board's FIRST inbound DMA ever (lspci is config-only), i.e. the bringup-playbook §3
    rule meeting a brand-new handoff: a bus-mastering device is an agent at the PoC.

    The fix is in the shared pci provider, so every driver gets it: `arch::dma_coherence::
    sync` (board: the existing `clean_invalidate_to_poc` civac sweep + `dsb sy`; QEMU/other
    arches: no-op — emulated DMA is coherent) is applied (a) once at `alloc-dma` (evicting
    the allocation memset's dirty lines, which could otherwise write back OVER
    device-written bytes later), (b) after every `dma-write` (the device's next fetch reads
    DRAM; the sweep's `dsb sy` doubles as the reference drivers' `dma_wmb()` — a doorbell
    `bar-write` can no longer overtake the descriptor, closing the round-2 hypothesis-2
    ordering question by construction), (c) before every `dma-read` (the invalidate drops
    stale lines so completions and received frames are observed). Driver code unchanged —
    the discipline that every CPU access goes through the dma accessors is what makes the
    provider the single right place.

    Also landed for the bench (round-2 hypothesis 1, instrumented + defended): a
    board-only claim diagnostic — on `open` and on `set-bus-master(true)` the kernel
    prints the endpoint's command register AND its segment's root-port command, bus
    numbers, and type-1 memory window (`pci[claim] …` / `pci[busmaster] …` lines), so the
    next transcript proves bridge forwarding on the wire; and `set_bus_master(enable)` on
    the board now re-asserts MSE+BME on the root port if anything cleared them since
    bring-up (rk3588_pcie sets 0x107 at init; nothing is known to clear it — the re-assert
    prints loudly if it ever fires). Hypothesis 3 closed by reference: mainline
    `pcie-dw-rockchip.c` programs no inbound ATU in RC mode — inbound requests pass
    through untranslated, matching our setup.

    Verified: full ci green; check-telnet green (net.virtio DMA through the modified
    accessors); one QEMU session running both `net.rtl8125 $ l2check` (typed refusal) and
    `net.virtio $ l2check` (ARP resolved); both board images rebuilt.

    *Board round 4 (2026-06-08, wire truth) → the full rge(4) bring-up tables.* Host-side
    tcpdump on the same VLAN proved BOTH directions dead between the MAC engine and the
    wire — the ARP request never appeared on a busy VLAN (35k pkts/min) and the receiver
    caught none of it — while link (2500 Mb/s), descriptors (consumed), and the bridge
    diagnostics were all healthy. The link-up-MAC-speed-config hypothesis is REFUTED by
    reference: rge(4)'s `rge_link_state` does NOTHING at link change (no MAC writes), so a
    working 8125B driver carries no post-autoneg MAC speed programming. What rge carries
    that this driver did not is the bring-up table set it loads unconditionally before any
    traffic — and OpenBSD ships these IN-SOURCE (ISC-licensed, no firmware files), which
    both proves external firmware is unnecessary AND makes them transcribable into this
    MIT tree with attribution. Transcribed into `crates/eo9-rtl8125/src/r25b_tables.rs`
    (provenance header; sizes + spot values pinned by host tests):

    * `MAC_MCU` (138 pairs, rge `rtl8125b_mac_bps`): replayed after halting the MAC MCU
      (break vector 0xfc48=0, break registers 0xfc28..0xfc46=0, settle, 0xfc26=0; ram
      page 0 via 0xe446) — `rge_hw_init`'s MAC_R25B arm.
    * `EPHY` (46 pairs, rge `mac_r25b_ephy`): PCIe SerDes tuning through EPHYAR 0x80,
      rge's exact 7-bit address masking replayed.
    * `PHY_MCU` (1447 pairs, rge `MAC_R25B_MCU`): the GPHY MCU patch, applied inside the
      0xb820/0xb800 patch-mode bracket ONLY when the PHY's ram code version (OCP
      0xa436=0x801e → 0xa438) is not 0x0b99, then stamped — `rge_phy_config_mcu`.

    Bring-up now mirrors rge's `rge_chipinit` → `rge_phy_config` order: exit-OOB → MAC
    MCU halt+patch → PHY power (PMCH 0x6f |= 0xc0, BMCR=AUTOEN, GPHY state 0xa420==3) →
    second reset + IMR/ISR → EPHY table → advertise-nothing + PHY reset → ram-code check
    + PHY MCU patch → the ~30 `rge_phy_config_mac_r25b` writes → 0xa5b4 fix → **EEE
    disabled MAC- and PHY-side** (0xe040[1:0], 0xe052[0], the a432/a5d0/a6d4/a6d8/a428/
    a4a2/a442/a430 bits — un-configured EEE is itself a known frame-blackhole shape) →
    hw_start_8125 → advertise + autoneg → rings/enable. Bring-up diagnostic now also
    prints the GPHY state machine, the MAC EEE bits, and the ram code version, so the
    next transcript shows the datapath state either way. Omitted from rge, recorded:
    ASPM/clkreq disable + the CSI 0x108 write (PCIe power management, not frame path);
    8125A tables (the board is 8125B, xid 0x641 — an A part would skip the tables and
    likely still need its own set). Core crate: 16 host tests. Licensing note: the tables
    are transcribed from OpenBSD rge(4) (Copyright (c) 2019-2024 Kevin Lo, ISC license) —
    register/value facts plus replay order, with the provenance header in the module.

    *Board round 5 (2026-06-08) readings → round 6 instrumentation.* The one-run
    diagnostics worked: bring-up showed `xid 0x641, link up 2500, phy-state 3, eee-mac 0x0,
    ram-code 0x0000`; the rx diagnostic showed `tok+` with tally `tx 1` (TX provably leaves
    the MAC) and `rok- rdu- rx 0 missed 0` with the RXDV gate open (RX stone dead at the
    PHY→MAC boundary — the PHY delivered zero symbols on a VLAN carrying broadcast ARP).
    IMPORTANT correction recorded: round 5's `ram-code` print was the PRE-patch read — on a
    cold PHY 0x0000 proves the patch branch RAN, not that the load failed; whether it took
    was unobservable. Round 6 closes that hole and adds discrimination:

    * The GPHY ram code is RE-READ after the load and reported `before->after` in the
      bring-up line; `after != 0x0b99` on an 8125B prints a hard WARNING line, and both
      patch-mode handshakes (0xb820 bit 4 / 0xb800 bit 6) warn loudly on
      non-acknowledgement instead of the reference's silent continue.
    * Loopback self-tests at every bring-up, one line each: MAC loopback (TxConfig bit 17,
      vendor r8125.h `TxMACLoopBack`) proves descriptors/DMA/receive engine inside the
      chip; PHY PCS loopback (IEEE BMCR bit 14, forced 1000FD = 0x4140) proves the
      MAC<->PHY datapath to the MDI. PASS on both + dead wire RX discriminates
      cable/switch (e.g. 802.1X-style port policy) from silicon — the competing theory the
      board side could not previously kill. The tests share the raw transmit path (no
      link check) and park the one-shot rx diagnostic while running.
    * Compose-time speed cap: `rtl8125-config.configure(advertise-max)` (2500 default |
      1000 | 100; option-C unconfigured default, never traps). Bench triage:
      `net.rtl8125 --advertise-max 1000 $ l2check --gateway …` — frames at 1000 but not
      2500 pin the 2.5G datapath. Validated end-to-end under QEMU through eosh's
      compose/configure machinery (the configured composition compiles, instantiates,
      validates, probes, refuses typed).

    Core crate: 17 host tests (+ the loopback words). Full ci green; QEMU bare and
    configured refusal probes green; both board images rebuilt.
