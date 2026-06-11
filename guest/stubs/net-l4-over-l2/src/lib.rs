//! `net.l4.over-l2` — a TCP/IP stack as ordinary provider middleware.
//!
//! Targets the crate-local `eo9:net-l4-over-l2/l4-over-l2` world: imports the link
//! layer (`eo9:net/l2`), the clock (`eo9:time/time`) and entropy
//! (`eo9:entropy/entropy`), and exports transport sockets (`eo9:net/l4`), so a program
//! that speaks only `l4` gets working TCP and UDP when an l2 provider (the
//! `net.virtio` driver on metal, a mock in tests) is composed below it:
//!
//! ```text
//! net.virtio $ net.l4.over-l2 $ program
//! ```
//!
//! The engine is [smoltcp] (no_std + alloc; Ethernet + IPv4 + TCP + UDP only). The
//! provider's own surface area is deliberately small:
//!
//! * **Addressing.** The documented default is QEMU user-mode networking's layout —
//!   `10.0.2.15/24` with gateway `10.0.2.2` — bound lazily on first use, so plain
//!   composition works and never traps (plan/09 Decision 14). The exported
//!   `eo9:net/l4-over-l2-config` entry binds different addressing: static values, or
//!   `--address dhcp` to acquire address, prefix length, and gateway from the network's
//!   DHCP service on first use (smoltcp's built-in DHCPv4 client — discover → offer →
//!   request → ack — gated before any l4 operation is served; the lease is announced in
//!   one console line, and no lease within the bounded window is a typed error, never a
//!   trap or a hang). DNS servers offered by the lease are logged AND reported through
//!   the l4 `dns-servers` introspection (lease-gated like every other operation; the
//!   unconfigured QEMU default layout reports that layout's forwarder, 10.0.2.3, and
//!   explicitly configured static addressing reports none): the middleware still sends
//!   no DNS queries itself — resolvers live *above* `l4`. Lease renewal (T1/T2) is
//!   handled inside smoltcp's client as long as the stack is pumped, which in this
//!   executor model means **while l4 operations are in flight** — an idle stack does
//!   not renew. Sessions here are short-lived relative to any real lease, so this is
//!   documented rather than worked around; a lease that does expire mid-pump
//!   deconfigures the stack honestly and the next operation re-acquires.
//! * **Driving the link.** Every l2 import call is a genuine await (the SPEC's
//!   "boundaries are honestly async" rule): each exported l4 operation pumps the link
//!   — transmit what the stack queued, receive what the device has, let smoltcp
//!   process it — awaiting each frame exchange, until the operation completes or its
//!   deadline passes. An l2 provider that completes within the call (a leaf driver
//!   over the host) resolves on the spot; one that suspends (a switch port, any
//!   forwarding middleware) parks this operation, and the consumer above absorbs that
//!   by awaiting its own l4 call. smoltcp itself never touches the link: its device
//!   abstraction runs over in-memory frame queues ([`QueueDevice`]), and all I/O
//!   happens between `poll`s in the pump — so the sync stack core and the async link
//!   never meet on one call stack. An empty pump round does not spin: the wait loop
//!   parks on the link's receive event (`l2::wait-recv`, the net.virtio RX interrupt
//!   on metal), bounded by the operation's window and the stack's next timed
//!   obligation (`poll_delay`), so an idle listener costs the machine nothing while
//!   protocol deadlines still fire on time; a link with no receive event answers the
//!   wait immediately and the loop polls exactly as it always did. Nothing here blocks
//!   forever: every wait is bounded by its operation's wall-clock deadline plus the
//!   frozen-clock backstop.
//! * **Bounds.** At most 16 sockets (TCP + UDP combined), 16 KiB TCP buffers per
//!   direction, 8 × 1536 B received / 4 × 1536 B queued UDP datagrams per socket, a
//!   32-frame receive queue, and per-operation wall-clock deadlines (4 s receive, 6 s
//!   connect, 1.5 s send-flush, 20 s DHCP acquisition) — honestly time-bounded (the
//!   clock is read every pump round; a round count never cuts a window short), with a
//!   consecutive-rounds-without-clock-movement backstop so even a frozen test clock
//!   cannot loop forever ([`FROZEN_CLOCK_ROUNDS`]).
//! * **Errors.** The l2 layer refusing (`denied`) surfaces as the l4 `denied`; every
//!   other link or stack problem is a typed l4 error (`timed-out`,
//!   `connection-refused`, `io(...)`, …) — never a trap, regardless of what arrives on
//!   the wire (smoltcp drops malformed frames).
//!
//! [smoltcp]: https://docs.rs/smoltcp

#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;

use eo9_guest::provider::ProviderState;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    DhcpRepr, EthernetAddress, HardwareAddress, IpAddress as SmolIpAddress, IpCidr, IpEndpoint,
    Ipv4Address,
};

wit_bindgen::generate!({
    world: "l4-over-l2",
    path: "wit",
    // Pull in bindings for eo9:io/buffers and eo9:time/types, which the imported and
    // exported interfaces use but the world does not name directly.
    generate_all,
});

// The user-facing manual, embedded as the `eo9-manual` custom section and rendered by
// `man net.l4.over-l2` in eosh (docs/design/component-manuals.md).
eo9_guest::manual! {
    name: "net.l4.over-l2",
    synopsis: "TCP and UDP over any link-layer provider — the TCP/IP stack as middleware",
    description: [
        "A whole TCP/IP stack (Ethernet + IPv4 + TCP + UDP) as ordinary provider middleware: compose it",
        "over a link-layer provider and a program that speaks only transport sockets gets working TCP",
        "and UDP. Unconfigured, it binds QEMU user networking's layout — 10.0.2.15/24, gateway 10.0.2.2 —",
        "lazily on first use, so plain composition always works. With --address dhcp it leases address,",
        "prefix, and gateway from the network on first use and announces the lease in one console line;",
        "on a real LAN that line is where to reach the stack. No lease within the bounded window is a",
        "typed error, never a hang. DNS servers the addressing teaches the stack — the lease's offer, or",
        "the unconfigured QEMU layout's forwarder 10.0.2.3 — are reported to consumers through the l4",
        "dns-servers introspection (curl's default resolver); configured static addressing reports none.",
    ],
    args: [
        { name: "address", ty: "string", required,
          doc: "`dhcp` to lease addressing from the network on first use, or a static dotted quad",
          values: "dhcp" },
        { name: "prefix-length", ty: "u8", optional,
          doc: "subnet prefix length for a static address (default 24)" },
        { name: "gateway", ty: "string", optional,
          doc: "IPv4 gateway for a static address, dotted quad" },
    ],
    examples: [
        { line: "net.virtio $ net.l4.over-l2 $ l4check",
          doc: "QEMU: the default user-net addressing, no configuration" },
        { line: "net.virtio $ (net.l4.over-l2 --address dhcp) $ l4check",
          doc: "lease addressing from the network's DHCP service" },
        { line: "net.rtl8125 $ (net.l4.over-l2 --address 10.20.3.70 --gateway 10.20.3.1) $ l4check",
          doc: "the board: static LAN addressing over the real NIC" },
    ],
    see_also: "net.virtio, net.rtl8125, l4check, telnetd",
}

