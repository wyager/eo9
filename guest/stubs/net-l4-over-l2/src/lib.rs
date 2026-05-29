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
//!   composition works and never traps (plan/09 Decision 14). Address overrides need an
//!   `l4-over-l2-config` interface in `wit/net`, recorded as a follow-up; until then
//!   the default is the only configuration.
//! * **Driving the link.** Every l2 import is driven eagerly (the same single-poll
//!   pattern as `net.virtio` driving `eo9:pci`, and `fs.eofs` driving `eo9:disk`):
//!   each exported l4 operation pumps the link — transmit what the stack queued,
//!   receive what the device has, let smoltcp process it — until the operation
//!   completes or its deadline passes, then returns a typed error. Nothing here ever
//!   suspends mid-operation and nothing blocks forever.
//! * **Bounds.** At most 16 sockets (TCP + UDP combined), 16 KiB TCP buffers per
//!   direction, 8 × 1536 B received / 4 × 1536 B queued UDP datagrams per socket, a
//!   32-frame receive queue, and per-operation deadlines (4 s receive, 6 s connect,
//!   1.5 s send-flush) backed by a hard cap on pump rounds so even a frozen test clock
//!   cannot loop forever.
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
use core::pin::pin;
use core::task::{Context as TaskContext, Poll, Waker};

use eo9_guest::provider::ProviderState;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress as SmolIpAddress, IpCidr, IpEndpoint, Ipv4Address,
};

wit_bindgen::generate!({
    world: "l4-over-l2",
    path: "wit",
    // Pull in bindings for eo9:io/buffers and eo9:time/types, which the imported and
    // exported interfaces use but the world does not name directly.
    generate_all,
});

use eo9::entropy::entropy;
use eo9::net::l2;
use eo9::time::time;
use exports::eo9::net::l4::{
    self, Buffer, IpAddress, L4Error, RecvResult, SendResult, SocketAddress,
};
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

/// Set exactly once, by `configure`; absent for an unconfigured provider.
static ADDRESSING: ProviderState<Addressing> = ProviderState::new();

