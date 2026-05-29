//! `net.l4.loopback` — a self-contained, in-memory transport layer.
//!
//! Targets the `eo9:net/l4-loopback` stub world: exports `eo9:net/l4` where TCP and UDP
//! work between sockets created through this provider instance, loopback addresses
//! only, and nothing outside the instance is reachable. The standard test/mock L4 — it
//! needs no lower layers at all (which is the point of the layered net split).
//!
//! Semantics (the MVP surface, documented here because the WIT leaves them open):
//!
//! * Destinations must be loopback (`127.0.0.0/8` or `::1`); anything else fails with
//!   `unreachable`. Local bind addresses must be loopback or unspecified; the bound
//!   address is canonicalized to `127.0.0.1` / `::1`. Port 0 binds an ephemeral port.
//! * `connect` requires a listener on the destination port (else `connection-refused`)
//!   and completes immediately: the server end is queued on the listener's backlog, so
//!   a single task can `listen`, `connect`, then `accept` sequentially.
//! * The provider never blocks: `accept` with an empty backlog and `recv`/`recv-from`
//!   with nothing queued (and the peer still open) fail with an `io` error saying so.
//!   `recv` on a connection whose peer end has been dropped reports 0 bytes (EOF) once
//!   the queued data drains; `send` to a dropped peer fails with `connection-reset`.
//! * `recv-from` truncates a datagram to the destination buffer's length, like UDP.
//!
//! The documented default state is the empty loopback network — `configure` (which
//! takes no arguments) creates exactly that, and an unconfigured provider
//! self-initializes to it on first use, so plain `net.l4.loopback $ program` works and
//! never traps (plan/09 Decision 14).

#![no_std]

extern crate alloc;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "l4-loopback",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the exported l4 interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::net::l4::{
    self, Buffer, IpAddress, L4Error, RecvResult, SendResult, SocketAddress,
};
use exports::eo9::net::l4_loopback_config;

/// The `net.l4.loopback` provider.
struct Stub;

/// The root-handle resource: a token — the network state lives in [`STATE`].
struct LoopbackRoot;

/// One direction of a TCP pair: bytes one end has sent and the other has not yet
/// received, plus whether either endpoint has been dropped.
struct Stream {
    queue: VecDeque<u8>,
    /// One of the two endpoints has been dropped: reads see EOF after the queue
    /// drains, writes fail with `connection-reset`.
    closed: bool,
}

impl Stream {
    fn new() -> Rc<RefCell<Stream>> {
        Rc::new(RefCell::new(Stream {
            queue: VecDeque::new(),
            closed: false,
        }))
    }
}

/// The representation behind a `tcp-connection` handle: one end of an in-memory pair.
struct ConnectionState {
    /// The remote endpoint of this connection.
    peer: SocketAddress,
    /// Bytes the peer has sent and this end has not yet received.
    rx: Rc<RefCell<Stream>>,
    /// The peer's inbound stream — where this end's sends go.
    tx: Rc<RefCell<Stream>>,
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        // The peer sees EOF after draining what was already sent, and its further
        // sends fail with `connection-reset`.
        self.rx.borrow_mut().closed = true;
        self.tx.borrow_mut().closed = true;
    }
}

/// A connection sitting in a listener's backlog, waiting to be accepted.
struct Pending {
    connection: ConnectionState,
    peer: SocketAddress,
}

/// The representation behind a `tcp-listener` handle.
struct ListenerState {
    local: SocketAddress,
    backlog: Rc<RefCell<VecDeque<Pending>>>,
}

impl Drop for ListenerState {
    fn drop(&mut self) {
        with_state(|net| {
            net.listeners.remove(&self.local.port);
        });
    }
}

/// One queued UDP datagram.
struct Datagram {
    from: SocketAddress,
    payload: Vec<u8>,
}

/// The representation behind a `udp-socket` handle.
struct UdpState {
    local: SocketAddress,
    queue: Rc<RefCell<VecDeque<Datagram>>>,
}

impl Drop for UdpState {
    fn drop(&mut self) {
        with_state(|net| {
            net.udp.remove(&self.local.port);
        });
    }
}

/// The whole in-memory network: which ports are bound, by whom.
struct Loopback {
    /// TCP: bound listening port → backlog of connections waiting to be accepted.
    listeners: BTreeMap<u16, Rc<RefCell<VecDeque<Pending>>>>,
    /// UDP: bound port → queue of datagrams waiting to be received.
    udp: BTreeMap<u16, Rc<RefCell<VecDeque<Datagram>>>>,
    /// Next ephemeral port to hand out (port-0 binds and client connect ports).
    next_ephemeral: u16,
}