use eo9::entropy::entropy;
use eo9::net::l2;
use eo9::text::text;
use eo9::time::time;
use exports::eo9::net::l4::{
    self, Buffer, IpAddress, L4Error, RecvResult, SendResult, SocketAddress,
};
use exports::eo9::net::l4_factory;
use exports::eo9::net::l4_over_l2_config;

// ------------------------------------------------------------------------------------------
// Defaults and bounds (all documented in the crate header).
// ------------------------------------------------------------------------------------------

/// The address QEMU user-mode networking hands its guest — the documented default.
const OUR_ADDRESS: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
/// The prefix length of the default address.
const PREFIX_LEN: u8 = 24;
/// The default gateway (QEMU user-mode networking's router).
const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
/// The DNS forwarder of the same documented QEMU user-net layout: what `dns-servers`
/// reports when the provider runs unconfigured (the layout comes as a set; an
/// explicitly configured static address reports no servers instead).
const USER_NET_DNS: Ipv4Address = Ipv4Address::new(10, 0, 2, 3);

// ------------------------------------------------------------------------------------------
// Configured addressing (`eo9:net/l4-over-l2-config`).
// ------------------------------------------------------------------------------------------

/// The static IPv4 addressing the stack binds on first use: the configured values, or the
/// documented QEMU user-net defaults when the provider was composed without `configure`.
#[derive(Clone, Copy)]
struct Addressing {
    address: Ipv4Address,
    prefix_len: u8,
    gateway: Ipv4Address,
}

impl Addressing {
    const fn defaults() -> Addressing {
        Addressing {
            address: OUR_ADDRESS,
            prefix_len: PREFIX_LEN,
            gateway: GATEWAY,
        }
    }
}

/// How the stack addresses its link: static IPv4 values (configured, or the documented
/// QEMU user-net defaults), or a DHCP lease acquired from the network on first use.
#[derive(Clone, Copy)]
enum AddressMode {
    Static(Addressing),
    Dhcp,
}

/// Set exactly once, by `configure`; absent for an unconfigured provider.
static MODE: ProviderState<AddressMode> = ProviderState::new();

/// The addressing mode in force: the configured mode when `configure` ran, the static
/// defaults otherwise (an unconfigured provider behaves exactly as it always has).
fn mode() -> AddressMode {
    if MODE.is_set() {
        MODE.with(|m| *m)
    } else {
        AddressMode::Static(Addressing::defaults())
    }
}

/// The static addressing to bind at bring-up, `None` when DHCP acquires it instead.
fn static_addressing() -> Option<Addressing> {
    match mode() {
        AddressMode::Static(addressing) => Some(addressing),
        AddressMode::Dhcp => None,
    }
}

/// Parse a dotted-quad IPv4 address (`"10.0.2.15"`). Configure-time validation only —
/// a malformed value is a configure error, never a trap.
fn parse_ipv4(text: &str) -> Result<Ipv4Address, String> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in text.split('.') {
        if count == 4 {
            return Err(format!("not a dotted-quad IPv4 address: {text:?}"));
        }
        octets[count] = part
            .parse::<u8>()
            .map_err(|_| format!("not a dotted-quad IPv4 address: {text:?}"))?;
        count += 1;
    }
    if count != 4 {
        return Err(format!("not a dotted-quad IPv4 address: {text:?}"));
    }
    Ok(Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]))
}

/// Sockets (TCP + UDP combined) that may exist at once.
const MAX_SOCKETS: usize = 16;
/// TCP receive/transmit buffer, per direction.
const TCP_BUFFER_BYTES: usize = 16 * 1024;
/// One UDP datagram slot (payload bytes).
const UDP_PACKET_BYTES: usize = 1536;
/// Received datagrams a UDP socket can hold.
const UDP_RX_PACKETS: usize = 8;
/// Queued outgoing datagrams a UDP socket can hold.
const UDP_TX_PACKETS: usize = 4;
/// Frames the receive queue holds before older device frames are left unread.
const RX_QUEUE_CAP: usize = 32;
/// Frame buffer handed to the l2 provider for one receive (MTU + Ethernet header slack).
const RX_BUFFER_BYTES: u64 = 2048;
/// Frames pulled from the l2 provider per pump round.
const RX_BATCH: usize = 4;

/// Deadline for receive-shaped operations (`recv`, `recv-from`, `accept`).
const RECV_DEADLINE_NS: u64 = 4_000_000_000;
/// Deadline for the TCP handshake.
const CONNECT_DEADLINE_NS: u64 = 6_000_000_000;
/// Deadline for flushing queued sends out of the stack.
const SEND_FLUSH_DEADLINE_NS: u64 = 1_500_000_000;
/// Frozen-clock backstop for [`wait_until`]: how many CONSECUTIVE pump rounds with zero
/// observed clock movement before the wait gives up. Every operation's wait is bounded
/// by its wall-clock deadline (the clock is read every round); this round bound exists
/// ONLY for a clock that genuinely never advances (a frozen test stub, where the
/// deadline can never expire), so it counts rounds *since the clock last moved* and
/// resets on every observed tick — with any advancing clock it never fires, and a round
/// count can no longer silently cut a wall-clock window short (the conflation bug the
/// DHCP lane hit: empty receive polls complete in microseconds, so a flat 4096-round cap
/// elapsed long before the intended window had honestly passed).
const FROZEN_CLOCK_ROUNDS: u32 = 4096;

/// Bounded window for acquiring a DHCP lease on first use: long enough for the full
/// discover → offer → request → ack exchange plus one of smoltcp's discover
/// retransmits (10 s apart by default — a first discover lost while a real link is
/// still settling must not doom the acquisition). An honest wall-clock window: the wait
/// is time-bounded (plus the frozen-clock backstop), never round-limited.
const DHCP_DEADLINE_NS: u64 = 20_000_000_000;

/// One best-effort console line (the net.virtio precedent: a diagnostic the operator
/// needs to see on the machine console; the provider works identically without one).
fn console(line: &str) {
    let handle = text::default();
    let _ = text::write(&handle, text::OutputStream::Out, line);
    let _ = text::write(&handle, text::OutputStream::Out, "\n");
}

/// The l2 layer's own error, in l4 vocabulary: a refusal stays a refusal, everything
/// else is a typed `io` error naming the layer.
fn l2_failure(err: l2::L2Error) -> L4Error {
    match err {
        l2::L2Error::Denied => L4Error::Denied,
        other => L4Error::Io(format!("l2: {other:?}")),
    }
}

fn table_full() -> L4Error {
    L4Error::Io(format!("socket table full ({MAX_SOCKETS} sockets)"))
}