/// The addressing in force: configured values when `configure` ran, the defaults otherwise.
fn addressing() -> Addressing {
    if ADDRESSING.is_set() {
        ADDRESSING.with(|a| *a)
    } else {
        Addressing::defaults()
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
/// Hard cap on pump rounds per operation, so a clock that never advances (a frozen test
/// stub) still cannot make an operation loop forever.
const MAX_PUMPS: u32 = 4096;

// ------------------------------------------------------------------------------------------
// Eager driving of the async l2 imports (same pattern and reasoning as net.virtio's pci
// imports and fs.eofs's disk imports).
// ------------------------------------------------------------------------------------------

/// Drive an async import call that completes without suspending. Every l2 operation the
/// providers below us export completes in a single poll (that is the convention the
/// drivers and stubs follow); one that genuinely suspends makes the operation fail with
/// a typed `io` error rather than blocking the consumer's eager poll of *us*.
fn eager<F: Future>(what: &str, future: F) -> Result<F::Output, L4Error> {
    let mut future = pin!(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => Ok(value),
        Poll::Pending => Err(L4Error::Io(format!("{what}: the l2 provider suspended"))),
    }
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

struct LinkSlot(Option<Link>);

static LINK: ProviderState<LinkSlot> = ProviderState::new();

/// Puts the link back in its slot when the operation that took it finishes.
struct LinkGuard(Option<Link>);

impl Drop for LinkGuard {
    fn drop(&mut self) {
        if let Some(link) = self.0.take() {
            LINK.with(|slot| slot.0 = Some(link));
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

/// Take the link for one operation, bringing it (and the smoltcp state) up on first
/// use: open the l2 provider's first interface, read its MAC address, seed the stack
/// from entropy, and bind the documented default address.
fn acquire() -> Result<LinkGuard, L4Error> {
    if !LINK.is_set() {
        LINK.set(LinkSlot(None));
    }
    if let Some(link) = LINK.with(|slot| slot.0.take()) {
        return Ok(LinkGuard(Some(link)));
    }

    let root = l2::default();
    let interfaces = eager("list-interfaces", l2::list_interfaces(&root))?.map_err(l2_failure)?;
    let first = interfaces
        .first()
        .ok_or_else(|| L4Error::Io(String::from("the l2 capability exposes no interfaces")))?;
    let (a, b, c, d, e, f) = first.mac;
    let mac = [a, b, c, d, e, f];
    let mtu = first.mtu.clamp(576, 9216) as usize;
    let iface = eager(
        "open-interface",
        l2::open_interface(&root, first.name.clone()),
    )?
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
        let bound = addressing();
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(
                SmolIpAddress::Ipv4(bound.address),
                bound.prefix_len,
            ));
        });
        let _ = iface.routes_mut().add_default_ipv4_route(bound.gateway);
        NetState {
            iface,
            sockets: SocketSet::new(Vec::new()),
            dev,
            ephemeral: 49152u16.wrapping_add((seed % 16000) as u16),
            closing: Vec::new(),
            live: 0,
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

/// Advance the stack against the current frame queues.
fn poll_stack(link: &Link) {
    let timestamp = smol_instant(now_ns(&link.clock));
    with_net(|n| {
        let _ = n.iface.poll(timestamp, &mut n.dev, &mut n.sockets);
    });
}

/// Hand everything the stack queued to the l2 provider.
fn flush_tx(link: &Link) -> Result<(), L4Error> {
    let frames: Vec<Vec<u8>> = with_net(|n| n.dev.tx.drain(..).collect());
    for frame in frames {
        let buffer = Buffer::new(frame.len() as u64);
        buffer.write(0, &frame);
        let (_buffer, sent) = eager("send-frame", l2::send_frame(&link.iface, buffer))?;
        match sent {
            Ok(_) => {}
            Err(l2::L2Error::Denied) => return Err(L4Error::Denied),
            // Any other send problem: drop the frame. TCP retransmits, and the
            // operation deadline reports persistent trouble.
            Err(_) => {}
        }
    }
    Ok(())
}

/// One pump round: let the stack emit what is due, hand it to the link, pull a few
/// frames the other way, and let the stack process them.
fn pump(link: &Link) -> Result<(), L4Error> {
    poll_stack(link);
    flush_tx(link)?;

    let mut received_any = false;
    for _ in 0..RX_BATCH {
        let dst = Buffer::new(RX_BUFFER_BYTES);
        let (dst, received) = eager("recv-frame", l2::recv_frame(&link.iface, dst))?;
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
            Ok(_) => break,
            Err(l2::L2Error::Denied) => return Err(L4Error::Denied),
            // "Nothing waiting" and transient receive trouble look the same from here;
            // the operation deadline decides whether it matters.
            Err(_) => break,
        }
    }
    if received_any {
        poll_stack(link);
        flush_tx(link)?;
    }
    Ok(())
}

/// Pump the link until `check` reports a result, the deadline passes, or the pump-round
/// cap is hit. `check` runs before the first pump, so already-satisfiable operations
/// never touch the link.
fn wait_until<T>(
    link: &Link,
    deadline_ns: u64,
    mut check: impl FnMut() -> Option<Result<T, L4Error>>,
) -> Result<T, L4Error> {
    let start = now_ns(&link.clock);
    let mut rounds: u32 = 0;
    loop {
        if let Some(result) = check() {
            return result;
        }
        if rounds >= MAX_PUMPS {
            return Err(L4Error::TimedOut);
        }
        if now_ns(&link.clock).saturating_sub(start) >= deadline_ns {
            return Err(L4Error::TimedOut);
        }
        pump(link)?;
        rounds += 1;
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

/// Is this an acceptable local bind address (unspecified or the bound address)?
fn bindable(address: &IpAddress) -> bool {
    match address {
        IpAddress::V4(octets) => {
            *octets == (0, 0, 0, 0)
                || Ipv4Address::new(octets.0, octets.1, octets.2, octets.3)
                    == addressing().address
        }
        IpAddress::V6(groups) => *groups == (0, 0, 0, 0, 0, 0, 0, 0),
    }
}

/// Our own address with `port`, in the WIT vocabulary.
fn local_address(port: u16) -> SocketAddress {
    let octets = addressing().address.octets();
    SocketAddress {
        address: IpAddress::V4((octets[0], octets[1], octets[2], octets[3])),
        port,
    }
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
    /// Bind the static IPv4 addressing the stack uses on its link. Validation happens
    /// here, at compose time: a malformed address is a configure error, never a trap.
    fn configure(address: String, prefix_length: u8, gateway: String) -> Result<l4::L4Impl, String> {
        let address = parse_ipv4(&address)?;
        let gateway = parse_ipv4(&gateway)?;
        if prefix_length > 32 {
            return Err(format!(
                "prefix-length must be 0..=32, not {prefix_length}"
            ));
        }
        ADDRESSING.set(Addressing {
            address,
            prefix_len: prefix_length,
            gateway,
        });
        Ok(l4::L4Impl::new(Root))
    }
}

// ------------------------------------------------------------------------------------------
// The exported l4 surface.
// ------------------------------------------------------------------------------------------

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
        let link = acquire()?;

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
        });

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
        let _link = acquire()?;
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
                if let smoltcp::socket::Socket::Tcp(tcp_socket) = socket
                    && tcp_socket.state() == tcp::State::Listen
                    && tcp_socket
                        .local_endpoint()
                        .is_some_and(|ep| ep.port == port)
                {
                    return Err(L4Error::AddressInUse);
                }
            }
            let mut socket = new_tcp_socket();
            socket
                .listen(port)
                .map_err(|err| L4Error::Io(format!("listen: {err:?}")))?;
            let handle = n.sockets.add(socket);
            n.live += 1;
            Ok(l4::TcpListener::new(Listener {
                handle: Cell::new(handle),
                local: local_address(port),
            }))
        })
    }

    async fn accept(
        l: l4::TcpListenerBorrow<'_>,
    ) -> Result<(l4::TcpConnection, SocketAddress), L4Error> {
        let listener = l.get::<Listener>();
        let link = acquire()?;

        let peer_endpoint = wait_until(&link, RECV_DEADLINE_NS, || {
            with_net(|n| {
                let socket = n.sockets.get::<tcp::Socket>(listener.handle.get());
                match socket.state() {
                    tcp::State::Established => Some(Ok(socket.remote_endpoint())),
                    tcp::State::Closed => Some(Err(L4Error::ConnectionReset)),
                    _ => None,
                }
            })
        })?;

        // The socket that just went Established becomes the connection; a fresh socket
        // takes over listening on the same port.
        let connection_handle = listener.handle.get();
        let port = listener.local.port;
        let replacement = with_net(|n| -> Result<SocketHandle, L4Error> {
            let mut socket = new_tcp_socket();
            socket
                .listen(port)
                .map_err(|err| L4Error::Io(format!("listen: {err:?}")))?;
            Ok(n.sockets.add(socket))
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
        let link = match acquire() {
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
        });
        // Give what was queued a chance to leave the stack.
        if !matches!(outcome, Err(L4Error::Denied)) {
            let _ = pump(&link);
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
        let link = match acquire() {
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
        });

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
        let _link = acquire()?;
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
                local: local_address(port),
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
        let link = match acquire() {
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
            if let Err(L4Error::Denied) = pump(&link) {
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
        let link = match acquire() {
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
        });

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
}

export!(Stub);
