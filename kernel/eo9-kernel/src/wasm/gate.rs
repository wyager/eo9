//! The kernel call gate (shared-resources design, docs/design/shared-resources.md).
//!
//! Gate = route a child's import to a resource the spawner provided. v1 implements the
//! settled M1 slice, typed for `eo9:net/l4` only:
//!
//! * **Shares** — a boot-config `share` clause names a service whose composition exports
//!   the blessed `eo9:net/l4-factory` (validated at detach); the service becomes an
//!   *owner*: its drive future is widened from "one call to `main`" to
//!   `run_concurrent` over a kernel-side **intake** that serves gate calls inside the
//!   owner's store (§3.3 of the design — the only legal entry into a live store is from
//!   inside its own drive future). Factory-only providers have no `main` at all; their
//!   drive is the intake alone.
//! * **Grants** — a console `use l4=<share>` clause routes every console-descendant
//!   spawn whose component still imports `eo9:net/l4` (unfused) through the gate: the
//!   spawn mints a grant whose handler is lazily produced by the share's factory
//!   (`get`, called as the first gated operation inside the owner), and the child's
//!   ordinary l4 import is implemented by the typed gate shims in the spawn linker.
//! * **Gate calls** — queue-only in v1 (the inline first-poll fast path of §3.3 is the
//!   recorded follow-up; correctness never depended on it): the child-side shim
//!   enqueues a typed [`GateOp`], parks on a [`GateCallFuture`], and the owner's intake
//!   drains the queue on its next poll, starting each entry as a concurrent guest call
//!   on the owner's exported l4 surface. Completion rings the child's waker — events,
//!   never timers (the events-or-wfi doctrine; a gate-induced backstop wake is a bug).
//! * **Translation tables** — per-grant handle namespaces (§3.2): a child rep is an
//!   index into its own grant's table of owner-side [`ResourceAny`] entries; reps
//!   outside the table are typed errors, never owner-store access. Buffers cross the
//!   gate as host-table entry moves ([`BufferTable::take`]/`insert`) — no byte copy.
//! * **Death** (§6) — the owner's run ending severs every grant atomically before
//!   anything else: parked and in-flight calls answer the interface's own
//!   `io("provider task ended")`, and the gated children are killed (the task-tree
//!   rule, extended over the registry `use` edge). A child dying mid-call leaves the
//!   call to complete into the void inside the owner (SPEC "Kill and linearity").
//! * **Fuel** — owner-pays (§3.7): gate calls execute inside the owner's store on the
//!   owner's quantum-sliced pool; the child's pool is never touched while it parks.
//! * **Lock** — v1 ships the `instance` domain only, and on the single boot core it is
//!   structurally provided: wasmtime interleaves concurrent calls inside one store only
//!   at await points (intra-poll atomicity), the checkout discipline guarantees one
//!   embedder entry at a time, and the intake starts queued calls strictly FIFO. The
//!   `token` domain (per-grant interleaving) is config-accepted but refused at parse.
//!
//! v1 deferrals, recorded honestly (each is in the design's post-M1 ledger or the
//! workaround report): owner-side handle drops (`ResourceAny::resource_drop` needs a
//! sync call context and `resource_drop_async` needs the bare store — neither is
//! callable from inside `run_concurrent`, so dropped child handles release their
//! kernel-side table entry immediately but the owner-side wrapper resource lives until
//! the owner store drops); the generic dynamic gate shims; the guest-visible
//! `spawn(..., grants)` surface (parent→child sharing — M2, telnetd); `serves:` in
//! `describe`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use wasmtime::component::{
    Accessor, ComponentType, Func, Lift, Linker, Lower, Resource, ResourceAny, ResourceType, Val,
};
use wasmtime::{Result, StoreContextMut};

use super::providers::KernelState;
use super::shellexec::KLock;
use super::shellfs::BufferRes;

/// Boxed future shape for `func_wrap_concurrent` closures (the providers.rs alias).
type ConcurrentFuture<'a, R> = Pin<Box<dyn Future<Output = Result<R>> + Send + 'a>>;

/// The interface the typed v1 gate serves. The config grammar names it explicitly so
/// the day a second shareable API exists is a grammar no-op.
pub const L4_INTERFACE: &str = "eo9:net/l4@0.1.0";
/// The blessed factory sibling (validated at detach — design §5.2/R7).
pub const L4_FACTORY_INTERFACE: &str = "eo9:net/l4-factory@0.1.0";

// -----------------------------------------------------------------------------------------
// WIT-shaped host types (eo9:net/l4), for the child-side typed shims
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(variant)]
pub(super) enum WitIpAddress {
    #[component(name = "v4")]
    V4((u8, u8, u8, u8)),
    #[component(name = "v6")]
    V6((u16, u16, u16, u16, u16, u16, u16, u16)),
}

#[derive(Clone, Copy, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(record)]
pub(super) struct WitSocketAddress {
    pub(super) address: WitIpAddress,
    pub(super) port: u16,
}