// ------------------------------------------------------------------------------------------
// The link: the opened l2 interface plus the clock, taken out of its slot for the
// duration of one exported operation (no RefCell borrow is ever held across an l2 call).
// ------------------------------------------------------------------------------------------

struct Link {
    iface: l2::L2Interface,
    clock: time::TimeImpl,
}

struct LinkSlot {
    link: Option<Link>,
    /// Whether the link has been claimed for opening (set before the first awaited
    /// open, so a concurrent first use cannot open a second interface; cleared again if
    /// opening fails, so the next use retries).
    opened: bool,
}

static LINK: ProviderState<LinkSlot> = ProviderState::new();

/// Puts the link back in its slot when the operation that took it finishes.
struct LinkGuard(Option<Link>);

impl Drop for LinkGuard {
    fn drop(&mut self) {
        if let Some(link) = self.0.take() {
            LINK.with(|slot| slot.link = Some(link));
        }
    }
}

impl core::ops::Deref for LinkGuard {
    type Target = Link;
    fn deref(&self) -> &Link {
        self.0
            .as_ref()
            .expect("the link is held for the guard's lifetime")
    }
}

fn now_ns(clock: &time::TimeImpl) -> u64 {
    time::monotonic_now(clock).nanoseconds
}

fn smol_instant(ns: u64) -> Instant {
    Instant::from_micros((ns / 1_000) as i64)
}

/// What `acquire` found in the link slot.
enum LinkView {
    Ready(Link),
    Busy,
    NeedOpen,
}

/// Take the link for one operation, bringing it (and the smoltcp state) up on first
/// use: open the l2 provider's first interface, read its MAC address, seed the stack
/// from entropy, and bind the documented default address. A second activation arriving
/// while one is parked mid-operation gets a typed error, never a second interface.
async fn acquire() -> Result<LinkGuard, L4Error> {
    if !LINK.is_set() {
        LINK.set(LinkSlot {
            link: None,
            opened: false,
        });
    }
    let view = LINK.with(|slot| {
        if let Some(link) = slot.link.take() {
            LinkView::Ready(link)
        } else if slot.opened {
            LinkView::Busy
        } else {
            slot.opened = true;
            LinkView::NeedOpen
        }
    });
    match view {
        LinkView::Ready(link) => {
            let guard = LinkGuard(Some(link));
            ensure_dhcp_bound(&guard).await?;
            return Ok(guard);
        }
        LinkView::Busy => {
            return Err(L4Error::Io(String::from(
                "another l4 operation on this stack is in progress",
            )));
        }
        LinkView::NeedOpen => {}
    }

    // `opened` is set from the `with` above: arm the restore before the first await
    // of bring-up, so an error return *or a future dropped mid-open* clears the claim
    // and the next use retries (instead of wedging the stack behind the typed busy
    // answer).
    let claim = BringUpClaim { armed: true };
    let opened = open_link().await?;
    claim.defuse();
    // DHCP acquisition is gated here, after the claim is defused: a failed (or
    // dropped) acquisition keeps the opened link in its slot and the next operation
    // simply retries the wait — the link does not get re-opened.
    ensure_dhcp_bound(&opened).await?;
    Ok(opened)
}

/// In DHCP mode, gate every operation behind a bound lease: pump the link through the
/// discover → offer → request → ack exchange until the interface holds an address, or
/// answer typed (never trap, never hang) when no lease arrives within the bounded
/// window. Static mode — and an already-bound lease — returns immediately (the first
/// `check` runs before any pump touches the link).
async fn ensure_dhcp_bound(link: &LinkGuard) -> Result<(), L4Error> {
    let waiting = with_net(|n| n.dhcp.is_some() && n.bound.is_none());
    if !waiting {
        return Ok(());
    }
    let outcome = wait_until(link, DHCP_DEADLINE_NS, || {
        with_net(|n| n.bound.map(|_| Ok(())))
    })
    .await;
    match outcome {
        Ok(()) => Ok(()),
        Err(L4Error::TimedOut) => {
            // The no-answer note, on the console for the operator (the acquired line's
            // failure sibling) and typed to the caller.
            console(&format!(
                "net.l4.over-l2: dhcp no lease within {}s — check the link and the \
                 network's DHCP service",
                DHCP_DEADLINE_NS / 1_000_000_000
            ));
            Err(L4Error::Io(String::from(
                "dhcp: no lease arrived within the acquisition window",
            )))
        }
        Err(other) => Err(other),
    }
}

/// Releases the bring-up claim (`opened`) if the first-use open never completes; armed
/// from the instant the claim exists, defused on success when the [`LinkGuard`] takes
/// over (a successful open keeps `opened = true` for the instance's lifetime).
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
            LINK.with(|slot| slot.opened = false);
        }
    }
}

/// First-use bring-up: the awaited l2 opens plus the smoltcp state.
async fn open_link() -> Result<LinkGuard, L4Error> {
    let root = l2::default();
    let interfaces = l2::list_interfaces(&root).await.map_err(l2_failure)?;
    let first = interfaces
        .first()
        .ok_or_else(|| L4Error::Io(String::from("the l2 capability exposes no interfaces")))?;
    let (a, b, c, d, e, f) = first.mac;
    let mac = [a, b, c, d, e, f];
    let mtu = first.mtu.clamp(576, 9216) as usize;
    let iface = l2::open_interface(&root, first.name.clone())
        .await
        .map_err(l2_failure)?;

    let clock = time::default();
    let entropy_root = entropy::default();
    let seed = entropy::get_u64(&entropy_root);

    if !NET.is_set() {
        NET.set(NetState::new(mac, mtu, seed, now_ns(&clock)));
    }
    Ok(LinkGuard(Some(Link { iface, clock })))
}

// ------------------------------------------------------------------------------------------
// The smoltcp state: interface, sockets, and the in-memory frame queues the stack reads
// from and writes to. Only ever touched synchronously (never across an l2 call).
// ------------------------------------------------------------------------------------------

/// The frame queues smoltcp's device abstraction runs over: the pump moves frames
/// between these queues and the real l2 provider.
struct QueueDevice {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
    mtu: usize,
}

struct QueueRxToken(Vec<u8>);

struct QueueTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl smoltcp::phy::RxToken for QueueRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

impl smoltcp::phy::TxToken for QueueTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);
        self.0.push_back(frame);
        result
    }
}

