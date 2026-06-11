//! `net.l2.bridge` — the 802.1D learning bridge (plan/09 D42: the stacked-fan-out
//! question resolves as *separate providers* — this is the trusting one).
//!
//! Targets the `eo9:net/l2-bridge` world: imports ONE upstream `eo9:net/l2` (the
//! physical NIC's driver, another bridge, or a switch port — it stacks) and exports
//! the named ports `port-a` and `port-b`, each a full `eo9:net/l2` with its own
//! root-handle type (named exports mint per-port nominal types — SPEC,
//! "Multi-instance imports and type identity").
//!
//! **The capability stance (the whole point of this provider existing alongside
//! `net.l2.switch`):** the switch is an identity-enforcing attenuator — it rewrites
//! every source MAC to the port's own, so a consumer cannot spoof. The bridge is a
//! transparent segment that TRUSTS its ports: frames pass with their original source
//! MACs, so **handing a program a bridge port hands it the ability to claim any
//! link-layer identity on that segment**. Choosing which provider to compose is the
//! security decision; neither is a tuning knob on the other.
//!
//! Forwarding policy (classic 802.1D, the upstream treated as just another port):
//!
//! * **Learning.** Every ingress frame's source MAC is learned (source → port),
//!   refreshed on each sighting; a MAC migrating between ports updates to the last
//!   sighting. Multicast/broadcast sources (invalid on the wire) are never learned.
//!   The table is bounded at [`LEARN_CAP`] entries with
//!   least-recently-LEARNED eviction — eviction order follows source sightings, the
//!   same events that reset a real bridge's aging timer, but without a clock import:
//!   the provider stays pure and deterministic (the documented trade vs. real
//!   time-based aging: an idle station's entry survives until table pressure evicts
//!   it, instead of expiring on a timer).
//! * **Forwarding.** A known unicast destination goes to its learned port alone —
//!   including port-to-port local delivery that never touches the upstream. Unknown
//!   unicast is FLOODED to every other port (the deliberate opposite of the switch's
//!   drop policy: flooding is how learning converges). Broadcast/multicast goes to
//!   every other port. A frame is never reflected to its ingress port (a destination
//!   learned on the ingress port is filtered — same-segment traffic is not the
//!   bridge's business). Learning runs before the forwarding lookup, so an eviction
//!   caused by the current frame's own source is visible to its own lookup
//!   (deterministic, documented).
//! * **Delivery atomicity.** A forward that needs the upstream acquires it first;
//!   local copies (the sibling's flood/broadcast share) are enqueued only once the
//!   upstream send succeeded, so a typed failure means nothing was delivered.
//! * **No STP.** Composition wiring is a DAG: loops are unconstructible by the
//!   algebra itself, so there is no spanning-tree machinery to need.
//!
//! Advertised MACs: each port's `interface-info.mac` is a *suggestion* for consumers
//! that source from it (the l4 middleware does; so does `vnicheck`) — the bridge never
//! checks or rewrites it. They derive from a configured base (`l2-bridge-config`,
//! same format/validation as the switch's): `port-a` = base+1, `port-b` = base+2 in
//! the last octet (wrapping). The unconfigured default base is `02:e0:09:00:01:00`
//! (documented-defaults rule) — deliberately distinct from the switch's default, so
//! mixed switch/bridge stacks at defaults never advertise colliding addresses.
//!
//! Async discipline matches the converted switch (plan/09 D40/D41): every uplink call
//! is a genuine await; the uplink lives in a take/put slot with a claim flag and a
//! bring-up guard armed from the instant the claim exists; while one port holds the
//! uplink parked mid-await, the sibling's `recv-frame` serves its own queue and
//! answers "nothing waiting" when empty, while operations that need the uplink answer
//! a typed busy error; the sync `info` reads link parameters cached at bring-up.

#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "l2-bridge",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the l2 interfaces use but the world
    // does not name directly.
    generate_all,
});

use eo9::net::l2 as uplink_l2;
use exports::eo9::net::l2_bridge_config;