#[derive(Clone, Debug, ComponentType, Lift, Lower)]
#[component(variant)]
pub(super) enum WitL4Error {
    #[component(name = "denied")]
    Denied,
    #[component(name = "unreachable")]
    Unreachable,
    #[component(name = "connection-refused")]
    ConnectionRefused,
    #[component(name = "connection-reset")]
    ConnectionReset,
    #[component(name = "timed-out")]
    TimedOut,
    #[component(name = "address-in-use")]
    AddressInUse,
    #[component(name = "address-unavailable")]
    AddressUnavailable,
    #[component(name = "not-connected")]
    NotConnected,
    #[component(name = "message-too-large")]
    MessageTooLarge,
    #[component(name = "io")]
    Io(String),
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
pub(super) struct WitSendResult {
    #[component(name = "bytes-sent")]
    pub(super) bytes_sent: u64,
}

#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(record)]
pub(super) struct WitRecvResult {
    #[component(name = "bytes-received")]
    pub(super) bytes_received: u64,
}

/// The owner-task-ended severance error, exactly the vocabulary §6 prescribes.
fn severed_error(reason: &str) -> WitL4Error {
    WitL4Error::Io(format!("provider task ended: {reason}"))
}

// -----------------------------------------------------------------------------------------
// Val encoding/decoding (the owner side speaks dynamic `Val`s)
// -----------------------------------------------------------------------------------------

fn addr_to_val(address: &WitSocketAddress) -> Val {
    let ip = match address.address {
        WitIpAddress::V4((a, b, c, d)) => Val::Variant(
            "v4".to_string(),
            Some(Box::new(Val::Tuple(vec![
                Val::U8(a),
                Val::U8(b),
                Val::U8(c),
                Val::U8(d),
            ]))),
        ),
        WitIpAddress::V6((a, b, c, d, e, f, g, h)) => Val::Variant(
            "v6".to_string(),
            Some(Box::new(Val::Tuple(vec![
                Val::U16(a),
                Val::U16(b),
                Val::U16(c),
                Val::U16(d),
                Val::U16(e),
                Val::U16(f),
                Val::U16(g),
                Val::U16(h),
            ]))),
        ),
    };
    Val::Record(vec![
        ("address".to_string(), ip),
        ("port".to_string(), Val::U16(address.port)),
    ])
}

fn val_to_addr(value: &Val) -> core::result::Result<WitSocketAddress, String> {
    let Val::Record(fields) = value else {
        return Err(format!("expected a socket-address record, got {value:?}"));
    };
    let mut address = None;
    let mut port = None;
    for (name, field) in fields {
        match name.as_str() {
            "address" => {
                let Val::Variant(case, payload) = field else {
                    return Err(format!("malformed ip-address: {field:?}"));
                };
                address = Some(match (case.as_str(), payload.as_deref()) {
                    ("v4", Some(Val::Tuple(t))) if t.len() == 4 => {
                        let octet = |v: &Val| match v {
                            Val::U8(b) => Ok(*b),
                            other => Err(format!("malformed v4 octet: {other:?}")),
                        };
                        WitIpAddress::V4((
                            octet(&t[0])?,
                            octet(&t[1])?,
                            octet(&t[2])?,
                            octet(&t[3])?,
                        ))
                    }
                    ("v6", Some(Val::Tuple(t))) if t.len() == 8 => {
                        let group = |v: &Val| match v {
                            Val::U16(g) => Ok(*g),
                            other => Err(format!("malformed v6 group: {other:?}")),
                        };
                        WitIpAddress::V6((
                            group(&t[0])?,
                            group(&t[1])?,
                            group(&t[2])?,
                            group(&t[3])?,
                            group(&t[4])?,
                            group(&t[5])?,
                            group(&t[6])?,
                            group(&t[7])?,
                        ))
                    }
                    _ => return Err(format!("malformed ip-address case `{case}`")),
                });
            }
            "port" => match field {
                Val::U16(p) => port = Some(*p),
                other => return Err(format!("malformed port: {other:?}")),
            },
            _ => {}
        }
    }
    match (address, port) {
        (Some(address), Some(port)) => Ok(WitSocketAddress { address, port }),
        _ => Err("socket-address record is missing fields".to_string()),
    }
}

fn val_to_l4_error(value: &Val) -> WitL4Error {
    let Val::Variant(case, payload) = value else {
        return WitL4Error::Io(format!("malformed l4-error: {value:?}"));
    };
    match case.as_str() {
        "denied" => WitL4Error::Denied,
        "unreachable" => WitL4Error::Unreachable,
        "connection-refused" => WitL4Error::ConnectionRefused,
        "connection-reset" => WitL4Error::ConnectionReset,
        "timed-out" => WitL4Error::TimedOut,
        "address-in-use" => WitL4Error::AddressInUse,
        "address-unavailable" => WitL4Error::AddressUnavailable,
        "not-connected" => WitL4Error::NotConnected,
        "message-too-large" => WitL4Error::MessageTooLarge,
        "io" => WitL4Error::Io(match payload.as_deref() {
            Some(Val::String(message)) => message.clone(),
            other => format!("{other:?}"),
        }),
        other => WitL4Error::Io(format!("unknown l4-error case `{other}`")),
    }
}

// -----------------------------------------------------------------------------------------
// Gate calls
// -----------------------------------------------------------------------------------------

/// One typed gated operation. Handle fields are translation-table indices of the call's
/// own grant — never owner reps, never another grant's indices (§3.2 anti-forgery).
/// Buffer payloads ride as moved bytes ([`BufferTable::take`] on the child side,
/// `insert` on the owner side — an entry move, not a copy).
pub(super) enum GateOp {
    Connect { remote: WitSocketAddress },
    Listen { local: WitSocketAddress },
    Accept { listener: u32 },
    ListenerAddress { listener: u32 },
    PeerAddress { conn: u32 },
    Send { conn: u32, bytes: Vec<u8> },
    Recv { conn: u32, bytes: Vec<u8> },
    BindUdp { local: WitSocketAddress },
    UdpAddress { socket: u32 },
    SendTo { socket: u32, remote: WitSocketAddress, bytes: Vec<u8> },
    RecvFrom { socket: u32, bytes: Vec<u8> },
}

/// A completed gate call's typed payload, kernel-plain (no store pointers — §3.2:
/// "the gate holds no pointer into either store").
pub(super) enum GateDone {
    /// connect / listen / bind-udp: a fresh handle (a table index) or the typed error.
    Handle(core::result::Result<u32, WitL4Error>),
    /// accept: (connection handle, peer address) or the typed error.
    Accepted(core::result::Result<(u32, WitSocketAddress), WitL4Error>),
    /// listener-address / peer-address / udp-address.
    Addr(WitSocketAddress),
    /// send / recv / send-to: the round-tripped bytes plus the count or typed error.
    Io(Vec<u8>, core::result::Result<u64, WitL4Error>),
    /// recv-from: bytes plus (count, sender) or the typed error.
    IoFrom(
        Vec<u8>,
        core::result::Result<(u64, WitSocketAddress), WitL4Error>,
    ),
}

enum CallState {
    /// Waiting in the share's queue; never entered the owner.
    Enqueued,
    /// The intake started it as a concurrent call inside the owner.
    InFlight,
    /// Finished; the payload is taken exactly once by the caller's future.
    Done(Option<GateDone>),
    /// The gate was severed (owner died) or the call failed gate-side.
    Severed(String),
}

struct CallInner {
    op: Option<GateOp>,
    state: CallState,
    waker: Option<Waker>,
}

/// One gate call, `Arc`-shared between the child's parked future and the gate registry
/// (§3.5) so either side can disappear first.
pub(super) struct GateCall {
    share: usize,
    grant: u32,
    inner: KLock<CallInner>,
}

impl GateCall {
    fn new(share: usize, grant: u32, op: GateOp) -> Arc<Self> {
        Arc::new(GateCall {
            share,
            grant,
            inner: KLock::new(CallInner {
                op: Some(op),
                state: CallState::Enqueued,
                waker: None,
            }),
        })
    }

    fn take_op(&self) -> Option<GateOp> {
        self.inner.with(|inner| {
            inner.state = CallState::InFlight;
            inner.op.take()
        })
    }

