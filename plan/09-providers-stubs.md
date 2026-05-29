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
