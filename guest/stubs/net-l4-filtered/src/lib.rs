//! `net.l4.filtered` — a connection-policy-attenuated view of an underlying transport layer.
//!
//! Targets the `eo9:net/l4-filtered` stub world: imports `eo9:net/l4` plus an
//! `eo9:net/connection-policy` decision function, and re-exports `l4` with every
//! *endpoint* operation gated by the policy — a firewall as ordinary composed
//! middleware ("policies are programs" — SPEC, Eo9 API design):
//!
//!   net.policy-ports --allow "[80, 443]" $ net.l4.filtered $ program
//!
//! Gated operations (refused endpoints answer the layer's own `denied`):
//!
//! * `connect`  — the remote endpoint is submitted with kind `connect`;
//! * `listen`   — the local endpoint, kind `listen`;
//! * `bind-udp` — the local endpoint, kind `bind-udp`;
//! * `send-to`  — the remote endpoint, kind `send-to` (per datagram, so a socket bound
//!   to an admitted local port still cannot reach refused remotes).
//!
//! Everything on an admitted connection/listener/socket (`accept`, `send`, `recv`,
//! `recv-from`, the address accessors) forwards on resources this provider owns and
//! wraps — a consumer can never reach an underlying handle except through an admitted
//! endpoint.

#![no_std]

extern crate alloc;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l4-filtered",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the l4 interface uses but the world
    // does not name directly.
    generate_all,
});

use eo9::net::connection_policy::{self, EndpointKind};
use eo9::net::l4 as underlying;
use exports::eo9::net::l4::{
    self, Buffer, IpAddress, L4Error, RecvResult, SendResult, SocketAddress,
};

/// Map an exported socket address onto the underlying interface's (structurally
/// identical) type. The connection policy's `socket-address` is a `use` of the imported
/// `l4`, so the same value feeds both the policy and the underlying operation.
fn to_underlying(address: &SocketAddress) -> underlying::SocketAddress {
    underlying::SocketAddress {
        address: match address.address {
            IpAddress::V4(octets) => underlying::IpAddress::V4(octets),
            IpAddress::V6(groups) => underlying::IpAddress::V6(groups),
        },
        port: address.port,
    }
}

/// Map an underlying socket address onto the exported type.
fn to_export(address: &underlying::SocketAddress) -> SocketAddress {
    SocketAddress {
        address: match address.address {
            underlying::IpAddress::V4(octets) => IpAddress::V4(octets),
            underlying::IpAddress::V6(groups) => IpAddress::V6(groups),
        },
        port: address.port,
    }
}

/// Map the underlying provider's error onto this provider's exported error type.
fn map_error(error: underlying::L4Error) -> L4Error {
    match error {
        underlying::L4Error::Denied => L4Error::Denied,
        underlying::L4Error::Unreachable => L4Error::Unreachable,
        underlying::L4Error::ConnectionRefused => L4Error::ConnectionRefused,
        underlying::L4Error::ConnectionReset => L4Error::ConnectionReset,
        underlying::L4Error::TimedOut => L4Error::TimedOut,
        underlying::L4Error::AddressInUse => L4Error::AddressInUse,
        underlying::L4Error::AddressUnavailable => L4Error::AddressUnavailable,
        underlying::L4Error::NotConnected => L4Error::NotConnected,
        underlying::L4Error::MessageTooLarge => L4Error::MessageTooLarge,
        underlying::L4Error::Io(message) => L4Error::Io(message),
    }
}

/// Map send results across the boundary.
fn map_send(result: underlying::SendResult) -> SendResult {
    SendResult {
        bytes_sent: result.bytes_sent,
    }
}

/// Map receive results across the boundary.
fn map_recv(result: underlying::RecvResult) -> RecvResult {
    RecvResult {
        bytes_received: result.bytes_received,
    }
}

/// Whether the composed connection policy admits this endpoint operation.
fn admitted(kind: EndpointKind, endpoint: &underlying::SocketAddress) -> bool {
    connection_policy::admit(kind, *endpoint)
}

/// The `net.l4.filtered` provider.
struct Stub;

/// The exported root handle: a token for the filtered view.
struct FilteredL4;

/// An established TCP connection of the filtered view: wraps the underlying connection.
struct FilteredConnection {
    inner: underlying::TcpConnection,
}

/// A listening TCP socket of the filtered view: wraps the underlying listener.
struct FilteredListener {
    inner: underlying::TcpListener,
}