    fn finish(&self, done: core::result::Result<GateDone, String>) {
        let waker = self.inner.with(|inner| {
            inner.state = match done {
                Ok(done) => CallState::Done(Some(done)),
                Err(reason) => CallState::Severed(reason),
            };
            inner.waker.take()
        });
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn sever(&self, reason: &str) {
        let waker = self.inner.with(|inner| {
            if matches!(inner.state, CallState::Done(_)) {
                return None;
            }
            inner.state = CallState::Severed(reason.to_string());
            inner.waker.take()
        });
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The caller's suspended future (§3.5): parked until the intake completes or severs the
/// call. Wake edges are events only — intake completion and severance both ring the
/// stored waker; there is no timer anywhere in the gate.
pub(super) struct GateCallFuture {
    call: Arc<GateCall>,
}

impl Future for GateCallFuture {
    type Output = core::result::Result<GateDone, WitL4Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.call.inner.with(|inner| match &mut inner.state {
            CallState::Done(done) => match done.take() {
                Some(done) => Poll::Ready(Ok(done)),
                None => Poll::Ready(Err(severed_error("result already consumed"))),
            },
            CallState::Severed(reason) => Poll::Ready(Err(severed_error(reason))),
            CallState::Enqueued | CallState::InFlight => {
                inner.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}

impl Drop for GateCallFuture {
    fn drop(&mut self) {
        // §3.6 — caller killed mid-gate-call. Enqueued: remove from the queue; nothing
        // happened, nothing to clean. InFlight: the call runs to completion inside the
        // owner and the result is discarded (the cell's other end is this future).
        let enqueued = self
            .call
            .inner
            .with(|inner| matches!(inner.state, CallState::Enqueued));
        if enqueued {
            SHARES.with(|shares| {
                if let Some(share) = shares.get_mut(self.call.share) {
                    share
                        .queue
                        .retain(|queued| !Arc::ptr_eq(queued, &self.call));
                    share.live.retain(|live| !Arc::ptr_eq(live, &self.call));
                }
            });
        }
    }
}

// -----------------------------------------------------------------------------------------
// The share registry
// -----------------------------------------------------------------------------------------

/// One grant: a child task's routed l4 import. The translation table is the per-grant
/// handle namespace (§3.2): index → owner-side handle; index 0 is the granted handler
/// (minted lazily by the factory on the grant's first operation).
struct Grant {
    table: Vec<Option<ResourceAny>>,
    /// The grantee's task rep, for the owner-death kill cascade (§6 step 2).
    task: Option<u32>,
}

/// One declared share (a config `share` clause), wired to its current owner run.
struct Share {
    name: String,
    /// Kept for the day a second shareable API exists (the grammar already names it).
    #[allow(dead_code)]
    interface: String,
    /// The owner's slot in the service registry, while a run is live.
    service: Option<usize>,
    severed: bool,
    /// Calls waiting for the intake's next poll (FIFO — the v1 `instance` domain's
    /// admission order).
    queue: Vec<Arc<GateCall>>,
    /// Every unfinished call (queued or in flight), for severance.
    live: Vec<Arc<GateCall>>,
    /// The intake's poll waker: enqueue rings it so the owner's next poll drains.
    intake_waker: Option<Waker>,
    grants: Vec<Option<Grant>>,
}

static SHARES: KLock<Vec<Share>> = KLock::new(Vec::new());

/// The console `use` clause: (import slot, share name). v1: slot must be `l4`.
static CONSOLE_USE: KLock<Option<(String, String)>> = KLock::new(None);

/// Reset the gate registry (a fresh boot's config parse).
pub fn reset() {
    SHARES.with(|shares| shares.clear());
    CONSOLE_USE.with(|console_use| *console_use = None);
}

/// The share declared for a service name, if any (svc's detach-time hook).
pub(super) fn declared_share(name: &str) -> Option<usize> {
    SHARES.with(|shares| shares.iter().position(|share| share.name == name))
}

/// Wire (or re-wire, after a restart) a share to its owner's registry slot. Clears any
/// severed state: the new run starts with a fresh queue and no grants.
pub(super) fn wire_owner(share_id: usize, service: usize) {
    SHARES.with(|shares| {
        if let Some(share) = shares.get_mut(share_id) {
            share.service = Some(service);
            share.severed = false;
            share.queue.clear();
            share.live.clear();
            share.intake_waker = None;
            share.grants.clear();
        }
    });
}

/// §6 step 1+2 — the owner's run ended (completed, trapped, stopped, or the registry is
/// shutting down): sever first, then the kill cascade over the registry `use` edges.
/// Severed-before-anything is the only ordering the gate needs: the gate holds reps and
/// plain data, never store pointers, so no use-after-free is reachable once severance
/// precedes the store drop.
pub(super) fn on_owner_run_ended(service: usize, reason: &str) {
    let Some(share_id) =
        SHARES.with(|shares| shares.iter().position(|share| share.service == Some(service)))
    else {
        return;
    };
    let (calls, tasks) = SHARES.with(|shares| {
        let share = &mut shares[share_id];
        share.severed = true;
        share.service = None;
        share.intake_waker = None;
        let calls: Vec<Arc<GateCall>> = share.queue.drain(..).chain(share.live.drain(..)).collect();
        let tasks: Vec<u32> = share
            .grants
            .iter()
            .flatten()
            .filter_map(|grant| grant.task)
            .collect();
        // The grants' translation tables are kernel-side plain data; the owner-side
        // entries die with the owner's store (order-independent by severance-first).
        share.grants.clear();
        (calls, tasks)
    });
    for call in &calls {
        call.sever(reason);
    }
    if !tasks.is_empty() {
        crate::kprintln!(
            "gate: share severed ({reason}); killing {} gated task(s)",
            tasks.len()
        );
    }
    for task in tasks {
        super::shellexec::kill_task_tree(task as usize);
    }
}

/// The console-use share for a child that imports `eo9:net/l4`, if this boot wired one.
fn console_l4_share() -> Option<usize> {
    let name = CONSOLE_USE.with(|console_use| {
        console_use
            .as_ref()
            .filter(|(slot, _)| slot == "l4")
            .map(|(_, share)| share.clone())
    })?;
    declared_share(&name)
}

/// A handle on one grant, owned by the grantee's store ([`KernelState::gate`]); dropping
/// it (the child's store drops) releases the grant's kernel-side state.
pub struct GrantHandle {
    pub(super) share: usize,
    pub(super) grant: u32,
}

impl Drop for GrantHandle {
    fn drop(&mut self) {
        SHARES.with(|shares| {
            if let Some(share) = shares.get_mut(self.share)
                && let Some(slot) = share.grants.get_mut(self.grant as usize)
            {
                // v1: kernel-side release only — the owner-side wrapper resources live
                // until the owner store drops (see the module docs' deferral note).
                *slot = None;
            }
        });
    }
}

/// Mint a grant for a console-descendant spawn whose component imports `eo9:net/l4`.
/// Returns `Ok(None)` when this boot wired no console share (the shims then answer the
/// capability refusal); a wired-but-dead share refuses the spawn with the §6 story.
pub(super) fn mint_console_grant() -> core::result::Result<Option<GrantHandle>, String> {
    let Some(share_id) = console_l4_share() else {
        return Ok(None);
    };
    SHARES.with(|shares| {
        let share = &mut shares[share_id];
        if share.severed || share.service.is_none() {
            return Err(format!(
                "the shared network provider `{}` is not running (the gate is severed); \
                 the spawn is refused until its supervisor restarts it",
                share.name
            ));
        }
        let grant = Grant {
            // Entry 0 is the handler, minted lazily by the factory on first use.
            table: vec![None],
            task: None,
        };
        let index = share.grants.iter().position(Option::is_none);
        let grant_id = match index {
            Some(index) => {
                share.grants[index] = Some(grant);
                index
            }
            None => {
                share.grants.push(Some(grant));
                share.grants.len() - 1
            }
        };
        Ok(Some(GrantHandle {
            share: share_id,
            grant: grant_id as u32,
        }))
    })
}

/// Record the grantee's task rep once the spawn registered it (the kill-cascade edge).
pub(super) fn bind_grant_task(share_id: usize, grant_id: u32, task: u32) {
    SHARES.with(|shares| {
        if let Some(share) = shares.get_mut(share_id)
            && let Some(Some(grant)) = share.grants.get_mut(grant_id as usize)
        {
            grant.task = Some(task);
        }
    });
}

/// A child handle drop: release the kernel-side table entry (the owner-side wrapper
/// resource is the v1 deferral — see the module docs).
fn drop_child_handle(share: usize, grant: u32, rep: u32) {
    SHARES.with(|shares| {
        if let Some(s) = shares.get_mut(share)
            && let Some(Some(g)) = s.grants.get_mut(grant as usize)
            && rep != 0
            && let Some(slot) = g.table.get_mut(rep as usize)
        {
            *slot = None;
        }
    });
}

/// Enqueue one gate call on a share and ring the owner's intake (an event, never a
/// timer). Refuses with the severance story when the owner is gone.
fn submit(share_id: usize, grant: u32, op: GateOp) -> core::result::Result<Arc<GateCall>, String> {
    let call = GateCall::new(share_id, grant, op);
    let waker = SHARES.with(|shares| {
        let Some(share) = shares.get_mut(share_id) else {
            return Err("unknown share".to_string());
        };
        if share.severed || share.service.is_none() {
            return Err(format!("the shared provider `{}` ended", share.name));
        }
        if share
            .grants
            .get(grant as usize)
            .and_then(Option::as_ref)
            .is_none()
        {
            return Err("the grant was released".to_string());
        }
        share.queue.push(call.clone());
        share.live.push(call.clone());
        Ok(share.intake_waker.take())
    })?;
    if let Some(waker) = waker {
        waker.wake();
    }
    Ok(call)
}

// -----------------------------------------------------------------------------------------
// Boot-config clauses: `share <interface> [lock=instance]` and `use l4=<share>`
// -----------------------------------------------------------------------------------------

/// Parse and strip the gate clauses from a boot services config. The kernel owns the
/// share/use wiring in v1 (the design's §5.3 grammar is spelling-illustrative; init's
/// own grammar is untouched — it receives the stripped text and detaches the service
/// lines exactly as before, and the registry recognizes share-declared names at
/// detach). Returns the stripped config; malformed clauses are reported and dropped
/// (the service line itself survives, unshared — a config error never bricks the boot).
pub fn parse_boot_config(config: &str) -> String {
    reset();
    let mut stripped = String::with_capacity(config.len());
    for raw_line in config.lines() {
        let (code, comment) = match raw_line.find('#') {
            Some(at) => raw_line.split_at(at),
            None => (raw_line, ""),
        };
        let mut tokens: Vec<&str> = code.split_whitespace().collect();
        let name = tokens
            .first()
            .map(|first| first.trim_end_matches('='))
            .unwrap_or("")
            .to_string();

        // `share <interface> [lock=instance|token]` on a service line.
        if let Some(at) = tokens.iter().position(|token| *token == "share") {
            let mut end = at + 1;
            let interface = tokens.get(at + 1).map(|s| s.to_string());
            if interface.is_some() {
                end += 1;
            }
            let mut lock = "instance";
            if let Some(token) = tokens.get(end)
                && let Some(value) = token.strip_prefix("lock=")
            {
                lock = match value {
                    "instance" => "instance",
                    "token" => {
                        crate::kprintln!(
                            "gate: lock=token is not implemented in v1; `{name}` uses \
                             lock=instance (the safe default)"
                        );
                        "instance"
                    }
                    other => {
                        crate::kprintln!(
                            "gate: unknown lock domain `{other}` on `{name}`; using instance"
                        );
                        "instance"
                    }
                };
                end += 1;
            }
            let _ = lock;
            match interface {
                Some(interface) if interface == L4_INTERFACE => {
                    SHARES.with(|shares| {
                        shares.push(Share {
                            name: name.clone(),
                            interface,
                            service: None,
                            severed: true, // until the owner is wired
                            queue: Vec::new(),
                            live: Vec::new(),
                            intake_waker: None,
                            grants: Vec::new(),
                        });
                    });
                    crate::kprintln!("gate: `{name}` declared as the {L4_INTERFACE} share");
                }
                Some(other) => crate::kprintln!(
                    "gate: `share {other}` on `{name}` is not supported (v1 serves \
                     {L4_INTERFACE} only); the clause is ignored"
                ),
                None => crate::kprintln!(
                    "gate: `share` on `{name}` names no interface; the clause is ignored"
                ),
            }
            tokens.drain(at..end);
        }

        // `use <slot>=<share>` on the console line.
        if let Some(at) = tokens.iter().position(|token| *token == "use") {
            let mut end = at + 1;
            if let Some(edge) = tokens.get(at + 1) {
                end += 1;
                match edge.split_once('=') {
                    Some(("l4", share_name)) => {
                        CONSOLE_USE.with(|console_use| {
                            *console_use =
                                Some(("l4".to_string(), share_name.to_string()));
                        });
                        crate::kprintln!(
                            "gate: console children's eo9:net/l4 imports route to `{share_name}`"
                        );
                    }
                    _ => crate::kprintln!(
                        "gate: `use {edge}` is not supported (v1 wires `use l4=<share>` \
                         only); the clause is ignored"
                    ),
                }
            } else {
                crate::kprintln!("gate: `use` names no edge; the clause is ignored");
            }
            tokens.drain(at..end);
        }

        stripped.push_str(&tokens.join(" "));
        if !comment.is_empty() {
            if !tokens.is_empty() {
                stripped.push(' ');
            }
            stripped.push_str(comment);
        }
        stripped.push('\n');
    }
    stripped
}

// -----------------------------------------------------------------------------------------
// The owner side: exported-surface lookup and the intake
// -----------------------------------------------------------------------------------------

/// The owner instance's exported l4 + factory functions, looked up once at owner spawn.
/// `Func` is a plain copyable index — no store pointer (§3.2's "no pointers" rule holds:
/// these are only ever used from inside the owner's own drive future).
#[derive(Clone, Copy)]
pub(super) struct OwnerFuncs {
    get: Func,
    connect: Func,
    listen: Func,
    accept: Func,
    listener_address: Func,
    peer_address: Func,
    send: Func,
    recv: Func,
    bind_udp: Func,
    udp_address: Func,
    send_to: Func,
    recv_from: Func,
}

/// Look up `func` inside the exported instance `interface` (the async_demo pattern).
fn exported_func(
    instance: &wasmtime::component::Instance,
    store: &mut wasmtime::Store<KernelState>,
    interface: &str,
    func: &str,
) -> core::result::Result<Func, String> {
    let interface_index = instance
        .get_export_index(&mut *store, None, interface)
        .ok_or_else(|| format!("the serving composition does not export `{interface}`"))?;
    let func_index = instance
        .get_export_index(&mut *store, Some(&interface_index), func)
        .ok_or_else(|| format!("`{interface}` does not export `{func}`"))?;
    instance
        .get_func(&mut *store, func_index)
        .ok_or_else(|| format!("`{interface}.{func}` is not a function"))
}

/// Look up the whole serving surface on a freshly instantiated owner. This is the
/// blessing-with-validation moment's structural half (R7): a share-declared service
/// must export the blessed factory AND the full l4 contract (the gate can only call
/// exported functions — non-exported fused internals are unreachable from the
/// embedder), or the detach is refused before anything runs.
pub(super) fn lookup_owner_funcs(
    instance: &wasmtime::component::Instance,
    store: &mut wasmtime::Store<KernelState>,
) -> core::result::Result<OwnerFuncs, String> {
    Ok(OwnerFuncs {
        get: exported_func(instance, store, L4_FACTORY_INTERFACE, "get")?,
        connect: exported_func(instance, store, L4_INTERFACE, "connect")?,
        listen: exported_func(instance, store, L4_INTERFACE, "listen")?,
        accept: exported_func(instance, store, L4_INTERFACE, "accept")?,
        listener_address: exported_func(instance, store, L4_INTERFACE, "listener-address")?,
        peer_address: exported_func(instance, store, L4_INTERFACE, "peer-address")?,
        send: exported_func(instance, store, L4_INTERFACE, "send")?,
        recv: exported_func(instance, store, L4_INTERFACE, "recv")?,
        bind_udp: exported_func(instance, store, L4_INTERFACE, "bind-udp")?,
        udp_address: exported_func(instance, store, L4_INTERFACE, "udp-address")?,
        send_to: exported_func(instance, store, L4_INTERFACE, "send-to")?,
        recv_from: exported_func(instance, store, L4_INTERFACE, "recv-from")?,
    })
}

/// What one completed owner-side call should be parsed as.
enum ResultKind {
    Handle,
    Accepted,
    Addr,
    Io,
    IoFrom,
}

/// One in-flight owner-side call future.
type CallFut<'a> =
    Pin<Box<dyn Future<Output = core::result::Result<GateDone, String>> + Send + 'a>>;

/// Read a grant's table entry (an owner-side handle) for a child rep.
fn table_entry(share_id: usize, grant: u32, rep: u32) -> core::result::Result<ResourceAny, String> {
    SHARES.with(|shares| {
        shares
            .get(share_id)
            .and_then(|share| share.grants.get(grant as usize))
            .and_then(|slot| slot.as_ref())
            .and_then(|g| g.table.get(rep as usize))
            .and_then(|entry| *entry)
            .ok_or_else(|| format!("unknown handle {rep} (not in this grant's table)"))
    })
}

/// Enter an owner-side handle into a grant's table, returning the child rep.
fn table_enter(
    share_id: usize,
    grant: u32,
    handle: ResourceAny,
) -> core::result::Result<u32, String> {
    SHARES.with(|shares| {
        let g = shares
            .get_mut(share_id)
            .and_then(|share| share.grants.get_mut(grant as usize))
            .and_then(|slot| slot.as_mut())
            .ok_or_else(|| "the grant was released".to_string())?;
        let index = g.table.iter().position(Option::is_none);
        Ok(match index {
            Some(index) => {
                g.table[index] = Some(handle);
                index as u32
            }
            None => {
                g.table.push(Some(handle));
                (g.table.len() - 1) as u32
            }
        })
    })
}

/// The handler entry (table index 0) for a grant, if already minted.
fn handler_entry(share_id: usize, grant: u32) -> Option<ResourceAny> {
    table_entry(share_id, grant, 0).ok()
}

fn set_handler_entry(share_id: usize, grant: u32, handle: ResourceAny) {
    SHARES.with(|shares| {
        if let Some(share) = shares.get_mut(share_id)
            && let Some(Some(g)) = share.grants.get_mut(grant as usize)
            && let Some(slot) = g.table.get_mut(0)
        {
            *slot = Some(handle);
        }
    });
}

/// Mint an owner-side buffer from moved child bytes; returns the own handle as a `Val`.
fn mint_owner_buffer(
    accessor: &Accessor<KernelState>,
    bytes: Vec<u8>,
) -> core::result::Result<Val, String> {
    accessor.with(|mut access| {
        let rep = access
            .get()
            .shell_buffers()
            .map_err(|err| format!("{err:?}"))?
            .insert(bytes);
        let any = Resource::<BufferRes>::new_own(rep)
            .try_into_resource_any(&mut access)
            .map_err(|err| format!("buffer mint failed: {err:?}"))?;
        Ok(Val::Resource(any))
    })
}

/// Take an owner-side buffer's bytes back out (the return leg of the entry move).
fn take_owner_buffer(
    accessor: &Accessor<KernelState>,
    value: &Val,
) -> core::result::Result<Vec<u8>, String> {
    let Val::Resource(any) = value else {
        return Err(format!("expected a buffer resource, got {value:?}"));
    };
    accessor.with(|mut access| {
        let resource = Resource::<BufferRes>::try_from_resource_any(*any, &mut access)
            .map_err(|err| format!("buffer return failed: {err:?}"))?;
        let bytes = access
            .get()
            .shell_buffers()
            .map_err(|err| format!("{err:?}"))?
            .take(resource.rep())
            .map_err(|err| format!("{err:?}"))?;
        Ok(bytes)
    })
}

/// Parse a `result<own-handle, l4-error>` into a table entry.
fn parse_handle_result(
    share_id: usize,
    grant: u32,
    value: &Val,
) -> core::result::Result<GateDone, String> {
    match value {
        Val::Result(Ok(Some(payload))) => match payload.as_ref() {
            Val::Resource(any) => {
                let rep = table_enter(share_id, grant, *any)?;
                Ok(GateDone::Handle(Ok(rep)))
            }
            other => Err(format!("expected a resource, got {other:?}")),
        },
        Val::Result(Err(payload)) => Ok(GateDone::Handle(Err(match payload.as_deref() {
            Some(error) => val_to_l4_error(error),
            None => WitL4Error::Io("malformed error payload".to_string()),
        }))),
        other => Err(format!("expected a result, got {other:?}")),
    }
}

/// Build the owner-side call future for one drained gate call. Runs synchronously in
/// the intake's poll (store access via the accessor); the returned future performs the
/// actual concurrent guest call(s) and parses the typed payload.
fn build_call_future<'a>(
    accessor: &'a Accessor<KernelState>,
    funcs: OwnerFuncs,
    call: &Arc<GateCall>,
) -> core::result::Result<CallFut<'a>, String> {
    let share_id = call.share;
    let grant = call.grant;
    let op = call.take_op().ok_or("the call has no operation")?;

    // Pre-translate handles and move buffers (synchronous, before the future starts).
    enum Prepared {
        Plain(Func, Vec<Val>, ResultKind),
        /// (func, params with a buffer hole at `at`, bytes, kind)
        WithBuffer(Func, Vec<Val>, usize, Vec<u8>, ResultKind),
        /// connect/listen/bind-udp: needs the handler as param 0.
        RootCall(Func, Vec<Val>, ResultKind),
    }
    let prepared = match op {
        GateOp::Connect { remote } => Prepared::RootCall(
            funcs.connect,
            vec![Val::Bool(false), addr_to_val(&remote)],
            ResultKind::Handle,
        ),
        GateOp::Listen { local } => Prepared::RootCall(
            funcs.listen,
            vec![Val::Bool(false), addr_to_val(&local)],
            ResultKind::Handle,
        ),
        GateOp::BindUdp { local } => Prepared::RootCall(
            funcs.bind_udp,
            vec![Val::Bool(false), addr_to_val(&local)],
            ResultKind::Handle,
        ),
        GateOp::Accept { listener } => Prepared::Plain(
            funcs.accept,
            vec![Val::Resource(table_entry(share_id, grant, listener)?)],
            ResultKind::Accepted,
        ),
        GateOp::ListenerAddress { listener } => Prepared::Plain(
            funcs.listener_address,
            vec![Val::Resource(table_entry(share_id, grant, listener)?)],
            ResultKind::Addr,
        ),
        GateOp::PeerAddress { conn } => Prepared::Plain(
            funcs.peer_address,
            vec![Val::Resource(table_entry(share_id, grant, conn)?)],
            ResultKind::Addr,
        ),
        GateOp::UdpAddress { socket } => Prepared::Plain(
            funcs.udp_address,
            vec![Val::Resource(table_entry(share_id, grant, socket)?)],
            ResultKind::Addr,
        ),
        GateOp::Send { conn, bytes } => Prepared::WithBuffer(
            funcs.send,
            vec![
                Val::Resource(table_entry(share_id, grant, conn)?),
                Val::Bool(false),
            ],
            1,
            bytes,
            ResultKind::Io,
        ),
        GateOp::Recv { conn, bytes } => Prepared::WithBuffer(
            funcs.recv,
            vec![
                Val::Resource(table_entry(share_id, grant, conn)?),
                Val::Bool(false),
            ],
            1,
            bytes,
            ResultKind::Io,
        ),
        GateOp::SendTo {
            socket,
            remote,
            bytes,
        } => Prepared::WithBuffer(
            funcs.send_to,
            vec![
                Val::Resource(table_entry(share_id, grant, socket)?),
                addr_to_val(&remote),
                Val::Bool(false),
            ],
            2,
            bytes,
            ResultKind::Io,
        ),
        GateOp::RecvFrom { socket, bytes } => Prepared::WithBuffer(
            funcs.recv_from,
            vec![
                Val::Resource(table_entry(share_id, grant, socket)?),
                Val::Bool(false),
            ],
            1,
            bytes,
            ResultKind::IoFrom,
        ),
    };

    Ok(Box::pin(async move {
        // Lazily mint the handler when a root operation needs it.
        let ensure_handler = async |accessor: &Accessor<KernelState>| {
            if let Some(handler) = handler_entry(share_id, grant) {
                return Ok(handler);
            }
            let mut results = vec![Val::Bool(false)];
            funcs
                .get
                .call_concurrent(accessor, &[], &mut results)
                .await
                .map_err(|err| format!("the factory's get failed: {err:?}"))?;
            match &results[0] {
                Val::Result(Ok(Some(payload))) => match payload.as_ref() {
                    Val::Resource(any) => {
                        set_handler_entry(share_id, grant, *any);
                        Ok(*any)
                    }
                    other => Err(format!("the factory returned a non-resource: {other:?}")),
                },
                Val::Result(Err(payload)) => Err(format!(
                    "the factory refused the wiring: {:?}",
                    payload.as_deref().map(val_to_l4_error)
                )),
                other => Err(format!("malformed factory result: {other:?}")),
            }
        };

        let (func, params, kind) = match prepared {
            Prepared::RootCall(func, mut params, kind) => {
                let handler = ensure_handler(accessor).await?;
                params[0] = Val::Resource(handler);
                (func, params, kind)
            }
            Prepared::Plain(func, params, kind) => (func, params, kind),
            Prepared::WithBuffer(func, mut params, at, bytes, kind) => {
                params[at] = mint_owner_buffer(accessor, bytes)?;
                (func, params, kind)
            }
        };

        let mut results = vec![Val::Bool(false)];
        func.call_concurrent(accessor, &params, &mut results)
            .await
            .map_err(|err| format!("the gated call failed inside the owner: {err:?}"))?;
        let value = &results[0];

        match kind {
            ResultKind::Handle => parse_handle_result(share_id, grant, value),
            ResultKind::Accepted => match value {
                Val::Result(Ok(Some(payload))) => match payload.as_ref() {
                    Val::Tuple(pair) if pair.len() == 2 => {
                        let Val::Resource(any) = &pair[0] else {
                            return Err(format!("accept returned a non-resource: {:?}", pair[0]));
                        };
                        let rep = table_enter(share_id, grant, *any)?;
                        Ok(GateDone::Accepted(Ok((rep, val_to_addr(&pair[1])?))))
                    }
                    other => Err(format!("malformed accept payload: {other:?}")),
                },
                Val::Result(Err(payload)) => {
                    Ok(GateDone::Accepted(Err(match payload.as_deref() {
                        Some(error) => val_to_l4_error(error),
                        None => WitL4Error::Io("malformed error payload".to_string()),
                    })))
                }
                other => Err(format!("expected a result, got {other:?}")),
            },
            ResultKind::Addr => Ok(GateDone::Addr(val_to_addr(value)?)),
            ResultKind::Io | ResultKind::IoFrom => match value {
                Val::Tuple(pair) if pair.len() == 2 => {
                    let bytes = take_owner_buffer(accessor, &pair[0])?;
                    match (&pair[1], &kind) {
                        (Val::Result(Ok(Some(payload))), ResultKind::Io) => {
                            let count = match payload.as_ref() {
                                Val::Record(fields) => fields
                                    .iter()
                                    .find_map(|(name, v)| match (name.as_str(), v) {
                                        ("bytes-sent" | "bytes-received", Val::U64(n)) => Some(*n),
                                        _ => None,
                                    })
                                    .ok_or_else(|| "malformed io result record".to_string())?,
                                other => return Err(format!("malformed io result: {other:?}")),
                            };
                            Ok(GateDone::Io(bytes, Ok(count)))
                        }
                        (Val::Result(Ok(Some(payload))), ResultKind::IoFrom) => {
                            match payload.as_ref() {
                                Val::Tuple(inner) if inner.len() == 2 => {
                                    let count = match &inner[0] {
                                        Val::Record(fields) => fields
                                            .iter()
                                            .find_map(|(name, v)| match (name.as_str(), v) {
                                                ("bytes-received", Val::U64(n)) => Some(*n),
                                                _ => None,
                                            })
                                            .ok_or_else(|| {
                                                "malformed recv-from record".to_string()
                                            })?,
                                        other => {
                                            return Err(format!(
                                                "malformed recv-from result: {other:?}"
                                            ));
                                        }
                                    };
                                    Ok(GateDone::IoFrom(
                                        bytes,
                                        Ok((count, val_to_addr(&inner[1])?)),
                                    ))
                                }
                                other => Err(format!("malformed recv-from payload: {other:?}")),
                            }
                        }
                        (Val::Result(Err(payload)), ResultKind::Io) => Ok(GateDone::Io(
                            bytes,
                            Err(match payload.as_deref() {
                                Some(error) => val_to_l4_error(error),
                                None => WitL4Error::Io("malformed error payload".to_string()),
                            }),
                        )),
                        (Val::Result(Err(payload)), ResultKind::IoFrom) => Ok(GateDone::IoFrom(
                            bytes,
                            Err(match payload.as_deref() {
                                Some(error) => val_to_l4_error(error),
                                None => WitL4Error::Io("malformed error payload".to_string()),
                            }),
                        )),
                        (other, _) => Err(format!("malformed io tuple: {other:?}")),
                    }
                }
                other => Err(format!("expected an (buffer, result) tuple, got {other:?}")),
            },
        }
    }))
}

/// The owner-side intake (§3.3): kernel code living inside the owner's `run_concurrent`
/// closure. On every poll of the owner's drive future it (a) drains the share's queue,
/// starting each entry as a concurrent guest call via the accessor — the only place
/// wasmtime permits starting calls — and (b) polls the in-flight calls alongside
/// whatever else the store is running. It never completes; the owner's run ends by
/// being dropped (kill/stop), and severance is the registry's job, not this future's.
pub(super) async fn intake(
    accessor: &Accessor<KernelState>,
    share_id: usize,
    funcs: OwnerFuncs,
) {
    let mut in_flight: Vec<(Arc<GateCall>, CallFut<'_>)> = Vec::new();
    core::future::poll_fn(move |cx| {
        // Drain the queue (FIFO — the v1 instance-domain admission order), registering
        // this poll's waker so a later enqueue rings the owner runnable.
        let drained: Vec<Arc<GateCall>> = SHARES.with(|shares| {
            let Some(share) = shares.get_mut(share_id) else {
                return Vec::new();
            };
            share.intake_waker = Some(cx.waker().clone());
            core::mem::take(&mut share.queue)
        });
        for call in drained {
            match build_call_future(accessor, funcs, &call) {
                Ok(future) => in_flight.push((call, future)),
                Err(reason) => {
                    SHARES.with(|shares| {
                        if let Some(share) = shares.get_mut(share_id) {
                            share.live.retain(|live| !Arc::ptr_eq(live, &call));
                        }
                    });
                    call.finish(Err(reason));
                }
            }
        }
        // Poll the in-flight calls; completion translates handles/buffers and rings the
        // child's waker.
        let mut index = 0;
        while index < in_flight.len() {
            match in_flight[index].1.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    let (call, _) = in_flight.swap_remove(index);
                    SHARES.with(|shares| {
                        if let Some(share) = shares.get_mut(share_id) {
                            share.live.retain(|live| !Arc::ptr_eq(live, &call));
                        }
                    });
                    call.finish(result);
                }
                Poll::Pending => index += 1,
            }
        }
        Poll::<()>::Pending
    })
    .await
}

// -----------------------------------------------------------------------------------------
// The child side: typed gate shims for eo9:net/l4
// -----------------------------------------------------------------------------------------

/// Host representation of a gated `l4-impl` (rep 0 = the grant's handler).
pub struct GateL4Res;
/// Host representation of a gated `tcp-connection` (rep = translation-table index).
pub struct GateConnRes;
/// Host representation of a gated `tcp-listener`.
pub struct GateListenerRes;
/// Host representation of a gated `udp-socket`.
pub struct GateUdpRes;

/// The no-grant refusal: the same capability story instantiation used to tell, now told
/// at first use (the gate shims are linked unconditionally — design §3.1 v1 rule — so a
/// netless boot refuses at the first l4 call instead of at instantiation).
fn no_grant_refusal() -> wasmtime::Error {
    wasmtime::Error::msg(
        "the program requires the network, which this boot does not provide (no \
         eo9:net/l4 share is wired; compose a transport — `net.l4.loopback $ …` — or \
         boot a config with a `share`/`use` clause)",
    )
}

/// The grant ids of the calling store, or the typed refusal.
fn grant_ids(accessor: &Accessor<KernelState>) -> Result<(usize, u32)> {
    accessor.with(|mut access| {
        access
            .get()
            .gate
            .as_ref()
            .map(|handle| (handle.share, handle.grant))
            .ok_or_else(no_grant_refusal)
    })
}

/// Submit and await one gate call from a child shim.
async fn gate_call(
    accessor: &Accessor<KernelState>,
    op: GateOp,
) -> core::result::Result<GateDone, WitL4Error> {
    let (share, grant) = match grant_ids(accessor) {
        Ok(ids) => ids,
        Err(_) => {
            return Err(WitL4Error::Io(
                "the network capability was not granted to this task".to_string(),
            ));
        }
    };
    let call = submit(share, grant, op).map_err(|reason| severed_error(&reason))?;
    GateCallFuture { call }.await
}

/// Take a child buffer's bytes for the gate crossing.
fn child_buffer_take(accessor: &Accessor<KernelState>, rep: u32) -> Result<Vec<u8>> {
    accessor.with(|mut access| access.get().shell_buffers()?.take(rep))
}

/// Restore round-tripped bytes under the same child rep and hand the own handle back.
fn child_buffer_restore(
    accessor: &Accessor<KernelState>,
    rep: u32,
    bytes: Vec<u8>,
) -> Resource<BufferRes> {
    accessor.with(|mut access| {
        if let Ok(buffers) = access.get().shell_buffers() {
            buffers.restore(rep, bytes);
        }
    });
    Resource::new_own(rep)
}

/// Register the typed `eo9:net/l4` gate shims (design §3.1: registered unconditionally
/// in the spawn linker — collision-free because the kernel links no root provider for
/// `eo9:net/*`; a store with no grant answers the capability refusal).
pub fn add_l4_gate(linker: &mut Linker<KernelState>) -> Result<()> {
    let mut l4 = linker.instance("eo9:net/l4@0.1.0")?;

    let drop_handle = |store: &mut StoreContextMut<'_, KernelState>, rep: u32| {
        if let Some(handle) = store.data().gate.as_ref() {
            drop_child_handle(handle.share, handle.grant, rep);
        }
    };
    l4.resource(
        "l4-impl",
        ResourceType::host::<GateL4Res>(),
        // Rep 0 (the handler) is released at grant teardown, not per handle drop.
        |_store: StoreContextMut<'_, KernelState>, _rep| Ok(()),
    )?;
    l4.resource(
        "tcp-connection",
        ResourceType::host::<GateConnRes>(),
        move |mut store: StoreContextMut<'_, KernelState>, rep| {
            drop_handle(&mut store, rep);
            Ok(())
        },
    )?;
    l4.resource(
        "tcp-listener",
        ResourceType::host::<GateListenerRes>(),
        move |mut store: StoreContextMut<'_, KernelState>, rep| {
            drop_handle(&mut store, rep);
            Ok(())
        },
    )?;
    l4.resource(
        "udp-socket",
        ResourceType::host::<GateUdpRes>(),
        move |mut store: StoreContextMut<'_, KernelState>, rep| {
            drop_handle(&mut store, rep);
            Ok(())
        },
    )?;