impl Device for QueueDevice {
    type RxToken<'a>
        = QueueRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = QueueTxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((QueueRxToken(frame), QueueTxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(QueueTxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

struct NetState {
    iface: Interface,
    sockets: SocketSet<'static>,
    dev: QueueDevice,
    /// Next ephemeral port to hand out (connect sources and port-0 binds).
    ephemeral: u16,
    /// Sockets whose resource has been dropped but whose close handshake may still be
    /// in flight; swept (removed) once they reach the Closed state.
    closing: Vec<SocketHandle>,
    /// Sockets currently backed by a live resource handle.
    live: usize,
    /// Ports held by live `tcp-listener` resources. Tracked here (rather than inferred
    /// from socket state) because a listener whose underlying socket is mid-handshake or
    /// established-but-not-yet-accepted is *not* in the `Listen` state, yet its port is
    /// still taken.
    listening_ports: Vec<u16>,
    /// The DHCP client socket when `--address dhcp` selected acquisition; `None` in
    /// static mode. Lives outside the `live` resource count: it is the stack's own.
    dhcp: Option<SocketHandle>,
    /// The IPv4 address/prefix currently bound on the interface: set at construction in
    /// static mode, set (and re-set, and cleared on lease loss) by DHCP lease events.
    bound: Option<(Ipv4Address, u8)>,
    /// The DNS servers the current DHCP lease offered, in the lease's order; cleared
    /// with the lease. Always empty in static mode (the unconfigured QEMU default's
    /// forwarder is answered straight from the mode, never stored here).
    dns: Vec<Ipv4Address>,
}

impl NetState {
    fn new(mac: [u8; 6], mtu: usize, seed: u64, now: u64) -> NetState {
        let mut dev = QueueDevice {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
            mtu,
        };
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = seed;
        let mut iface = Interface::new(config, &mut dev, smol_instant(now));
        let mut sockets = SocketSet::new(Vec::new());
        let mut bound = None;
        let mut dhcp = None;
        match static_addressing() {
            Some(addressing) => {
                iface.update_ip_addrs(|addrs| {
                    let _ = addrs.push(IpCidr::new(
                        SmolIpAddress::Ipv4(addressing.address),
                        addressing.prefix_len,
                    ));
                });
                let _ = iface
                    .routes_mut()
                    .add_default_ipv4_route(addressing.gateway);
                bound = Some((addressing.address, addressing.prefix_len));
            }
            None => {
                let mut socket = dhcpv4::Socket::new();
                // Keep the raw ACK so the announced lease line can carry the lease
                // duration (smoltcp's `Config` does not surface it; its own wire
                // parser reads it back out of the packet). The buffer must outlive
                // the `'static` socket set, hence the one-time leak: one 1536-byte
                // allocation, exactly once, for the provider instance's lifetime.
                socket.set_receive_packet_buffer(alloc::boxed::Box::leak(
                    vec![0u8; UDP_PACKET_BYTES].into_boxed_slice(),
                ));
                dhcp = Some(sockets.add(socket));
            }
        }
        NetState {
            iface,
            sockets,
            dev,
            ephemeral: 49152u16.wrapping_add((seed % 16000) as u16),
            closing: Vec::new(),
            live: 0,
            listening_ports: Vec::new(),
            dhcp,
            bound,
            dns: Vec::new(),
        }
    }

    /// Hand out the next ephemeral port.
    fn ephemeral_port(&mut self) -> u16 {
        let port = self.ephemeral;
        self.ephemeral = if self.ephemeral == u16::MAX {
            49152
        } else {
            self.ephemeral + 1
        };
        port
    }

    /// Remove dropped sockets whose close handshake has finished.
    fn sweep(&mut self) {
        let mut still_closing = Vec::new();
        for handle in core::mem::take(&mut self.closing) {
            let closed = matches!(
                self.sockets.get::<tcp::Socket>(handle).state(),
                tcp::State::Closed
            );
            if closed {
                self.sockets.remove(handle);
            } else {
                still_closing.push(handle);
            }
        }
        self.closing = still_closing;
    }
}

static NET: ProviderState<NetState> = ProviderState::new();

fn with_net<R>(f: impl FnOnce(&mut NetState) -> R) -> R {
    NET.with(f)
}

/// Advance the stack against the current frame queues, applying any DHCP lease event
/// to the interface (address, prefix, default route) on the way. The lease announcement
/// is printed *outside* the state borrow (`text::write` is an import call; the standing
/// discipline keeps borrows away from the boundary).
fn poll_stack(link: &Link) {
    let timestamp = smol_instant(now_ns(&link.clock));
    let announcement = with_net(|n| {
        let _ = n.iface.poll(timestamp, &mut n.dev, &mut n.sockets);
        let handle = n.dhcp?;
        match n.sockets.get_mut::<dhcpv4::Socket>(handle).poll()? {
            dhcpv4::Event::Configured(lease) => {
                // The lease duration comes from the raw ACK (smoltcp's `Config` does
                // not carry it); best-effort — the announcement omits it if absent.
                let lease_seconds = lease
                    .packet
                    .as_ref()
                    .and_then(|packet| DhcpRepr::parse(packet).ok())
                    .and_then(|repr| repr.lease_duration);
                let address = lease.address.address();
                let prefix = lease.address.prefix_len();
                n.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(lease.address));
                });
                match lease.router {
                    Some(router) => {
                        let _ = n.iface.routes_mut().add_default_ipv4_route(router);
                    }
                    None => {
                        n.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                n.bound = Some((address, prefix));
                // Kept for the l4 `dns-servers` introspection (the lease's order is
                // its preference order); the console line below stays the operator's.
                n.dns = lease.dns_servers.iter().copied().collect();

                let mut line = format!("net.l4.over-l2: dhcp acquired {address}/{prefix}");
                match lease.router {
                    Some(router) => line.push_str(&format!(" gw {router}")),
                    None => line.push_str(" gw none"),
                }
                if !lease.dns_servers.is_empty() {
                    line.push_str(" dns");
                    for server in lease.dns_servers.iter() {
                        line.push_str(&format!(" {server}"));
                    }
                }
                if let Some(seconds) = lease_seconds {
                    line.push_str(&format!(" lease {seconds}s"));
                }
                Some(line)
            }
            dhcpv4::Event::Deconfigured => {
                // The client starts deconfigured (its first poll always reports it);
                // only a *lost* lease — expiry, or a NAKed renewal — is an event worth
                // acting on: deconfigure honestly (the address may be reassigned) and
                // let the next operation's acquisition gate re-acquire.
                let lost = n.bound.take().is_some();
                if lost {
                    n.dns.clear();
                    n.iface.update_ip_addrs(|addrs| addrs.clear());
                    n.iface.routes_mut().remove_default_ipv4_route();
                    Some(String::from(
                        "net.l4.over-l2: dhcp lease lost; reacquiring on the next operation",
                    ))
                } else {
                    None
                }
            }
        }
    });
    if let Some(line) = announcement {
        console(&line);
    }
}

/// Hand everything the stack queued to the l2 provider. Returns whether any frame was
/// handed over (progress, for the wait loop's park decision).
async fn flush_tx(link: &Link) -> Result<bool, L4Error> {
    let frames: Vec<Vec<u8>> = with_net(|n| n.dev.tx.drain(..).collect());
    let sent_any = !frames.is_empty();
    for frame in frames {
        let buffer = Buffer::new(frame.len() as u64);
        buffer.write(0, &frame);
        let (_buffer, sent) = l2::send_frame(&link.iface, buffer).await;
        match sent {
            Ok(_) => {}
            Err(l2::L2Error::Denied) => return Err(L4Error::Denied),
            // Any other send problem: drop the frame. TCP retransmits, and the
            // operation deadline reports persistent trouble.
            Err(_) => {}
        }
    }
    Ok(sent_any)
}

/// One pump round: let the stack emit what is due, hand it to the link, pull a few
/// frames the other way, and let the stack process them. Returns whether any frame
/// moved in either direction — `false` means the round was empty, so the caller may
/// park on the link's receive event instead of spinning another round.
async fn pump(link: &Link) -> Result<bool, L4Error> {
    poll_stack(link);
    let mut sent_any = flush_tx(link).await?;

    let mut received_any = false;
    for _ in 0..RX_BATCH {
        let dst = Buffer::new(RX_BUFFER_BYTES);
        let (dst, received) = l2::recv_frame(&link.iface, dst).await;
        match received {
            Ok(result) if result.bytes_received > 0 => {
                let frame = dst.read(0, result.bytes_received.min(RX_BUFFER_BYTES));
                with_net(|n| {
                    if n.dev.rx.len() < RX_QUEUE_CAP {
                        n.dev.rx.push_back(frame);
                    }
                });
                received_any = true;
            }
            // Nothing waiting right now (the link's short poll came back empty):
            // this pump round is done.
            Ok(_) => break,
            Err(l2::L2Error::Denied) => return Err(L4Error::Denied),
            // Transient receive trouble: drop out of this round and let the operation
            // deadline decide whether it matters.
            Err(_) => break,
        }
    }
    if received_any {
        poll_stack(link);
        sent_any |= flush_tx(link).await?;
    }
    Ok(received_any || sent_any)
}

/// Pump the link until `check` reports a result or the wall-clock deadline passes.
/// `check` runs before the first pump, so already-satisfiable operations never touch
/// the link.
///
/// The bound is honestly time-shaped: the clock is read every round and only the
/// deadline (or the result) ends the wait — a round count never cuts a wall-clock
/// window short, however cheap the empty receive polls are. The one round-shaped bound
/// left is the [`FROZEN_CLOCK_ROUNDS`] backstop, which fires only after that many
/// consecutive rounds with zero observed clock movement (a frozen test clock, where the
/// deadline can never expire) and keeps the wait finite even then.
///
/// **The wait parks, it does not spin** (the timer-crutch audit's A2): a pump round
/// that moved no frame, advanced no socket, and has nothing due means the only thing
/// that can change the outcome is traffic — so the loop parks on the link's receive
/// event (`l2::wait-recv`) instead of burning empty pump rounds. The park is bounded by
/// whichever comes first of the operation's remaining window and the stack's next timed
/// obligation (`poll_delay`: ARP/TCP retransmits, delayed ACKs, DHCP timers — protocol
/// deadlines stay deadline-bounded waits, never spin-counted), and the event provider
/// clamps it again with its own bound. An l2 provider with no receive event returns
/// immediately, degrading this loop to exactly the poll cadence it always had.
async fn wait_until<T>(
    link: &Link,
    deadline_ns: u64,
    mut check: impl FnMut() -> Option<Result<T, L4Error>>,
) -> Result<T, L4Error> {
    let start = now_ns(&link.clock);
    let mut last_now = start;
    let mut frozen_rounds: u32 = 0;
    loop {
        if let Some(result) = check() {
            return result;
        }
        let now = now_ns(&link.clock);
        if now == last_now {
            frozen_rounds += 1;
        } else {
            last_now = now;
            frozen_rounds = 0;
        }
        let expired =
            now.saturating_sub(start) >= deadline_ns || frozen_rounds >= FROZEN_CLOCK_ROUNDS;
        if expired {
            // Wire truth beats the clock (user study 08, finding F3): before declaring
            // a timeout, pump once more and re-check, so frames that already reached
            // the device -- an RST, a FIN, the very reply we were waiting for -- decide
            // the outcome rather than the deadline. A connect whose SYN was answered
            // with an RST must report `connection-refused`, never `timed-out`, no
            // matter how late the answer is processed.
            pump(link).await?;
            if let Some(result) = check() {
                return result;
            }
            return Err(L4Error::TimedOut);
        }
        let progressed = pump(link).await?;
        if progressed {
            continue;
        }
        // The pump's poll can advance socket state without moving a frame (a
        // retransmit-exhausted connect falling to Closed, an RTO firing): answer
        // before parking, or the park would sit on an already-decided outcome.
        if let Some(result) = check() {
            return result;
        }
        // Nothing moved and nothing is decided: park until traffic (the rx event),
        // the stack's next timed obligation, or the operation's own window — the
        // earliest of the three. A zero bound means stack work is due right now, so
        // loop straight into the next pump instead.
        let now = now_ns(&link.clock);
        let remaining = deadline_ns.saturating_sub(now.saturating_sub(start));
        let stack_due_ns = with_net(|n| {
            let timestamp = smol_instant(now);
            n.iface
                .poll_delay(timestamp, &n.sockets)
                .map(|delay| delay.total_micros().saturating_mul(1_000))
        });
        let bound = stack_due_ns.map_or(remaining, |due| due.min(remaining));
        if bound > 0 {
            match l2::wait_recv(&link.iface, bound).await {
                // Woken by the rx event, by a bound, or immediately by a provider
                // with no event source: re-poll either way (the wait is advisory).
                Ok(()) => {}
                Err(l2::L2Error::Denied) => return Err(L4Error::Denied),
                // Any other wait failure degrades to polling; the deadline above
                // keeps the loop bounded.
                Err(_) => {}
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Address helpers.
// ------------------------------------------------------------------------------------------

fn copy_ip(ip: &IpAddress) -> IpAddress {
    match ip {
        IpAddress::V4(octets) => IpAddress::V4(*octets),
        IpAddress::V6(groups) => IpAddress::V6(*groups),
    }
}

fn copy_addr(addr: &SocketAddress) -> SocketAddress {
    SocketAddress {
        address: copy_ip(&addr.address),
        port: addr.port,
    }
}

/// A destination address in smoltcp's vocabulary; only IPv4 is built in.
fn destination_v4(address: &IpAddress) -> Result<Ipv4Address, L4Error> {
    match address {
        IpAddress::V4((a, b, c, d)) => Ok(Ipv4Address::new(*a, *b, *c, *d)),
        IpAddress::V6(_) => Err(L4Error::Unreachable),
    }
}

/// The interface's currently-bound IPv4 address and prefix: the static values once the
/// stack exists (and, before it does, straight from the configuration), the lease in
/// DHCP mode once acquired — `None` only in DHCP mode before (or between) leases.
fn bound_address() -> Option<(Ipv4Address, u8)> {
    if NET.is_set() {
        with_net(|n| n.bound)
    } else {
        static_addressing().map(|a| (a.address, a.prefix_len))
    }
}

/// Is this an acceptable local bind address (unspecified or the bound address)? In
/// DHCP mode before a lease is bound only the unspecified address qualifies — a caller
/// cannot know the leased address in advance, and the acquisition gate runs before any
/// bind takes effect anyway.
fn bindable(address: &IpAddress) -> bool {
    match address {
        IpAddress::V4(octets) => {
            *octets == (0, 0, 0, 0)
                || bound_address().is_some_and(|(bound, _)| {
                    Ipv4Address::new(octets.0, octets.1, octets.2, octets.3) == bound
                })
        }
        IpAddress::V6(groups) => *groups == (0, 0, 0, 0, 0, 0, 0, 0),
    }
}

/// `bound` (the interface's address, if any) with `port`, in the WIT vocabulary. Takes
/// the bound pair as a value so callers already inside the [`NET`] borrow can pass
/// `n.bound` directly — [`bound_address`] re-enters the state and must not be called
/// under it. (The unbound-DHCP fallback is the unspecified address; it is unreachable
/// in practice — every caller runs behind the acquisition gate.)
fn local_address_with(bound: Option<(Ipv4Address, u8)>, port: u16) -> SocketAddress {
    let octets = bound.map_or([0, 0, 0, 0], |(address, _)| address.octets());
    SocketAddress {
        address: IpAddress::V4((octets[0], octets[1], octets[2], octets[3])),
        port,
    }
}

/// Our own address with `port`, in the WIT vocabulary. Only for callers *outside* the
/// [`NET`] borrow.
fn local_address(port: u16) -> SocketAddress {
    local_address_with(bound_address(), port)
}

/// A smoltcp endpoint rendered back into the WIT vocabulary.
#[allow(unreachable_patterns)]
fn wit_endpoint(endpoint: IpEndpoint) -> SocketAddress {
    let address = match endpoint.addr {
        SmolIpAddress::Ipv4(v4) => {
            let o = v4.octets();
            IpAddress::V4((o[0], o[1], o[2], o[3]))
        }
        _ => IpAddress::V4((0, 0, 0, 0)),
    };
    SocketAddress {
        address,
        port: endpoint.port,
    }
}

fn new_tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER_BYTES]),
        tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER_BYTES]),
    )
}

fn new_udp_socket() -> udp::Socket<'static> {
    udp::Socket::new(
        udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_RX_PACKETS],
            vec![0u8; UDP_RX_PACKETS * UDP_PACKET_BYTES],
        ),
        udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_TX_PACKETS],
            vec![0u8; UDP_TX_PACKETS * UDP_PACKET_BYTES],
        ),
    )
}