impl Loopback {
    fn empty() -> Loopback {
        Loopback {
            listeners: BTreeMap::new(),
            udp: BTreeMap::new(),
            next_ephemeral: 49152,
        }
    }

    /// Hand out the next ephemeral port not currently bound by either protocol.
    fn ephemeral_port(&mut self) -> u16 {
        loop {
            let candidate = self.next_ephemeral;
            self.next_ephemeral = if candidate == u16::MAX {
                49152
            } else {
                candidate + 1
            };
            if !self.listeners.contains_key(&candidate) && !self.udp.contains_key(&candidate) {
                return candidate;
            }
        }
    }
}

static STATE: ProviderState<Loopback> = ProviderState::new();

/// Run `f` against the network state, lazily binding the documented default — the empty
/// loopback network, exactly the state the nullary `configure` creates — so an
/// unconfigured `net.l4.loopback $ program` works and never traps (plan/09 Decision 14).
fn with_state<R>(f: impl FnOnce(&mut Loopback) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(Loopback::empty());
    }
    STATE.with(f)
}

// --- address helpers --------------------------------------------------------------

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

/// Is this a loopback destination this provider can reach?
fn is_loopback(ip: &IpAddress) -> bool {
    match ip {
        IpAddress::V4((first, ..)) => *first == 127,
        IpAddress::V6(groups) => *groups == (0, 0, 0, 0, 0, 0, 0, 1),
    }
}

/// Is this address acceptable as a local bind address (loopback or unspecified)?
fn is_bindable(ip: &IpAddress) -> bool {
    match ip {
        IpAddress::V4(octets) => *octets == (0, 0, 0, 0) || is_loopback(ip),
        IpAddress::V6(groups) => *groups == (0, 0, 0, 0, 0, 0, 0, 0) || is_loopback(ip),
    }
}

/// The canonical loopback address in the same family as `ip`, with `port`.
fn canonical(ip: &IpAddress, port: u16) -> SocketAddress {
    let address = match ip {
        IpAddress::V4(_) => IpAddress::V4((127, 0, 0, 1)),
        IpAddress::V6(_) => IpAddress::V6((0, 0, 0, 0, 0, 0, 0, 1)),
    };
    SocketAddress { address, port }
}

fn buffer_capacity(buffer: &Buffer) -> usize {
    usize::try_from(buffer.len()).unwrap_or(usize::MAX)
}

const NO_DATA: &str =
    "no data queued — the loopback transport does not block; send before receiving";
const NO_PENDING: &str =
    "nothing to accept — the loopback transport does not block; connect before accepting";
const NO_DATAGRAM: &str =
    "no datagram queued — the loopback transport does not block; send before receiving";

// --- resource representations -------------------------------------------------------

impl l4::GuestL4Impl for LoopbackRoot {}
impl l4::GuestTcpConnection for ConnectionState {}
impl l4::GuestTcpListener for ListenerState {}
impl l4::GuestUdpSocket for UdpState {}

impl l4_loopback_config::Guest for Stub {
    fn configure() -> Result<l4::L4Impl, String> {
        STATE.set(Loopback::empty());
        Ok(l4::L4Impl::new(LoopbackRoot))
    }
}

impl l4::Guest for Stub {
    type L4Impl = LoopbackRoot;
    type TcpConnection = ConnectionState;
    type TcpListener = ListenerState;
    type UdpSocket = UdpState;

    fn default() -> l4::L4Impl {
        l4::L4Impl::new(LoopbackRoot)
    }

    async fn connect(
        _l4: l4::L4ImplBorrow<'_>,
        remote: SocketAddress,
    ) -> Result<l4::TcpConnection, L4Error> {
        if !is_loopback(&remote.address) {
            return Err(L4Error::Unreachable);
        }
        with_state(|net| {
            let Some(backlog) = net.listeners.get(&remote.port).map(Rc::clone) else {
                return Err(L4Error::ConnectionRefused);
            };
            let client_addr = canonical(&remote.address, net.ephemeral_port());
            let server_addr = canonical(&remote.address, remote.port);

            // Two directed streams make one connection pair.
            let to_client = Stream::new();
            let to_server = Stream::new();
            let client_end = ConnectionState {
                peer: server_addr,
                rx: Rc::clone(&to_client),
                tx: Rc::clone(&to_server),
            };
            let server_end = ConnectionState {
                peer: copy_addr(&client_addr),
                rx: to_server,
                tx: to_client,
            };
            backlog.borrow_mut().push_back(Pending {
                connection: server_end,
                peer: client_addr,
            });
            Ok(l4::TcpConnection::new(client_end))
        })
    }