// ------------------------------------------------------------------------------------------
// Constants and configuration.
// ------------------------------------------------------------------------------------------

/// The documented default advertised-MAC base (locally administered, multicast clear;
/// distinct from the switch's default so mixed stacks at defaults never collide).
const DEFAULT_BASE: [u8; 6] = [0x02, 0xe0, 0x09, 0x00, 0x01, 0x00];
/// Frames a port's receive queue holds; the oldest is dropped on overflow.
const RX_QUEUE_CAP: usize = 32;
/// Largest frame the bridge will queue or forward (Ethernet + generous slack).
const MAX_FRAME_BYTES: u64 = 2048;
/// Receive attempts against the uplink per drain (one drain per port `recv-frame`).
const DRAIN_BATCH: usize = 16;
/// Learning-table capacity; the least-recently-learned entry is evicted when full.
const LEARN_CAP: usize = 64;
/// The Ethernet broadcast address.
const BROADCAST: [u8; 6] = [0xff; 6];

/// Set exactly once, by `configure`; absent means the documented default base.
static BASE: ProviderState<[u8; 6]> = ProviderState::new();

fn mac_base() -> [u8; 6] {
    if BASE.is_set() {
        BASE.with(|base| *base)
    } else {
        DEFAULT_BASE
    }
}

/// One of the two consumer ports. All port logic is shared and parameterized by this.
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    A,
    B,
}

impl Slot {
    fn index(self) -> usize {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }

    fn sibling(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    fn interface_name(self) -> &'static str {
        match self {
            Slot::A => "bridge-a",
            Slot::B => "bridge-b",
        }
    }

    /// The port's ADVERTISED MAC (a suggestion, never enforced): the base with the
    /// last octet advanced by slot index + 1 (wrapping).
    fn advertised_mac(self) -> [u8; 6] {
        let mut mac = mac_base();
        mac[5] = mac[5].wrapping_add(self.index() as u8 + 1);
        mac
    }
}

/// Where a learned MAC lives: one of the consumer ports, or the upstream segment.
#[derive(Clone, Copy, PartialEq)]
enum BridgePort {
    Consumer(Slot),
    Uplink,
}

// ------------------------------------------------------------------------------------------
// Errors (the same shape as the switch's).
// ------------------------------------------------------------------------------------------

enum BridgeError {
    Denied,
    LinkDown,
    FrameTooLarge,
    NoSuchInterface,
    Io(String),
}

/// The typed busy error: the sibling port holds the uplink, parked mid-await upstream.
fn busy(what: &str) -> BridgeError {
    BridgeError::Io(format!(
        "{what}: the uplink is busy with the sibling port's operation"
    ))
}

fn uplink_failure(err: uplink_l2::L2Error) -> BridgeError {
    match err {
        uplink_l2::L2Error::Denied => BridgeError::Denied,
        uplink_l2::L2Error::LinkDown => BridgeError::LinkDown,
        uplink_l2::L2Error::FrameTooLarge => BridgeError::FrameTooLarge,
        other => BridgeError::Io(format!("uplink: {other:?}")),
    }
}

// ------------------------------------------------------------------------------------------
// Bridge state: the opened uplink, the learning table, and the per-port queues.
// ------------------------------------------------------------------------------------------

struct Uplink {
    iface: uplink_l2::L2Interface,
    mtu: u32,
    up: bool,
}

struct BridgeState {
    /// `Some` once the upstream interface is open. Taken out of the slot (with
    /// `claimed` set) for the duration of any uplink call — no `ProviderState` borrow
    /// is ever held across an await, and the operation may genuinely park.
    uplink: Option<Uplink>,
    /// True while the uplink is taken out of the slot or being opened: a second
    /// activation arriving meanwhile must never open a second upstream interface.
    claimed: bool,
    /// Link parameters cached at bring-up, so the sync `info` (no error channel)
    /// never has to touch the uplink.
    params: Option<(u32, bool)>,
    /// The learning table, ordered oldest→newest by last learn (index 0 is the
    /// eviction victim). Bounded at [`LEARN_CAP`].
    learned: Vec<([u8; 6], BridgePort)>,
    /// Per-port receive queues (index = `Slot::index`).
    rx: [VecDeque<Vec<u8>>; 2],
}