// ------------------------------------------------------------------------------------------
// Resource representations.
// ------------------------------------------------------------------------------------------

/// The `net.l4.over-l2` provider.
struct Stub;

/// The root-handle resource: a token — the stack lives in [`NET`], the link in [`LINK`].
struct Root;

/// An established TCP connection: one smoltcp socket plus the peer it reached.
struct Conn {
    handle: SocketHandle,
    peer: SocketAddress,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if NET.is_set() {
            with_net(|n| {
                n.sockets.get_mut::<tcp::Socket>(self.handle).close();
                n.closing.push(self.handle);
                n.live = n.live.saturating_sub(1);
            });
        }
    }
}

/// A listening TCP socket. Accepting swaps the underlying smoltcp socket (the accepted
/// one becomes the connection, a fresh one keeps listening), hence the `Cell`.
struct Listener {
    handle: Cell<SocketHandle>,
    local: SocketAddress,
}

impl Drop for Listener {
    fn drop(&mut self) {
        if NET.is_set() {
            with_net(|n| {
                let handle = self.handle.get();
                n.sockets.get_mut::<tcp::Socket>(handle).abort();
                n.sockets.remove(handle);
                n.live = n.live.saturating_sub(1);
                n.listening_ports.retain(|p| *p != self.local.port);
            });
        }
    }
}

