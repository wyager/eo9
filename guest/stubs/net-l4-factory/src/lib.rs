//! `l4.factory` — the shareable transport tail (shared-resources design, §5.2).
//!
//! Targets the `eo9:net/l4-factory-tail` stub world: imports `eo9:net/l4` and
//! re-exports it as pure delegation (the `net.l4.filtered` shape minus the policy),
//! plus the blessed `eo9:net/l4-factory`, whose `get` mints one handler per consumer
//! wiring. Composing it at the tail is the deliberate, visible act of making a
//! composition shareable:
//!
//!   net.virtio $ net.l4.over-l2 $ l4.factory      (the station's `lan` service)
//!   net.l4.loopback $ l4.factory                  (the gate's QEMU unit shape)
//!
//! Why the full `l4` re-export exists: the kernel call gate executes a child's gated
//! import by calling *exported* functions of the owner's instance — non-exported
//! (fused-internal) functions are unreachable from the embedder, so the serving
//! composition's terminal component must surface the whole transport contract. Every
//! method here is one delegation line; the wrapper resources are this component's own
//! exported types, so a consumer can never reach an underlying handle directly.
//!
//! v1 sharing is the degenerate kind: every `get` returns a fresh root-handle wrapper
//! onto the one shared stack — full access for every grantee. Scoping (per-consumer
//! wrappers that close over policy) is owner code by design ("policies are programs")
//! and the recorded follow-up.

#![no_std]

extern crate alloc;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l4-factory-tail",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the l4 interface uses but the world
    // does not name directly.
    generate_all,
});

use eo9::net::l4 as underlying;
use exports::eo9::net::l4::{
    self, Buffer, IpAddress, L4Error, RecvResult, SendResult, SocketAddress,
};
use exports::eo9::net::l4_factory;

/// Map an exported socket address onto the underlying interface's (structurally
/// identical) type.
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

/// The `l4.factory` provider.
struct Stub;

/// The exported root handle: a fresh wrapper onto the shared underlying stack (one per
/// `default()` / factory `get()` — v1's degenerate full sharing).
struct SharedL4;

/// An established TCP connection: wraps the underlying connection.
struct SharedConnection {
    inner: underlying::TcpConnection,
}

/// A listening TCP socket: wraps the underlying listener.
struct SharedListener {
    inner: underlying::TcpListener,
}

/// A bound UDP socket: wraps the underlying socket.
struct SharedUdp {
    inner: underlying::UdpSocket,
}

impl l4::GuestL4Impl for SharedL4 {}
impl l4::GuestTcpConnection for SharedConnection {}
impl l4::GuestTcpListener for SharedListener {}
impl l4::GuestUdpSocket for SharedUdp {}

impl l4_factory::Guest for Stub {
    /// One handler per consumer wiring. v1: a fresh full-access wrapper onto the one
    /// shared stack; a scoping factory would close policy state into the wrapper here.
    fn get() -> Result<l4_factory::L4Impl, l4_factory::L4Error> {
        Ok(l4::L4Impl::new(SharedL4))
    }
}

impl l4::Guest for Stub {
    type L4Impl = SharedL4;
    type TcpConnection = SharedConnection;
    type TcpListener = SharedListener;
    type UdpSocket = SharedUdp;

    fn default() -> l4::L4Impl {
        l4::L4Impl::new(SharedL4)
    }

    async fn connect(
        _l4: l4::L4ImplBorrow<'_>,
        remote: SocketAddress,
    ) -> Result<l4::TcpConnection, L4Error> {
        let inner = underlying::connect(&underlying::default(), to_underlying(&remote))
            .await
            .map_err(map_error)?;
        Ok(l4::TcpConnection::new(SharedConnection { inner }))
    }

    async fn listen(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::TcpListener, L4Error> {
        let inner = underlying::listen(&underlying::default(), to_underlying(&local))
            .await
            .map_err(map_error)?;
        Ok(l4::TcpListener::new(SharedListener { inner }))
    }

    async fn accept(
        l: l4::TcpListenerBorrow<'_>,
    ) -> Result<(l4::TcpConnection, SocketAddress), L4Error> {
        let listener = l.get::<SharedListener>();
        let (inner, peer) = underlying::accept(&listener.inner)
            .await
            .map_err(map_error)?;
        Ok((
            l4::TcpConnection::new(SharedConnection { inner }),
            to_export(&peer),
        ))
    }

    fn listener_address(l: l4::TcpListenerBorrow<'_>) -> SocketAddress {
        to_export(&underlying::listener_address(
            &l.get::<SharedListener>().inner,
        ))
    }

    fn peer_address(c: l4::TcpConnectionBorrow<'_>) -> SocketAddress {
        to_export(&underlying::peer_address(
            &c.get::<SharedConnection>().inner,
        ))
    }

    async fn send(
        c: l4::TcpConnectionBorrow<'_>,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let connection = c.get::<SharedConnection>();
        let (src, result) = underlying::send(&connection.inner, src).await;
        (src, result.map(map_send).map_err(map_error))
    }

    async fn recv(
        c: l4::TcpConnectionBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L4Error>) {
        let connection = c.get::<SharedConnection>();
        let (dst, result) = underlying::recv(&connection.inner, dst).await;
        (dst, result.map(map_recv).map_err(map_error))
    }

    async fn bind_udp(
        _l4: l4::L4ImplBorrow<'_>,
        local: SocketAddress,
    ) -> Result<l4::UdpSocket, L4Error> {
        let inner = underlying::bind_udp(&underlying::default(), to_underlying(&local))
            .await
            .map_err(map_error)?;
        Ok(l4::UdpSocket::new(SharedUdp { inner }))
    }

    fn udp_address(s: l4::UdpSocketBorrow<'_>) -> SocketAddress {
        to_export(&underlying::udp_address(&s.get::<SharedUdp>().inner))
    }

    async fn send_to(
        s: l4::UdpSocketBorrow<'_>,
        remote: SocketAddress,
        src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        let socket = s.get::<SharedUdp>();
        let (src, result) = underlying::send_to(&socket.inner, to_underlying(&remote), src).await;
        (src, result.map(map_send).map_err(map_error))
    }

    async fn recv_from(
        s: l4::UdpSocketBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, SocketAddress), L4Error>) {
        let socket = s.get::<SharedUdp>();
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