static STATE: ProviderState<BridgeState> = ProviderState::new();

fn with_state<R>(f: impl FnOnce(&mut BridgeState) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(BridgeState {
            uplink: None,
            claimed: false,
            params: None,
            learned: Vec::new(),
            rx: [VecDeque::new(), VecDeque::new()],
        });
    }
    STATE.with(f)
}

/// Puts the uplink back in its slot (and releases the claim) when the operation that
/// took it finishes — on every exit path, including a future dropped mid-await.
struct UplinkGuard(Option<Uplink>);

impl Drop for UplinkGuard {
    fn drop(&mut self) {
        if let Some(uplink) = self.0.take() {
            with_state(|state| {
                state.uplink = Some(uplink);
                state.claimed = false;
            });
        }
    }
}

impl core::ops::Deref for UplinkGuard {
    type Target = Uplink;
    fn deref(&self) -> &Uplink {
        self.0
            .as_ref()
            .expect("the uplink is held for the guard's lifetime")
    }
}

/// What taking the uplink slot found.
enum UplinkView {
    Ready(Uplink),
    Busy,
    NeedOpen,
}

fn take_uplink() -> UplinkView {
    with_state(|state| {
        if let Some(uplink) = state.uplink.take() {
            state.claimed = true;
            UplinkView::Ready(uplink)
        } else if state.claimed {
            UplinkView::Busy
        } else {
            state.claimed = true;
            UplinkView::NeedOpen
        }
    })
}

/// Take the uplink for one operation, opening it on first use. `Ok(None)` means the
/// sibling port holds it right now (parked mid-await upstream); the caller picks the
/// policy — `recv` answers "nothing waiting" (the sibling's drain fills both queues),
/// everything else answers the typed busy error. Never opens a second interface.
async fn acquire_uplink() -> Result<Option<UplinkGuard>, BridgeError> {
    match take_uplink() {
        UplinkView::Ready(uplink) => Ok(Some(UplinkGuard(Some(uplink)))),
        UplinkView::Busy => Ok(None),
        UplinkView::NeedOpen => {
            // `claimed` is set from `take_uplink` above: arm the restore before the
            // first await of bring-up (plan/09 D41), so an error return *or a future
            // dropped mid-open* releases the claim and the next use retries.
            let claim = BringUpClaim { armed: true };
            let opened = open_uplink().await?;
            claim.defuse();
            Ok(Some(opened))
        }
    }
}

/// Releases the bring-up claim (`claimed`) if the first-use open never completes;
/// armed from the instant the claim exists, defused on success when the
/// [`UplinkGuard`] takes over the claim's lifecycle.
struct BringUpClaim {
    armed: bool,
}