/// A bound UDP socket.
struct Udp {
    handle: SocketHandle,
    local: SocketAddress,
}

impl Drop for Udp {
    fn drop(&mut self) {
        if NET.is_set() {
            with_net(|n| {
                n.sockets.get_mut::<udp::Socket>(self.handle).close();
                n.sockets.remove(self.handle);
                n.live = n.live.saturating_sub(1);
            });
        }
    }
}

impl l4::GuestL4Impl for Root {}
impl l4::GuestTcpConnection for Conn {}
impl l4::GuestTcpListener for Listener {}
impl l4::GuestUdpSocket for Udp {}

// ------------------------------------------------------------------------------------------
// The configure entry (`eo9:net/l4-over-l2-config`).
// ------------------------------------------------------------------------------------------

impl l4_over_l2_config::Guest for Stub {
    /// Bind the IPv4 addressing the stack uses on its link: a static dotted quad (with
    /// a required gateway; the prefix length defaults to 24), or `dhcp` to acquire all
    /// of it from the network on first use. Validation happens here, at compose time: a
    /// malformed address, a static address without a gateway, or `dhcp` combined with
    /// the static-only arguments is a configure error, never a trap (and never a silent
    /// ignore — option C, plan/09 D14).
    fn configure(
        address: String,
        prefix_length: Option<u8>,
        gateway: Option<String>,
    ) -> Result<l4::L4Impl, String> {
        if address == "dhcp" {
            if prefix_length.is_some() || gateway.is_some() {
                return Err(String::from(
                    "--address dhcp acquires the prefix length and gateway from the \
                     lease; do not combine it with --prefix-length or --gateway",
                ));
            }
            MODE.set(AddressMode::Dhcp);
            return Ok(l4::L4Impl::new(Root));
        }
        let address = parse_ipv4(&address)
            .map_err(|err| format!("{err} (or `dhcp` to acquire addressing from the network)"))?;
        let Some(gateway) = gateway else {
            return Err(String::from(
                "static addressing needs --gateway (or use `--address dhcp` to acquire \
                 everything from the network)",
            ));
        };
        let gateway = parse_ipv4(&gateway)?;
        let prefix_length = prefix_length.unwrap_or(PREFIX_LEN);
        if prefix_length > 32 {
            return Err(format!("prefix-length must be 0..=32, not {prefix_length}"));
        }
        MODE.set(AddressMode::Static(Addressing {
            address,
            prefix_len: prefix_length,
            gateway,
        }));
        Ok(l4::L4Impl::new(Root))
    }
}

// ------------------------------------------------------------------------------------------
// The exported l4 surface.
// ------------------------------------------------------------------------------------------