    l4.func_wrap(
        "default",
        |store: StoreContextMut<'_, KernelState>, (): ()| -> Result<(Resource<GateL4Res>,)> {
            match store.data().gate.as_ref() {
                // Rep 0 = the grant's handler (minted lazily inside the owner).
                Some(_) => Ok((Resource::new_own(0),)),
                None => Err(no_grant_refusal()),
            }
        },
    )?;

    l4.func_wrap_concurrent(
        "connect",
        |accessor: &Accessor<KernelState>,
         (_l4, remote): (Resource<GateL4Res>, WitSocketAddress)|
         -> ConcurrentFuture<'_, (core::result::Result<Resource<GateConnRes>, WitL4Error>,)> {
            Box::pin(async move {
                Ok((match gate_call(accessor, GateOp::Connect { remote }).await {
                    Ok(GateDone::Handle(Ok(rep))) => Ok(Resource::new_own(rep)),
                    Ok(GateDone::Handle(Err(error))) => Err(error),
                    Ok(_) => Err(WitL4Error::Io("malformed gate payload".to_string())),
                    Err(error) => Err(error),
                },))
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "listen",
        |accessor: &Accessor<KernelState>,
         (_l4, local): (Resource<GateL4Res>, WitSocketAddress)|
         -> ConcurrentFuture<'_, (core::result::Result<Resource<GateListenerRes>, WitL4Error>,)> {
            Box::pin(async move {
                Ok((match gate_call(accessor, GateOp::Listen { local }).await {
                    Ok(GateDone::Handle(Ok(rep))) => Ok(Resource::new_own(rep)),
                    Ok(GateDone::Handle(Err(error))) => Err(error),
                    Ok(_) => Err(WitL4Error::Io("malformed gate payload".to_string())),
                    Err(error) => Err(error),
                },))
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "accept",
        |accessor: &Accessor<KernelState>,
         (listener,): (Resource<GateListenerRes>,)|
         -> ConcurrentFuture<
            '_,
            (
                core::result::Result<(Resource<GateConnRes>, WitSocketAddress), WitL4Error>,
            ),
        > {
            let listener = listener.rep();
            Box::pin(async move {
                Ok((match gate_call(accessor, GateOp::Accept { listener }).await {
                    Ok(GateDone::Accepted(Ok((rep, peer)))) => Ok((Resource::new_own(rep), peer)),
                    Ok(GateDone::Accepted(Err(error))) => Err(error),
                    Ok(_) => Err(WitL4Error::Io("malformed gate payload".to_string())),
                    Err(error) => Err(error),
                },))
            })
        },
    )?;

    // The address getters are sync in the WIT; the host side still goes through the
    // queue as a concurrent shim (stackful async lets a sync-lowered guest import wait
    // on a pending host future). Pure metadata reads inside the owner — one queue round
    // trip, no parking beyond it.
    l4.func_wrap_concurrent(
        "listener-address",
        |accessor: &Accessor<KernelState>,
         (listener,): (Resource<GateListenerRes>,)|
         -> ConcurrentFuture<'_, (WitSocketAddress,)> {
            let listener = listener.rep();
            Box::pin(async move {
                match gate_call(accessor, GateOp::ListenerAddress { listener }).await {
                    Ok(GateDone::Addr(addr)) => Ok((addr,)),
                    Ok(_) => Err(wasmtime::Error::msg("malformed gate payload")),
                    Err(error) => Err(wasmtime::Error::msg(format!(
                        "listener-address failed through the gate: {error:?}"
                    ))),
                }
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "peer-address",
        |accessor: &Accessor<KernelState>,
         (conn,): (Resource<GateConnRes>,)|
         -> ConcurrentFuture<'_, (WitSocketAddress,)> {
            let conn = conn.rep();
            Box::pin(async move {
                match gate_call(accessor, GateOp::PeerAddress { conn }).await {
                    Ok(GateDone::Addr(addr)) => Ok((addr,)),
                    Ok(_) => Err(wasmtime::Error::msg("malformed gate payload")),
                    Err(error) => Err(wasmtime::Error::msg(format!(
                        "peer-address failed through the gate: {error:?}"
                    ))),
                }
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "send",
        |accessor: &Accessor<KernelState>,
         (conn, src): (Resource<GateConnRes>, Resource<BufferRes>)|
         -> ConcurrentFuture<
            '_,
            (
                (
                    Resource<BufferRes>,
                    core::result::Result<WitSendResult, WitL4Error>,
                ),
            ),
        > {
            let conn = conn.rep();
            let src_rep = src.rep();
            Box::pin(async move {
                let bytes = child_buffer_take(accessor, src_rep)?;
                let (bytes, result) = match gate_call(accessor, GateOp::Send { conn, bytes }).await
                {
                    Ok(GateDone::Io(bytes, result)) => {
                        (bytes, result.map(|bytes_sent| WitSendResult { bytes_sent }))
                    }
                    Ok(_) => (
                        Vec::new(),
                        Err(WitL4Error::Io("malformed gate payload".to_string())),
                    ),
                    Err(error) => (Vec::new(), Err(error)),
                };
                let src = child_buffer_restore(accessor, src_rep, bytes);
                Ok(((src, result),))
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "recv",
        |accessor: &Accessor<KernelState>,
         (conn, dst): (Resource<GateConnRes>, Resource<BufferRes>)|
         -> ConcurrentFuture<
            '_,
            (
                (
                    Resource<BufferRes>,
                    core::result::Result<WitRecvResult, WitL4Error>,
                ),
            ),
        > {
            let conn = conn.rep();
            let dst_rep = dst.rep();
            Box::pin(async move {
                let bytes = child_buffer_take(accessor, dst_rep)?;
                let (bytes, result) = match gate_call(accessor, GateOp::Recv { conn, bytes }).await
                {
                    Ok(GateDone::Io(bytes, result)) => (
                        bytes,
                        result.map(|bytes_received| WitRecvResult { bytes_received }),
                    ),
                    Ok(_) => (
                        Vec::new(),
                        Err(WitL4Error::Io("malformed gate payload".to_string())),
                    ),
                    Err(error) => (Vec::new(), Err(error)),
                };
                let dst = child_buffer_restore(accessor, dst_rep, bytes);
                Ok(((dst, result),))
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "bind-udp",
        |accessor: &Accessor<KernelState>,
         (_l4, local): (Resource<GateL4Res>, WitSocketAddress)|
         -> ConcurrentFuture<'_, (core::result::Result<Resource<GateUdpRes>, WitL4Error>,)> {
            Box::pin(async move {
                Ok((match gate_call(accessor, GateOp::BindUdp { local }).await {
                    Ok(GateDone::Handle(Ok(rep))) => Ok(Resource::new_own(rep)),
                    Ok(GateDone::Handle(Err(error))) => Err(error),
                    Ok(_) => Err(WitL4Error::Io("malformed gate payload".to_string())),
                    Err(error) => Err(error),
                },))
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "udp-address",
        |accessor: &Accessor<KernelState>,
         (socket,): (Resource<GateUdpRes>,)|
         -> ConcurrentFuture<'_, (WitSocketAddress,)> {
            let socket = socket.rep();
            Box::pin(async move {
                match gate_call(accessor, GateOp::UdpAddress { socket }).await {
                    Ok(GateDone::Addr(addr)) => Ok((addr,)),
                    Ok(_) => Err(wasmtime::Error::msg("malformed gate payload")),
                    Err(error) => Err(wasmtime::Error::msg(format!(
                        "udp-address failed through the gate: {error:?}"
                    ))),
                }
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "send-to",
        |accessor: &Accessor<KernelState>,
         (socket, remote, src): (Resource<GateUdpRes>, WitSocketAddress, Resource<BufferRes>)|
         -> ConcurrentFuture<
            '_,
            (
                (
                    Resource<BufferRes>,
                    core::result::Result<WitSendResult, WitL4Error>,
                ),
            ),
        > {
            let socket = socket.rep();
            let src_rep = src.rep();
            Box::pin(async move {
                let bytes = child_buffer_take(accessor, src_rep)?;
                let (bytes, result) = match gate_call(
                    accessor,
                    GateOp::SendTo {
                        socket,
                        remote,
                        bytes,
                    },
                )
                .await
                {
                    Ok(GateDone::Io(bytes, result)) => {
                        (bytes, result.map(|bytes_sent| WitSendResult { bytes_sent }))
                    }
                    Ok(_) => (
                        Vec::new(),
                        Err(WitL4Error::Io("malformed gate payload".to_string())),
                    ),
                    Err(error) => (Vec::new(), Err(error)),
                };
                let src = child_buffer_restore(accessor, src_rep, bytes);
                Ok(((src, result),))
            })
        },
    )?;

    l4.func_wrap_concurrent(
        "recv-from",
        |accessor: &Accessor<KernelState>,
         (socket, dst): (Resource<GateUdpRes>, Resource<BufferRes>)|
         -> ConcurrentFuture<
            '_,
            (
                (
                    Resource<BufferRes>,
                    core::result::Result<(WitRecvResult, WitSocketAddress), WitL4Error>,
                ),
            ),
        > {
            let socket = socket.rep();
            let dst_rep = dst.rep();
            Box::pin(async move {
                let bytes = child_buffer_take(accessor, dst_rep)?;
                let (bytes, result) =
                    match gate_call(accessor, GateOp::RecvFrom { socket, bytes }).await {
                        Ok(GateDone::IoFrom(bytes, result)) => (
                            bytes,
                            result.map(|(bytes_received, sender)| {
                                (WitRecvResult { bytes_received }, sender)
                            }),
                        ),
                        Ok(_) => (
                            Vec::new(),
                            Err(WitL4Error::Io("malformed gate payload".to_string())),
                        ),
                        Err(error) => (Vec::new(), Err(error)),
                    };
                let dst = child_buffer_restore(accessor, dst_rep, bytes);
                Ok(((dst, result),))
            })
        },
    )?;

    Ok(())
}

/// Whether a compiled component still imports `eo9:net/l4` (unfused) — the spawn-time
/// trigger for minting a console grant.
pub(super) fn component_imports_l4(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name == L4_INTERFACE)
}

// -----------------------------------------------------------------------------------------
// The R2 bring-up check (`gatecheck` boot token)
// -----------------------------------------------------------------------------------------
//
// Design risk R2, the M1 bring-up gate that must pass BEFORE any gate traffic is
// trusted: `run_concurrent` + multiple concurrent guest calls in ONE store on the
// no_std vendored wasmtime (the kernel only exercised `call_async` until now). The
// vehicle is the in-memory loopback transport: `accept` is started before `connect`
// and parks inside the guest until the connect lands, then `recv` is started before
// `send` and parks until the bytes arrive — both joins complete only if wasmtime
// genuinely interleaves concurrent export calls inside one store. A serialized or
// wedged implementation deadlocks and the executor watchdog fails the check loudly.

/// Run the R2 check against the baked-in `net.l4.loopback`; prints PASS/FAIL.
pub fn r2_check(entries: &'static [super::store::StoreEntry]) {
    crate::kprintln!(
        "gate r2: run_concurrent + concurrent export calls in one store (net.l4.loopback)"
    );
    match try_r2(entries) {
        Ok(report) => crate::kprintln!("gate r2: PASS — {report}"),
        Err(error) => crate::kprintln!("gate r2: FAILED — {error:?}"),
    }
}

fn try_r2(entries: &'static [super::store::StoreEntry]) -> Result<String> {
    let loopback = entries
        .iter()
        .find(|entry| entry.name == "net.l4.loopback")
        .ok_or_else(|| wasmtime::Error::msg("the store has no `net.l4.loopback` entry"))?;

    let engine = super::new_engine()?;
    // SAFETY: the artifact comes from the store image produced by `cargo xtask
    // build-kernel` with the same wasmtime version and engine configuration.
    let component =
        unsafe { wasmtime::component::Component::deserialize(&engine, loopback.artifact)? };

    let mut linker: Linker<KernelState> = Linker::new(&engine);
    super::providers::add_providers(&mut linker)?;
    super::shellfs::add_buffers(&mut linker)?;

    let mut state = KernelState::new();
    state.shell = Some(Box::new(super::shell::ShellState {
        fs: super::shellfs::ShellFs::new(entries, String::new()),
        buffers: super::shellfs::BufferTable::default(),
        exec: super::shellexec::ShellExec::default(),
        engine: engine.clone(),
    }));
    let mut store = wasmtime::Store::new(&engine, state);
    store.set_fuel(u64::MAX)?;

    let instance = super::block_on(
        "r2 instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let lookup = |store: &mut wasmtime::Store<KernelState>, name: &str| {
        exported_func(&instance, store, L4_INTERFACE, name)
            .map_err(wasmtime::Error::msg)
    };
    let f_default = lookup(&mut store, "default")?;
    let f_listen = lookup(&mut store, "listen")?;
    let f_accept = lookup(&mut store, "accept")?;
    let f_connect = lookup(&mut store, "connect")?;
    let f_send = lookup(&mut store, "send")?;
    let f_recv = lookup(&mut store, "recv")?;

    const PAYLOAD: &[u8] = b"gate-r2-interleave-proof";
    let addr = WitSocketAddress {
        address: WitIpAddress::V4((127, 0, 0, 1)),
        port: 7777,
    };

    let report = super::block_on(
        "r2 run_concurrent",
        store.run_concurrent(async |accessor| -> Result<String> {
            let one_result = |func: Func, params: Vec<Val>| async move {
                let mut results = vec![Val::Bool(false)];
                func.call_concurrent(accessor, &params, &mut results).await?;
                Ok::<Val, wasmtime::Error>(results.remove(0))
            };
            let unwrap_handle = |value: Val, what: &str| match value {
                Val::Result(Ok(Some(payload))) => match *payload {
                    Val::Resource(any) => Ok(any),
                    other => Err(wasmtime::Error::msg(format!(
                        "{what} returned a non-resource: {other:?}"
                    ))),
                },
                other => Err(wasmtime::Error::msg(format!("{what} failed: {other:?}"))),
            };

            // default() → the root handle.
            let l4 = match one_result(f_default, vec![]).await? {
                Val::Resource(any) => any,
                other => {
                    return Err(wasmtime::Error::msg(format!(
                        "default returned a non-resource: {other:?}"
                    )));
                }
            };

            // listen(l4, 127.0.0.1:7777).
            let listener = unwrap_handle(
                one_result(f_listen, vec![Val::Resource(l4), addr_to_val(&addr)]).await?,
                "listen",
            )?;

            // THE R2 DISCRIMINATOR: two concurrent guest calls in flight inside one
            // store, joined. The loopback transport never blocks (typed errors instead
            // of parks — its documented contract), so the ordering is backlog-shaped:
            // one connection is queued first, then `accept` (draining the backlog) and
            // a second `connect` (refilling it) run as joined concurrent calls. The
            // parked-call flavor of R2 — a gate call suspended inside the owner while
            // others flow — is exercised by the smoltcp owner (station-net's curl leg),
            // whose recv genuinely parks on RX.
            let first = unwrap_handle(
                one_result(f_connect, vec![Val::Resource(l4), addr_to_val(&addr)]).await?,
                "connect",
            )?;
            let _ = first;
            let accept_fut = one_result(
                f_accept,
                vec![Val::Resource(listener)],
            );
            let connect_fut = one_result(
                f_connect,
                vec![Val::Resource(l4), addr_to_val(&addr)],
            );
            let (accepted, connected) = join2(accept_fut, connect_fut).await;
            let accepted = match accepted? {
                Val::Result(Ok(Some(payload))) => match *payload {
                    Val::Tuple(pair) => match &pair[0] {
                        Val::Resource(any) => *any,
                        other => {
                            return Err(wasmtime::Error::msg(format!(
                                "accept returned a non-resource: {other:?}"
                            )));
                        }
                    },
                    other => {
                        return Err(wasmtime::Error::msg(format!(
                            "malformed accept payload: {other:?}"
                        )));
                    }
                },
                other => return Err(wasmtime::Error::msg(format!("accept failed: {other:?}"))),
            };
            let client = unwrap_handle(connected?, "connect")?;

            // The client end accepted second: drain the backlog the joined connect
            // refilled, so the send/recv pair below crosses one established pair.
            let accepted2 = match one_result(f_accept, vec![Val::Resource(listener)]).await? {
                Val::Result(Ok(Some(payload))) => match *payload {
                    Val::Tuple(pair) => match &pair[0] {
                        Val::Resource(any) => *any,
                        other => {
                            return Err(wasmtime::Error::msg(format!(
                                "accept returned a non-resource: {other:?}"
                            )));
                        }
                    },
                    other => {
                        return Err(wasmtime::Error::msg(format!(
                            "malformed accept payload: {other:?}"
                        )));
                    }
                },
                other => return Err(wasmtime::Error::msg(format!("accept failed: {other:?}"))),
            };
            // Send/recv joined (send first in the join — the loopback recv answers a
            // typed error rather than parking when no bytes are queued).
            let mint = |bytes: Vec<u8>| {
                accessor.with(|mut access| {
                    let rep = access.get().shell_buffers()?.insert(bytes);
                    Resource::<BufferRes>::new_own(rep).try_into_resource_any(&mut access)
                })
            };
            let dst = mint(vec![0u8; PAYLOAD.len()])?;
            let src = mint(PAYLOAD.to_vec())?;
            let sent =
                one_result(f_send, vec![Val::Resource(client), Val::Resource(src)]).await?;
            let _ = sent;
            // `client` is the SECOND connection's client end; its server end is
            // `accepted2` (the backlog is FIFO: the joined accept took the first).
            let _ = accepted;
            let received =
                one_result(f_recv, vec![Val::Resource(accepted2), Val::Resource(dst)]).await?;

            // The received buffer's bytes must equal the payload.
            let bytes = match &received {
                Val::Tuple(pair) => accessor.with(|mut access| {
                    let Val::Resource(any) = &pair[0] else {
                        return Err(wasmtime::Error::msg("recv returned a non-buffer"));
                    };
                    let resource =
                        Resource::<BufferRes>::try_from_resource_any(*any, &mut access)?;
                    access.get().shell_buffers()?.take(resource.rep())
                })?,
                other => {
                    return Err(wasmtime::Error::msg(format!(
                        "malformed recv return: {other:?}"
                    )));
                }
            };
            if bytes != PAYLOAD {
                return Err(wasmtime::Error::msg(format!(
                    "payload mismatch: sent {PAYLOAD:?}, received {bytes:?}"
                )));
            }
            Ok(format!(
                "accept+connect ran as joined concurrent calls in one store; {} payload \
                 bytes round-tripped (loopback never parks; the parked-call leg is the \
                 smoltcp owner's, proven by the station-net curl gate)",
                PAYLOAD.len()
            ))
        }),
    )???;

    Ok(report)
}

/// Join two futures (a hand-rolled `join!` — core has none, and the kernel pulls no
/// futures crate).
async fn join2<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = core::pin::pin!(a);
    let mut b = core::pin::pin!(b);
    let mut out_a = None;
    let mut out_b = None;
    core::future::poll_fn(move |cx| {
        if out_a.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(cx)
        {
            out_a = Some(value);
        }
        if out_b.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(cx)
        {
            out_b = Some(value);
        }
        if out_a.is_some() && out_b.is_some() {
            Poll::Ready((out_a.take().expect("checked"), out_b.take().expect("checked")))
        } else {
            Poll::Pending
        }
    })
    .await
}