impl BringUpClaim {
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl Drop for BringUpClaim {
    fn drop(&mut self) {
        if self.armed {
            with_state(|state| state.claimed = false);
        }
    }
}

/// First-use bring-up: list the upstream provider's interfaces, take the first, open
/// it, and cache its link parameters for the sync `info`.
async fn open_uplink() -> Result<UplinkGuard, BridgeError> {
    let root = uplink_l2::default();
    let interfaces = uplink_l2::list_interfaces(&root)
        .await
        .map_err(uplink_failure)?;
    let first = interfaces
        .first()
        .ok_or_else(|| BridgeError::Io(String::from("the upstream l2 exposes no interfaces")))?;
    let iface = uplink_l2::open_interface(&root, first.name.clone())
        .await
        .map_err(uplink_failure)?;
    with_state(|state| state.params = Some((first.mtu, first.up)));
    Ok(UplinkGuard(Some(Uplink {
        iface,
        mtu: first.mtu,
        up: first.up,
    })))
}

// ------------------------------------------------------------------------------------------
// Learning and forwarding (the 802.1D core).
// ------------------------------------------------------------------------------------------

/// Learn (or refresh) `mac` on `port`. Refreshing moves the entry to the
/// most-recently-learned end; inserting into a full table evicts the
/// least-recently-learned entry (index 0). Broadcast/multicast sources are never
/// learned (invalid as a source on the wire).
fn learn(mac: [u8; 6], port: BridgePort) {
    if mac[0] & 0x01 != 0 {
        return;
    }
    with_state(|state| {
        if let Some(at) = state.learned.iter().position(|(seen, _)| *seen == mac) {
            state.learned.remove(at);
        } else if state.learned.len() == LEARN_CAP {
            state.learned.remove(0);
        }
        state.learned.push((mac, port));
    });
}

fn lookup(mac: [u8; 6]) -> Option<BridgePort> {
    with_state(|state| {
        state
            .learned
            .iter()
            .find(|(seen, _)| *seen == mac)
            .map(|(_, port)| *port)
    })
}

/// Where one ingress frame goes (after learning).
enum Forward {
    /// Filtered: the destination lives on the ingress port (or the frame is a runt).
    Drop,
    /// Known unicast to one consumer port — local delivery, the upstream never sees it.
    Local(Slot),
    /// Known unicast to the upstream segment.
    Upstream,
    /// Broadcast/multicast or unknown unicast: every port except the ingress one.
    /// `to_consumer` is the consumer-side share (`None` for uplink-ingress flooding to
    /// both consumer ports); `and_upstream` says whether the upstream gets a copy.
    Flood {
        to_consumer: Option<Slot>,
        and_upstream: bool,
    },
}

/// The forwarding decision for a frame entering on `ingress`. Learning has already
/// run (learn-then-lookup: an eviction caused by this frame's own source is visible
/// to its own lookup — deterministic and documented).
fn decide(ingress: BridgePort, dst: [u8; 6]) -> Forward {
    let broadcast_or_multicast = dst == BROADCAST || (dst[0] & 0x01) != 0;
    if broadcast_or_multicast {
        return match ingress {
            BridgePort::Consumer(slot) => Forward::Flood {
                to_consumer: Some(slot.sibling()),
                and_upstream: true,
            },
            BridgePort::Uplink => Forward::Flood {
                to_consumer: None,
                and_upstream: false,
            },
        };
    }
    match lookup(dst) {
        Some(learned) if learned == ingress => Forward::Drop,
        Some(BridgePort::Consumer(slot)) => Forward::Local(slot),
        Some(BridgePort::Uplink) => Forward::Upstream,
        None => match ingress {
            BridgePort::Consumer(slot) => Forward::Flood {
                to_consumer: Some(slot.sibling()),
                and_upstream: true,
            },
            BridgePort::Uplink => Forward::Flood {
                to_consumer: None,
                and_upstream: false,
            },
        },
    }
}

/// Bounded enqueue: drop the oldest frame on overflow (newest-wins).
fn enqueue(slot: Slot, frame: Vec<u8>) {
    with_state(|state| {
        let queue = &mut state.rx[slot.index()];
        if queue.len() == RX_QUEUE_CAP {
            queue.pop_front();
        }
        queue.push_back(frame);
    });
}

/// Deliver one frame that arrived FROM the upstream (during a drain): learn its
/// source on the uplink, then forward to the consumer side per the decision.
fn ingress_from_uplink(frame: Vec<u8>) {
    if frame.len() < 14 {
        return; // runt: not addressable
    }
    let dst: [u8; 6] = frame[0..6].try_into().expect("six bytes");
    let src: [u8; 6] = frame[6..12].try_into().expect("six bytes");
    learn(src, BridgePort::Uplink);
    match decide(BridgePort::Uplink, dst) {
        Forward::Drop | Forward::Upstream => {}
        Forward::Local(slot) => enqueue(slot, frame),
        Forward::Flood { .. } => {
            enqueue(Slot::A, frame.clone());
            enqueue(Slot::B, frame);
        }
    }
}

/// Pull whatever the uplink has waiting (bounded) and run each frame through the
/// uplink-ingress path. Each `recv-frame` await is bounded by the upstream's own
/// contract (an idle link answers `bytes-received: 0` rather than waiting), and
/// `DRAIN_BATCH` caps the awaits per drain.
async fn drain_uplink(uplink: &Uplink) -> Result<(), BridgeError> {
    for _ in 0..DRAIN_BATCH {
        let dst = uplink_l2::Buffer::new(MAX_FRAME_BYTES);
        let (dst, received) = uplink_l2::recv_frame(&uplink.iface, dst).await;
        match received {
            Ok(result) if result.bytes_received > 0 => {
                let frame = dst.read(0, result.bytes_received.min(MAX_FRAME_BYTES));
                ingress_from_uplink(frame);
            }
            // Nothing waiting right now: the drain is done.
            Ok(_) => break,
            Err(err) => return Err(uplink_failure(err)),
        }
    }
    Ok(())
}

/// Send one already-validated frame out the upstream link.
async fn send_upstream(uplink: &Uplink, frame: &[u8]) -> Result<u64, BridgeError> {
    let buffer = uplink_l2::Buffer::new(frame.len() as u64);
    buffer.write(0, frame);
    let (_buffer, sent) = uplink_l2::send_frame(&uplink.iface, buffer).await;
    let result = sent.map_err(uplink_failure)?;
    Ok(result.bytes_sent)
}

// ------------------------------------------------------------------------------------------
// The per-port operations.
// ------------------------------------------------------------------------------------------

/// `list-interfaces` for one port: exactly one interface, advertising the port's
/// suggested MAC and the uplink's link parameters.
async fn port_list(slot: Slot) -> Result<(String, [u8; 6], u32, bool), BridgeError> {
    let uplink = acquire_uplink()
        .await?
        .ok_or_else(|| busy("list-interfaces"))?;
    Ok((
        String::from(slot.interface_name()),
        slot.advertised_mac(),
        uplink.mtu,
        uplink.up,
    ))
}

/// `open-interface` for one port: the port's single interface name, strictly.
async fn port_open(slot: Slot, name: &str) -> Result<(), BridgeError> {
    let _uplink = acquire_uplink()
        .await?
        .ok_or_else(|| busy("open-interface"))?;
    if name == slot.interface_name() {
        Ok(())
    } else {
        Err(BridgeError::NoSuchInterface)
    }
}

/// `send-frame` for one port: learn the frame's source on this port, then forward per
/// 802.1D — UNCHANGED, with its original source MAC (the bridge never rewrites).
/// Returns the byte count accepted.
async fn port_send(slot: Slot, frame: Vec<u8>) -> Result<u64, BridgeError> {
    if frame.len() < 14 {
        return Err(BridgeError::Io(format!(
            "frame too short to be Ethernet ({} bytes)",
            frame.len()
        )));
    }
    if frame.len() as u64 > MAX_FRAME_BYTES {
        return Err(BridgeError::FrameTooLarge);
    }
    let dst: [u8; 6] = frame[0..6].try_into().expect("six bytes");
    let src: [u8; 6] = frame[6..12].try_into().expect("six bytes");
    let ingress = BridgePort::Consumer(slot);
    learn(src, ingress);

    match decide(ingress, dst) {
        // Same-segment traffic: accepted and filtered (classic 802.1D).
        Forward::Drop => Ok(frame.len() as u64),
        // Local port-to-port delivery: the upstream never sees the frame.
        Forward::Local(to) => {
            let len = frame.len() as u64;
            enqueue(to, frame);
            Ok(len)
        }
        Forward::Upstream => {
            let uplink = acquire_uplink().await?.ok_or_else(|| busy("send-frame"))?;
            send_upstream(&uplink, &frame).await
        }
        Forward::Flood {
            to_consumer,
            and_upstream,
        } => {
            // Acquire the uplink before delivering anything, so a typed busy/error
            // means NOTHING was delivered (atomic success — see the module docs).
            debug_assert!(and_upstream, "consumer-ingress floods include the upstream");
            let uplink = acquire_uplink().await?.ok_or_else(|| busy("send-frame"))?;
            let sent = send_upstream(&uplink, &frame).await?;
            drop(uplink);
            if let Some(sibling) = to_consumer {
                enqueue(sibling, frame);
            }
            Ok(sent)
        }
    }
}

/// `recv-frame` for one port: serve the port's queue, draining the uplink (which
/// learns and forwards for both ports) when the queue is empty. An empty result
/// (`bytes-received: 0`) means nothing is waiting — the consumer owns the retry,
/// including the sibling-holds-the-uplink case.
async fn port_recv(slot: Slot) -> Result<Option<Vec<u8>>, BridgeError> {
    let queued = with_state(|state| state.rx[slot.index()].pop_front());
    if let Some(frame) = queued {
        return Ok(Some(frame));
    }
    let Some(uplink) = acquire_uplink().await? else {
        return Ok(None);
    };
    drain_uplink(&uplink).await?;
    drop(uplink);
    Ok(with_state(|state| state.rx[slot.index()].pop_front()))
}

/// `wait-recv` for one port: the documented poll-fallback — return immediately, so a
/// consumer's wait loop degrades to exactly the poll cadence it ran before the
/// operation existed. Forwarding the wait to the uplink's own `wait-recv` is NOT done
/// deliberately (the same reasoning as the switch): the uplink is an exclusive slot
/// shared by both ports, and a port parked on it for the caller's bound would starve
/// the sibling's every operation into the typed busy answer for the duration. An
/// event-driven park here needs a cross-port wake — recorded as the follow-up next to
/// the A2 board leg; until then both ports stay honestly polled.
async fn port_wait_recv(slot: Slot, _max_wait_ns: u64) -> Result<(), BridgeError> {
    let _ = slot;
    Ok(())
}

// ------------------------------------------------------------------------------------------
// The two exported ports (the same macro shape as the switch: each named export mints
// its own nominal types, and this is where they meet the shared logic).
// ------------------------------------------------------------------------------------------

struct Stub;

macro_rules! port_binding {
    ($module:ident, $slot:expr, $impl_name:ident, $iface_name:ident, $error:ident) => {
        /// The port's root-handle resource: a token — the port identity is the module.
        struct $impl_name;
        /// The port's opened-interface resource: likewise a token.
        struct $iface_name;

        impl exports::$module::GuestL2Impl for $impl_name {}
        impl exports::$module::GuestL2Interface for $iface_name {}

        /// Map the shared bridge error into this port module's own error type.
        fn $error(err: BridgeError) -> exports::$module::L2Error {
            use exports::$module::L2Error;
            match err {
                BridgeError::Denied => L2Error::Denied,
                BridgeError::LinkDown => L2Error::LinkDown,
                BridgeError::FrameTooLarge => L2Error::FrameTooLarge,
                BridgeError::NoSuchInterface => L2Error::NoSuchInterface,
                BridgeError::Io(message) => L2Error::Io(message),
            }
        }

        impl exports::$module::Guest for Stub {
            type L2Impl = $impl_name;
            type L2Interface = $iface_name;

            fn default() -> exports::$module::L2Impl {
                exports::$module::L2Impl::new($impl_name)
            }

            async fn list_interfaces(
                _l2: exports::$module::L2ImplBorrow<'_>,
            ) -> Result<Vec<exports::$module::InterfaceInfo>, exports::$module::L2Error> {
                let (name, mac, mtu, up) = port_list($slot).await.map_err($error)?;
                Ok(alloc::vec![exports::$module::InterfaceInfo {
                    name,
                    mac: (mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]),
                    mtu,
                    up,
                }])
            }

            async fn open_interface(
                _l2: exports::$module::L2ImplBorrow<'_>,
                name: String,
            ) -> Result<exports::$module::L2Interface, exports::$module::L2Error> {
                port_open($slot, &name).await.map_err($error)?;
                Ok(exports::$module::L2Interface::new($iface_name))
            }

            fn info(
                _iface: exports::$module::L2InterfaceBorrow<'_>,
            ) -> exports::$module::InterfaceInfo {
                // Best-effort link parameters: `info` is sync with no error channel,
                // so it reads what bring-up cached and never touches the uplink
                // (`(0, false)` before the first successful open — the disk
                // size-reports-0 shape).
                let (mtu, up) = with_state(|state| state.params).unwrap_or((0, false));
                let mac = $slot.advertised_mac();
                exports::$module::InterfaceInfo {
                    name: String::from($slot.interface_name()),
                    mac: (mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]),
                    mtu,
                    up,
                }
            }

            async fn send_frame(
                _iface: exports::$module::L2InterfaceBorrow<'_>,
                frame: exports::$module::Buffer,
            ) -> (
                exports::$module::Buffer,
                Result<exports::$module::SendResult, exports::$module::L2Error>,
            ) {
                let len = frame.len();
                let bytes = frame.read(0, len);
                let outcome = port_send($slot, bytes)
                    .await
                    .map(|bytes_sent| exports::$module::SendResult { bytes_sent })
                    .map_err($error);
                (frame, outcome)
            }

            async fn wait_recv(
                _iface: exports::$module::L2InterfaceBorrow<'_>,
                max_wait_ns: u64,
            ) -> Result<(), exports::$module::L2Error> {
                port_wait_recv($slot, max_wait_ns).await.map_err($error)
            }

            async fn recv_frame(
                _iface: exports::$module::L2InterfaceBorrow<'_>,
                dst: exports::$module::Buffer,
            ) -> (
                exports::$module::Buffer,
                Result<exports::$module::RecvResult, exports::$module::L2Error>,
            ) {
                match port_recv($slot).await {
                    Ok(Some(frame)) => {
                        let take = (frame.len() as u64).min(dst.len());
                        dst.write(0, &frame[..take as usize]);
                        (
                            dst,
                            Ok(exports::$module::RecvResult {
                                bytes_received: take,
                            }),
                        )
                    }
                    Ok(None) => (dst, Ok(exports::$module::RecvResult { bytes_received: 0 })),
                    Err(err) => (dst, Err($error(err))),
                }
            }
        }
    };
}