/// The blessed factory (shared-resources design §5.2, native per owner ruling): the
/// kernel call gate mints one handler per consumer wiring through this export. A
/// fresh full-access root onto the one smoltcp stack (the degenerate sharing the
/// design's v1 prescribes); per-grantee attenuation is a policy provider composed on
/// top (`… $ net.l4.filtered`), whose own factory then serves.
impl l4_factory::Guest for Stub {
    fn get() -> Result<l4_factory::L4Impl, l4_factory::L4Error> {
        Ok(l4::L4Impl::new(Root))
    }
}

impl l4::Guest for Stub {
    type L4Impl = Root;
    type TcpConnection = Conn;
    type TcpListener = Listener;
    type UdpSocket = Udp;

    fn default() -> l4::L4Impl {
        l4::L4Impl::new(Root)
    }

    async fn connect(
        _l4: l4::L4ImplBorrow<'_>,
        remote: SocketAddress,
    ) -> Result<l4::TcpConnection, L4Error> {
        let destination = destination_v4(&remote.address)?;
        let link = acquire().await?;

        let handle = with_net(|n| -> Result<SocketHandle, L4Error> {
            n.sweep();
            if n.live >= MAX_SOCKETS {
                return Err(table_full());
            }
            let mut socket = new_tcp_socket();
            let local_port = n.ephemeral_port();
            let endpoint = IpEndpoint::new(SmolIpAddress::Ipv4(destination), remote.port);
            socket
                .connect(n.iface.context(), endpoint, local_port)
                .map_err(|err| L4Error::Io(format!("connect: {err:?}")))?;
            let handle = n.sockets.add(socket);
            n.live += 1;
            Ok(handle)
        })?;

        let outcome = wait_until(&link, CONNECT_DEADLINE_NS, || {
            with_net(|n| match n.sockets.get::<tcp::Socket>(handle).state() {
                tcp::State::Established => Some(Ok(())),
                // The remote answered with a reset (or the stack gave up): the socket
                // falls back to Closed without ever having been established.
                tcp::State::Closed => Some(Err(L4Error::ConnectionRefused)),
                _ => None,
            })
        })
        .await;

        match outcome {
            Ok(()) => Ok(l4::TcpConnection::new(Conn {
                handle,
                peer: copy_addr(&remote),
            })),
            Err(err) => {
                with_net(|n| {
                    n.sockets.get_mut::<tcp::Socket>(handle).abort();
                    n.sockets.remove(handle);
                    n.live = n.live.saturating_sub(1);
                });
                Err(err)
            }
        }
    }