    async fn listen(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::TcpListener, L4Error> {
        if !is_bindable(&local.address) {
            return Err(L4Error::AddressUnavailable);
        }
        with_state(|net| {
            let port = if local.port == 0 {
                net.ephemeral_port()
            } else {
                local.port
            };
            if net.listeners.contains_key(&port) {
                return Err(L4Error::AddressInUse);
            }
            let backlog = Rc::new(RefCell::new(VecDeque::new()));
            net.listeners.insert(port, Rc::clone(&backlog));
            Ok(l4::TcpListener::new(ListenerState {
                local: canonical(&local.address, port),
                backlog,
            }))
        })
    }

    async fn accept(
        l: l4::TcpListenerBorrow<'_>,
    ) -> Result<(l4::TcpConnection, SocketAddress), L4Error> {
        let listener = l.get::<ListenerState>();
        let pending = listener.backlog.borrow_mut().pop_front();
        match pending {
            Some(pending) => Ok((l4::TcpConnection::new(pending.connection), pending.peer)),
            None => Err(L4Error::Io(String::from(NO_PENDING))),
        }
    }

    fn listener_address(l: l4::TcpListenerBorrow<'_>) -> SocketAddress {
        copy_addr(&l.get::<ListenerState>().local)
    }

    fn peer_address(c: l4::TcpConnectionBorrow<'_>) -> SocketAddress {
        copy_addr(&c.get::<ConnectionState>().peer)
    }

    async fn send(
        c: l4::TcpConnectionBorrow<'_>,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let connection = c.get::<ConnectionState>();
        let bytes = src.read(0, src.len());
        let mut tx = connection.tx.borrow_mut();
        if tx.closed {
            return (src, Err(L4Error::ConnectionReset));
        }
        tx.queue.extend(bytes.iter().copied());
        (
            src,
            Ok(SendResult {
                bytes_sent: bytes.len() as u64,
            }),
        )
    }

    async fn recv(
        c: l4::TcpConnectionBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L4Error>) {
        let connection = c.get::<ConnectionState>();
        let wanted = buffer_capacity(&dst);
        let mut rx = connection.rx.borrow_mut();
        let count = usize::min(wanted, rx.queue.len());
        if count == 0 {
            return if rx.closed || wanted == 0 {
                // Peer gone and nothing left queued: end of stream.
                (dst, Ok(RecvResult { bytes_received: 0 }))
            } else {
                (dst, Err(L4Error::Io(String::from(NO_DATA))))
            };
        }
        let bytes: Vec<u8> = rx.queue.drain(..count).collect();
        dst.write(0, &bytes);
        (
            dst,
            Ok(RecvResult {
                bytes_received: count as u64,
            }),
        )
    }

    async fn bind_udp(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::UdpSocket, L4Error> {
        if !is_bindable(&local.address) {
            return Err(L4Error::AddressUnavailable);
        }
        with_state(|net| {
            let port = if local.port == 0 {
                net.ephemeral_port()
            } else {
                local.port
            };
            if net.udp.contains_key(&port) {
                return Err(L4Error::AddressInUse);
            }
            let queue = Rc::new(RefCell::new(VecDeque::new()));
            net.udp.insert(port, Rc::clone(&queue));
            Ok(l4::UdpSocket::new(UdpState {
                local: canonical(&local.address, port),
                queue,
            }))
        })
    }

    fn udp_address(s: l4::UdpSocketBorrow<'_>) -> SocketAddress {
        copy_addr(&s.get::<UdpState>().local)
    }

    async fn send_to(
        s: l4::UdpSocketBorrow<'_>,
        remote: SocketAddress,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let socket = s.get::<UdpState>();
        if !is_loopback(&remote.address) {
            return (src, Err(L4Error::Unreachable));
        }
        let destination = with_state(|net| net.udp.get(&remote.port).map(Rc::clone));
        let Some(destination) = destination else {
            // Real UDP surfaces an unreachable port as a refusal.
            return (src, Err(L4Error::ConnectionRefused));
        };
        let payload = src.read(0, src.len());
        let bytes_sent = payload.len() as u64;
        destination.borrow_mut().push_back(Datagram {
            from: copy_addr(&socket.local),
            payload,
        });
        (src, Ok(SendResult { bytes_sent }))
    }

    async fn recv_from(
        s: l4::UdpSocketBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, SocketAddress), L4Error>) {
        let socket = s.get::<UdpState>();
        let datagram = socket.queue.borrow_mut().pop_front();
        let Some(datagram) = datagram else {
            return (dst, Err(L4Error::Io(String::from(NO_DATAGRAM))));
        };
        // Truncate to the destination buffer, as UDP does.
        let count = usize::min(buffer_capacity(&dst), datagram.payload.len());
        dst.write(0, &datagram.payload[..count]);
        (
            dst,
            Ok((
                RecvResult {
                    bytes_received: count as u64,
                },
                datagram.from,
            )),
        )
    }
}

export!(Stub);
