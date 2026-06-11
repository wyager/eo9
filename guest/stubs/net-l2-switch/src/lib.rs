//! `net.l2.switch` — the virtual-NIC switch (the single-owner-NIC sharing story;
//! docs/design/executor-model.md §6, plan/09).
//!
//! Targets the `eo9:net/l2-switch` world: imports ONE upstream `eo9:net/l2` (the
//! physical NIC's driver — `net.virtio` on metal — or another switch: it stacks) and
//! exports two virtual NICs as the named ports `port-a` and `port-b`. Each port is a
//! full `eo9:net/l2` with exactly one interface (`vnic-a` / `vnic-b`) carrying its own
//! locally-administered MAC, and each named export mints its own root-handle type, so a
//! consumer wired to one port cannot reach the other even in principle (SPEC,
//! "Multi-instance imports and type identity").
//!
//! Switching policy (deliberate, recorded in plan/09):
//!
//! * **Send path.** A frame a port sends is forwarded upstream immediately, with its
//!   Ethernet source overwritten by the port's virtual MAC — a consumer cannot spoof
//!   its sibling (or the uplink) at the Ethernet layer. (A consumer can still write
//!   its own MACs *inside* protocol payloads such as the ARP sender-hardware-address;
//!   payload-level anti-spoofing is a recorded follow-up, and does not weaken the
//!   isolation property below: seeing traffic is governed by the receive path.)
//! * **Receive path.** Inbound frames are demuxed by destination MAC: broadcast and
//!   multicast are delivered to every port; a port's own unicast goes to that port
//!   alone; unknown unicast (including frames addressed to the uplink's real MAC) is
//!   dropped — never flooded — so one consumer can never observe another's traffic.
//!   There is no hairpin path: port-to-port traffic goes upstream like everything
//!   else, and whether it comes back is the upstream network's business.
//! * **Queues.** Each port has a bounded receive queue (32 frames); on overflow the
//!   OLDEST frame is dropped (newest-wins: replies correlate with recent requests,
//!   stale frames are the right victim). The uplink is drained only inside a port's
//!   `recv-frame` (consumer-pull, the same convention as the TCP/IP middleware), and a
//!   drain demuxes into BOTH queues, so a busy consumer keeps its idle sibling's queue
//!   warm rather than stealing its frames.
//! * **Honest awaits, one uplink, two ports.** Every uplink call is a genuine await
//!   (the SPEC's "boundaries are honestly async" rule), so the upstream may park us —
//!   and the uplink can be stacked (another switch, itself awaiting). The uplink lives
//!   in a take/put slot with a claim flag; while one port is parked mid-await holding
//!   it, the sibling is never wedged: its `recv-frame` serves its own queue (filled by
//!   whichever port pumps) and reports "nothing waiting" when the queue is empty — the
//!   consumer owns the retry, per the l2 contract — while `send-frame` (and the
//!   bring-up operations) report a typed busy error rather than opening a second
//!   upstream interface. `info` (sync, no error channel) reads link parameters cached
//!   at bring-up and never touches the uplink.
//!
//! Virtual MACs derive deterministically from a configured base (`l2-switch-config`,
//! colon-separated hex, locally-administered required): `port-a` = base+1, `port-b` =
//! base+2 in the last octet (wrapping). The unconfigured default base is
//! `02:e0:09:00:00:00` (documented-defaults rule: composition without `configure`
//! works and never traps).

#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "l2-switch",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the l2 interfaces use but the world
    // does not name directly.
    generate_all,
});

use eo9::net::l2 as uplink_l2;
use exports::eo9::net::l2_switch_config;

// ------------------------------------------------------------------------------------------
// Constants and configuration.
// ------------------------------------------------------------------------------------------

/// The documented default MAC base (locally administered, multicast clear).
const DEFAULT_BASE: [u8; 6] = [0x02, 0xe0, 0x09, 0x00, 0x00, 0x00];
/// Frames a port's receive queue holds; the oldest is dropped on overflow.
const RX_QUEUE_CAP: usize = 32;
/// Largest frame the switch will queue or forward (Ethernet + generous slack).
const MAX_FRAME_BYTES: u64 = 2048;
/// Receive attempts against the uplink per drain (one drain per port `recv-frame`).
const DRAIN_BATCH: usize = 16;
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