    async fn listen(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::TcpListener, L4Error> {
        if !bindable(&local.address) {
            return Err(L4Error::AddressUnavailable);
        }
        let _link = acquire().await?;
        with_net(|n| {
            n.sweep();
            if n.live >= MAX_SOCKETS {
                return Err(table_full());
            }
            let port = if local.port == 0 {
                n.ephemeral_port()
            } else {
                local.port
            };
            // A port is taken for as long as a listener resource holds it, whatever TCP
            // state its current underlying socket is in (Listen, mid-handshake, or
            // established-and-awaiting-accept).
            if n.listening_ports.contains(&port) {
                return Err(L4Error::AddressInUse);
            }
            let mut socket = new_tcp_socket();
            socket
                .listen(port)
                .map_err(|err| L4Error::Io(format!("listen: {err:?}")))?;
            let handle = n.sockets.add(socket);
            n.live += 1;
            n.listening_ports.push(port);
            Ok(l4::TcpListener::new(Listener {
                handle: Cell::new(handle),
                local: local_address_with(n.bound, port),
            }))
        })
    }

    async fn accept(
        l: l4::TcpListenerBorrow<'_>,
    ) -> Result<(l4::TcpConnection, SocketAddress), L4Error> {
        let listener = l.get::<Listener>();
        let link = acquire().await?;

        let peer_endpoint = wait_until(&link, RECV_DEADLINE_NS, || {
            with_net(|n| {
                let socket = n.sockets.get::<tcp::Socket>(listener.handle.get());
                match socket.state() {
                    tcp::State::Established => Some(Ok(socket.remote_endpoint())),
                    tcp::State::Closed => Some(Err(L4Error::ConnectionReset)),
                    _ => None,
                }
            })
        })
        .await?;

        // The socket that just went Established becomes the connection; a fresh socket
        // takes over listening on the same port. After the swap there are two live
        // resources where there was one (the connection plus the still-listening
        // listener), so the live-socket count grows with the replacement — without this
        // the MAX_SOCKETS bound under-counts by one per accept.
        let connection_handle = listener.handle.get();
        let port = listener.local.port;
        let replacement = with_net(|n| -> Result<SocketHandle, L4Error> {
            let mut socket = new_tcp_socket();
            socket
                .listen(port)
                .map_err(|err| L4Error::Io(format!("listen: {err:?}")))?;
            let handle = n.sockets.add(socket);
            n.live += 1;
            Ok(handle)
        })?;
        listener.handle.set(replacement);

        let peer = peer_endpoint.map_or_else(|| local_address(0), wit_endpoint);
        Ok((
            l4::TcpConnection::new(Conn {
                handle: connection_handle,
                peer: copy_addr(&peer),
            }),
            peer,
        ))
    }

    fn listener_address(l: l4::TcpListenerBorrow<'_>) -> SocketAddress {
        copy_addr(&l.get::<Listener>().local)
    }

    fn peer_address(c: l4::TcpConnectionBorrow<'_>) -> SocketAddress {
        copy_addr(&c.get::<Conn>().peer)
    }

    async fn send(
        c: l4::TcpConnectionBorrow<'_>,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let connection = c.get::<Conn>();
        let bytes = src.read(0, src.len());
        let link = match acquire().await {
            Ok(link) => link,
            Err(err) => return (src, Err(err)),
        };

        let mut queued = 0usize;
        let outcome = wait_until(&link, SEND_FLUSH_DEADLINE_NS, || {
            with_net(|n| {
                let socket = n.sockets.get_mut::<tcp::Socket>(connection.handle);
                if !socket.may_send() {
                    return if queued > 0 {
                        Some(Ok(()))
                    } else {
                        Some(Err(L4Error::ConnectionReset))
                    };
                }
                match socket.send_slice(&bytes[queued..]) {
                    Ok(count) => {
                        queued += count;
                        if queued == bytes.len() {
                            Some(Ok(()))
                        } else {
                            None
                        }
                    }
                    Err(err) => Some(Err(L4Error::Io(format!("send: {err:?}")))),
                }
            })
        })
        .await;
        // Give what was queued a chance to leave the stack.
        if !matches!(outcome, Err(L4Error::Denied)) {
            let _ = pump(&link).await;
        }

        match outcome {
            Ok(()) => (
                src,
                Ok(SendResult {
                    bytes_sent: queued as u64,
                }),
            ),
            Err(L4Error::TimedOut) if queued > 0 => (
                src,
                Ok(SendResult {
                    bytes_sent: queued as u64,
                }),
            ),
            Err(err) => (src, Err(err)),
        }
    }

    async fn recv(
        c: l4::TcpConnectionBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L4Error>) {
        let connection = c.get::<Conn>();
        let capacity = dst.len();
        let link = match acquire().await {
            Ok(link) => link,
            Err(err) => return (dst, Err(err)),
        };

        let outcome = wait_until(&link, RECV_DEADLINE_NS, || {
            with_net(|n| {
                let socket = n.sockets.get_mut::<tcp::Socket>(connection.handle);
                if socket.can_recv() {
                    let mut chunk = vec![0u8; capacity.min(TCP_BUFFER_BYTES as u64) as usize];
                    return match socket.recv_slice(&mut chunk) {
                        Ok(count) => {
                            chunk.truncate(count);
                            Some(Ok(chunk))
                        }
                        Err(err) => Some(Err(L4Error::Io(format!("recv: {err:?}")))),
                    };
                }
                if !socket.may_recv() {
                    // Peer closed and everything queued has been drained: end of stream.
                    return Some(Ok(Vec::new()));
                }
                None
            })
        })
        .await;

        match outcome {
            Ok(chunk) => {
                if !chunk.is_empty() {
                    dst.write(0, &chunk);
                }
                (
                    dst,
                    Ok(RecvResult {
                        bytes_received: chunk.len() as u64,
                    }),
                )
            }
            Err(err) => (dst, Err(err)),
        }
    }

    async fn bind_udp(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::UdpSocket, L4Error> {
        if !bindable(&local.address) {
            return Err(L4Error::AddressUnavailable);
        }
        let _link = acquire().await?;
        with_net(|n| {
            n.sweep();
            if n.live >= MAX_SOCKETS {
                return Err(table_full());
            }
            let port = if local.port == 0 {
                n.ephemeral_port()
            } else {
                local.port
            };
            for (_handle, socket) in n.sockets.iter() {
                if let smoltcp::socket::Socket::Udp(udp_socket) = socket
                    && udp_socket.endpoint().port == port
                {
                    return Err(L4Error::AddressInUse);
                }
            }
            let mut socket = new_udp_socket();
            socket
                .bind(port)
                .map_err(|err| L4Error::Io(format!("bind: {err:?}")))?;
            let handle = n.sockets.add(socket);
            n.live += 1;
            Ok(l4::UdpSocket::new(Udp {
                handle,
                local: local_address_with(n.bound, port),
            }))
        })
    }

    fn udp_address(s: l4::UdpSocketBorrow<'_>) -> SocketAddress {
        copy_addr(&s.get::<Udp>().local)
    }

    async fn send_to(
        s: l4::UdpSocketBorrow<'_>,
        remote: SocketAddress,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let socket_state = s.get::<Udp>();
        let destination = match destination_v4(&remote.address) {
            Ok(v4) => v4,
            Err(err) => return (src, Err(err)),
        };
        let payload = src.read(0, src.len());
        if payload.len() > UDP_PACKET_BYTES {
            return (src, Err(L4Error::MessageTooLarge));
        }
        let link = match acquire().await {
            Ok(link) => link,
            Err(err) => return (src, Err(err)),
        };

        let queue_outcome = with_net(|n| {
            let socket = n.sockets.get_mut::<udp::Socket>(socket_state.handle);
            let endpoint = IpEndpoint::new(SmolIpAddress::Ipv4(destination), remote.port);
            socket
                .send_slice(&payload, endpoint)
                .map_err(|err| match err {
                    udp::SendError::BufferFull => L4Error::Io(String::from("udp send queue full")),
                    udp::SendError::Unaddressable => L4Error::Unreachable,
                })
        });
        if let Err(err) = queue_outcome {
            return (src, Err(err));
        }

        // A few pump rounds give the datagram (and the ARP exchange it may need) a
        // chance to leave; a recv that follows keeps pumping anyway.
        for _ in 0..6 {
            if let Err(L4Error::Denied) = pump(&link).await {
                return (src, Err(L4Error::Denied));
            }
            let drained = with_net(|n| {
                n.dev.tx.is_empty()
                    && n.sockets
                        .get::<udp::Socket>(socket_state.handle)
                        .send_queue()
                        == 0
            });
            if drained {
                break;
            }
        }

        (
            src,
            Ok(SendResult {
                bytes_sent: payload.len() as u64,
            }),
        )
    }

    async fn recv_from(
        s: l4::UdpSocketBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, SocketAddress), L4Error>) {
        let socket_state = s.get::<Udp>();
        let capacity = dst.len();
        let link = match acquire().await {
            Ok(link) => link,
            Err(err) => return (dst, Err(err)),
        };

        let outcome = wait_until(&link, RECV_DEADLINE_NS, || {
            with_net(|n| {
                let socket = n.sockets.get_mut::<udp::Socket>(socket_state.handle);
                if !socket.can_recv() {
                    return None;
                }
                match socket.recv() {
                    Ok((payload, metadata)) => {
                        Some(Ok((payload.to_vec(), wit_endpoint(metadata.endpoint))))
                    }
                    Err(udp::RecvError::Exhausted) => None,
                    Err(err) => Some(Err(L4Error::Io(format!("recv-from: {err:?}")))),
                }
            })
        })
        .await;

        match outcome {
            Ok((payload, from)) => {
                let take = payload.len().min(capacity as usize);
                if take > 0 {
                    dst.write(0, &payload[..take]);
                }
                (
                    dst,
                    Ok((
                        RecvResult {
                            bytes_received: take as u64,
                        },
                        from,
                    )),
                )
            }
            Err(err) => (dst, Err(err)),
        }
    }

    /// What the stack's own addressing taught it about DNS. Static modes answer from
    /// the configuration alone (no link bring-up: the answer cannot change); DHCP mode
    /// runs behind the same acquisition gate as every transport operation, so the
    /// reported servers are the bound lease's — or the wait's typed error, never a
    /// guess.
    async fn dns_servers(_l4: l4::L4ImplBorrow<'_>) -> Result<Vec<IpAddress>, L4Error> {
        match mode() {
            AddressMode::Static(_) => {
                if MODE.is_set() {
                    // Explicitly configured static addressing: the operator chose the
                    // addressing and named no DNS (the config deliberately has no DNS
                    // knob) — honestly report none rather than invent one.
                    Ok(Vec::new())
                } else {
                    // The unconfigured documented default is QEMU user-net's layout,
                    // and that layout comes as a set: address, gateway, forwarder.
                    let o = USER_NET_DNS.octets();
                    Ok(vec![IpAddress::V4((o[0], o[1], o[2], o[3]))])
                }
            }
            AddressMode::Dhcp => {
                let _link = acquire().await?;
                Ok(with_net(|n| {
                    n.dns
                        .iter()
                        .map(|server| {
                            let o = server.octets();
                            IpAddress::V4((o[0], o[1], o[2], o[3]))
                        })
                        .collect()
                }))
            }
        }
    }
}

export!(Stub);