/// A bound UDP socket of the filtered view: wraps the underlying socket.
struct FilteredUdp {
    inner: underlying::UdpSocket,
}

impl l4::GuestL4Impl for FilteredL4 {}
impl l4::GuestTcpConnection for FilteredConnection {}
impl l4::GuestTcpListener for FilteredListener {}
impl l4::GuestUdpSocket for FilteredUdp {}

impl l4::Guest for Stub {
    type L4Impl = FilteredL4;
    type TcpConnection = FilteredConnection;
    type TcpListener = FilteredListener;
    type UdpSocket = FilteredUdp;

    fn default() -> l4::L4Impl {
        l4::L4Impl::new(FilteredL4)
    }

    async fn connect(
        _l4: l4::L4ImplBorrow<'_>,
        remote: SocketAddress,
    ) -> Result<l4::TcpConnection, L4Error> {
        let remote = to_underlying(&remote);
        if !admitted(EndpointKind::Connect, &remote) {
            return Err(L4Error::Denied);
        }
        let inner = underlying::connect(&underlying::default(), remote)
            .await
            .map_err(map_error)?;
        Ok(l4::TcpConnection::new(FilteredConnection { inner }))
    }

    async fn listen(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::TcpListener, L4Error> {
        let local = to_underlying(&local);
        if !admitted(EndpointKind::Listen, &local) {
            return Err(L4Error::Denied);
        }
        let inner = underlying::listen(&underlying::default(), local)
            .await
            .map_err(map_error)?;
        Ok(l4::TcpListener::new(FilteredListener { inner }))
    }

    async fn accept(
        l: l4::TcpListenerBorrow<'_>,
    ) -> Result<(l4::TcpConnection, SocketAddress), L4Error> {
        let listener = l.get::<FilteredListener>();
        let (inner, peer) = underlying::accept(&listener.inner)
            .await
            .map_err(map_error)?;
        Ok((
            l4::TcpConnection::new(FilteredConnection { inner }),
            to_export(&peer),
        ))
    }

    fn listener_address(l: l4::TcpListenerBorrow<'_>) -> SocketAddress {
        to_export(&underlying::listener_address(
            &l.get::<FilteredListener>().inner,
        ))
    }

    fn peer_address(c: l4::TcpConnectionBorrow<'_>) -> SocketAddress {
        to_export(&underlying::peer_address(
            &c.get::<FilteredConnection>().inner,
        ))
    }

    async fn send(
        c: l4::TcpConnectionBorrow<'_>,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let connection = c.get::<FilteredConnection>();
        let (src, result) = underlying::send(&connection.inner, src).await;
        (src, result.map(map_send).map_err(map_error))
    }

    async fn recv(
        c: l4::TcpConnectionBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L4Error>) {
        let connection = c.get::<FilteredConnection>();
        let (dst, result) = underlying::recv(&connection.inner, dst).await;
        (dst, result.map(map_recv).map_err(map_error))
    }

    async fn bind_udp(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::UdpSocket, L4Error> {
        let local = to_underlying(&local);
        if !admitted(EndpointKind::BindUdp, &local) {
            return Err(L4Error::Denied);
        }
        let inner = underlying::bind_udp(&underlying::default(), local)
            .await
            .map_err(map_error)?;
        Ok(l4::UdpSocket::new(FilteredUdp { inner }))
    }

    fn udp_address(s: l4::UdpSocketBorrow<'_>) -> SocketAddress {
        to_export(&underlying::udp_address(&s.get::<FilteredUdp>().inner))
    }

    async fn send_to(
        s: l4::UdpSocketBorrow<'_>,
        remote: SocketAddress,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let remote = to_underlying(&remote);
        // Per-datagram gate: an admitted local binding does not imply every remote is
        // reachable.
        if !admitted(EndpointKind::SendTo, &remote) {
            return (src, Err(L4Error::Denied));
        }
        let socket = s.get::<FilteredUdp>();
        let (src, result) = underlying::send_to(&socket.inner, remote, src).await;
        (src, result.map(map_send).map_err(map_error))
    }

    async fn recv_from(
        s: l4::UdpSocketBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, SocketAddress), L4Error>) {
        let socket = s.get::<FilteredUdp>();
        let (dst, result) = underlying::recv_from(&socket.inner, dst).await;
        (
            dst,
            result
                .map(|(received, sender)| (map_recv(received), to_export(&sender)))
                .map_err(map_error),
        )
    }
}

export!(Stub);