/// One of the two ports. All port logic is shared and parameterized by this.
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

    fn interface_name(self) -> &'static str {
        match self {
            Slot::A => "vnic-a",
            Slot::B => "vnic-b",
        }
    }

    /// The port's virtual MAC: the base with the last octet advanced by slot index + 1
    /// (wrapping), so distinct ports always have distinct addresses.
    fn mac(self) -> [u8; 6] {
        let mut mac = mac_base();
        mac[5] = mac[5].wrapping_add(self.index() as u8 + 1);
        mac
    }
}

// ------------------------------------------------------------------------------------------
// Errors. Every uplink import call below is a genuine await (the async-first doctrine);
// the only switch-introduced failure beyond the uplink's own is the typed busy error a
// non-recv operation gets while the sibling port holds the uplink mid-await.
// ------------------------------------------------------------------------------------------

/// A switch-internal failure, mapped into each port module's own (structurally
/// identical) error type at the export boundary.
enum SwitchError {
    Denied,
    LinkDown,
    FrameTooLarge,
    NoSuchInterface,
    Io(String),
}

/// The typed busy error: the sibling port holds the uplink, parked mid-await upstream.
fn busy(what: &str) -> SwitchError {
    SwitchError::Io(format!(
        "{what}: the uplink is busy with the sibling port's operation"
    ))
}

/// The uplink's own error, preserved in kind where the kinds align.
fn uplink_failure(err: uplink_l2::L2Error) -> SwitchError {
    match err {
        uplink_l2::L2Error::Denied => SwitchError::Denied,
        uplink_l2::L2Error::LinkDown => SwitchError::LinkDown,
        uplink_l2::L2Error::FrameTooLarge => SwitchError::FrameTooLarge,
        other => SwitchError::Io(format!("uplink: {other:?}")),
    }
}

// ------------------------------------------------------------------------------------------
// Switch state: the opened uplink and the per-port receive queues.
// ------------------------------------------------------------------------------------------

struct Uplink {
    iface: uplink_l2::L2Interface,
    mtu: u32,
    up: bool,
}

struct SwitchState {
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
    /// Per-port receive queues (index = `Slot::index`).
    rx: [VecDeque<Vec<u8>>; 2],
}

static STATE: ProviderState<SwitchState> = ProviderState::new();