port_binding!(port_a, Slot::A, PortAImpl, PortAIface, port_a_error);
port_binding!(port_b, Slot::B, PortBImpl, PortBIface, port_b_error);

// ------------------------------------------------------------------------------------------
// Configuration.
// ------------------------------------------------------------------------------------------

/// Parse a colon-separated MAC (`"02:e0:09:00:01:00"`). Configure-time validation only —
/// a malformed value is a configure error, never a trap.
fn parse_mac(text: &str) -> Result<[u8; 6], String> {
    let mut mac = [0u8; 6];
    let mut count = 0;
    for part in text.split(':') {
        if count == 6 {
            return Err(format!("not a colon-separated MAC address: {text:?}"));
        }
        mac[count] = u8::from_str_radix(part, 16)
            .map_err(|_| format!("not a colon-separated MAC address: {text:?}"))?;
        count += 1;
    }
    if count != 6 {
        return Err(format!("not a colon-separated MAC address: {text:?}"));
    }
    Ok(mac)
}

impl l2_bridge_config::Guest for Stub {
    fn configure(mac_base: String) -> Result<(), String> {
        let mac = parse_mac(&mac_base)?;
        if mac[0] & 0x02 == 0 {
            return Err(format!(
                "the MAC base must be locally administered (bit 0x02 of the first octet): {mac_base:?}"
            ));
        }
        if mac[0] & 0x01 != 0 {
            return Err(format!(
                "the MAC base must not be a multicast address (bit 0x01 of the first octet): {mac_base:?}"
            ));
        }
        BASE.set(mac);
        Ok(())
    }
}

export!(Stub);