fn with_state<R>(f: impl FnOnce(&mut SwitchState) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(SwitchState {
            uplink: None,
            claimed: false,
            params: None,
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
async fn acquire_uplink() -> Result<Option<UplinkGuard>, SwitchError> {
    match take_uplink() {
        UplinkView::Ready(uplink) => Ok(Some(UplinkGuard(Some(uplink)))),
        UplinkView::Busy => Ok(None),
        UplinkView::NeedOpen => {
            // `claimed` is set from `take_uplink` above: arm the restore before the
            // first await of bring-up, so an error return *or a future dropped
            // mid-open* releases the claim and the next use retries (instead of
            // wedging both ports behind the busy answer).
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
async fn open_uplink() -> Result<UplinkGuard, SwitchError> {
    let root = uplink_l2::default();
    let interfaces = uplink_l2::list_interfaces(&root)
        .await
        .map_err(uplink_failure)?;
    let first = interfaces
        .first()
        .ok_or_else(|| SwitchError::Io(String::from("the upstream l2 exposes no interfaces")))?;
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
// The switching logic (shared by both ports).
// ------------------------------------------------------------------------------------------

/// Deliver one inbound frame per the demux policy: broadcast/multicast to every port,
/// a port's own unicast to that port alone, unknown unicast dropped (never flooded).
fn demux(frame: Vec<u8>) {
    if frame.len() < 14 {
        return; // runt: not addressable
    }
    let dst: [u8; 6] = frame[0..6].try_into().expect("six bytes");
    let broadcast_or_multicast = dst == BROADCAST || (dst[0] & 0x01) != 0;
    if broadcast_or_multicast {
        enqueue(Slot::A, frame.clone());
        enqueue(Slot::B, frame);
    } else if dst == Slot::A.mac() {
        enqueue(Slot::A, frame);
    } else if dst == Slot::B.mac() {
        enqueue(Slot::B, frame);
    }
    // Anything else — including the uplink's own real MAC — is dropped, deliberately.
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

/// Pull whatever the uplink has waiting (bounded) and demux it into the port queues.
/// Each `recv-frame` await is bounded by the upstream's own contract (an idle link
/// answers `bytes-received: 0` rather than waiting for traffic), and `DRAIN_BATCH`
/// caps the awaits per drain.
async fn drain_uplink(uplink: &Uplink) -> Result<(), SwitchError> {
    for _ in 0..DRAIN_BATCH {
        let dst = uplink_l2::Buffer::new(MAX_FRAME_BYTES);
        let (dst, received) = uplink_l2::recv_frame(&uplink.iface, dst).await;
        match received {
            Ok(result) if result.bytes_received > 0 => {
                let frame = dst.read(0, result.bytes_received.min(MAX_FRAME_BYTES));
                demux(frame);
            }
            // Nothing waiting right now: the drain is done.
            Ok(_) => break,
            Err(err) => return Err(uplink_failure(err)),
        }
    }
    Ok(())
}

/// `list-interfaces` for one port: exactly one virtual NIC, with the uplink's link
/// parameters and the port's own MAC.
async fn port_list(slot: Slot) -> Result<(String, [u8; 6], u32, bool), SwitchError> {
    let uplink = acquire_uplink()
        .await?
        .ok_or_else(|| busy("list-interfaces"))?;
    Ok((
        String::from(slot.interface_name()),
        slot.mac(),
        uplink.mtu,
        uplink.up,
    ))
}

/// `open-interface` for one port: the port's single interface name, strictly.
async fn port_open(slot: Slot, name: &str) -> Result<(), SwitchError> {
    let _uplink = acquire_uplink()
        .await?
        .ok_or_else(|| busy("open-interface"))?;
    if name == slot.interface_name() {
        Ok(())
    } else {
        Err(SwitchError::NoSuchInterface)
    }
}

/// `send-frame` for one port: overwrite the Ethernet source with the port's virtual
/// MAC and forward upstream. Returns the byte count accepted.
async fn port_send(slot: Slot, frame_bytes: Vec<u8>) -> Result<u64, SwitchError> {
    if frame_bytes.len() < 14 {
        return Err(SwitchError::Io(format!(
            "frame too short to be Ethernet ({} bytes)",
            frame_bytes.len()
        )));
    }
    if frame_bytes.len() as u64 > MAX_FRAME_BYTES {
        return Err(SwitchError::FrameTooLarge);
    }
    let mut frame = frame_bytes;
    frame[6..12].copy_from_slice(&slot.mac());

    let uplink = acquire_uplink().await?.ok_or_else(|| busy("send-frame"))?;
    let buffer = uplink_l2::Buffer::new(frame.len() as u64);
    buffer.write(0, &frame);
    let (_buffer, sent) = uplink_l2::send_frame(&uplink.iface, buffer).await;
    let result = sent.map_err(uplink_failure)?;
    Ok(result.bytes_sent)
}

/// `recv-frame` for one port: serve the port's queue, draining the uplink (and
/// demuxing for both ports) when the queue is empty. An empty result (`bytes-received:
/// 0`) means nothing is waiting — the consumer owns the wait policy. That includes the
/// sibling-holds-the-uplink case: the sibling's drain demuxes into BOTH queues, so
/// "nothing waiting yet, retry" is the honest answer, never a wedge.
async fn port_recv(slot: Slot) -> Result<Option<Vec<u8>>, SwitchError> {
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
/// deliberately: the uplink is an exclusive slot shared by both ports, and a port
/// parked on it for the caller's bound would starve the sibling's every operation into
/// the typed busy answer for the duration. An event-driven park here needs a cross-port
/// wake (the sibling's drain waking this port's parked wait) — recorded as the
/// follow-up next to the A2 board leg; until then both ports stay honestly polled.
async fn port_wait_recv(slot: Slot, _max_wait_ns: u64) -> Result<(), SwitchError> {
    // Nothing waiting is the normal answer; a frame already demuxed into the port's
    // queue returns just the same — the consumer re-polls either way.
    let _ = slot;
    Ok(())
}

// ------------------------------------------------------------------------------------------
// The two exported ports. The macro instantiates the identical binding once per port
// module: each named export mints its own (nominal) resource and error types, so this
// is the one place the per-port types meet the shared logic.
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

        /// Map the shared switch error into this port module's own error type.
        fn $error(err: SwitchError) -> exports::$module::L2Error {
            use exports::$module::L2Error;
            match err {
                SwitchError::Denied => L2Error::Denied,
                SwitchError::LinkDown => L2Error::LinkDown,
                SwitchError::FrameTooLarge => L2Error::FrameTooLarge,
                SwitchError::NoSuchInterface => L2Error::NoSuchInterface,
                SwitchError::Io(message) => L2Error::Io(message),
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
                let mac = $slot.mac();
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

/// Parse a colon-separated MAC (`"02:e0:09:00:00:00"`). Configure-time validation only —
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

impl l2_switch_config::Guest for Stub {
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
