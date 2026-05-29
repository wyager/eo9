//! `net.l4.deny` — the transport-layer network capability, present but refusing.
//!
//! Targets the `eo9:net/l4-deny` stub world: exports `eo9:net/l4` where every operation
//! that could grant reachability (`connect`, `listen`, `bind-udp`) fails with the
//! layer's own `denied` error (see SPEC.md, "The capability algebra": refusal is
//! meaningful for net, so each layer gets a deny stub in the API's own vocabulary).
//!
//! Because no connection, listener, or socket can ever be created, the operations on
//! those resources (`accept`, `send`, `recv`, `send-to`, `recv-from`, the address
//! accessors) are unreachable: their resource types are uninhabited, which the match
//! below makes explicit.

#![no_std]

extern crate alloc;

use alloc::string::String;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "l4-deny",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the exported l4 interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::net::l4::{
    self, Buffer, L4Error, RecvResult, SendResult, SocketAddress,
};
use exports::eo9::net::l4_deny_config;

/// The `net.l4.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct DenyL4;

/// Uninhabited representation for the connection/listener/socket resources:
/// `net.l4.deny` never creates one, so the operations on them can never be reached.
enum NoSocket {}

impl NoSocket {
    /// Statically-checked unreachability: having a borrow of an uninhabited resource is
    /// a contradiction, which the empty match discharges.
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}

impl l4::GuestL4Impl for DenyL4 {}
impl l4::GuestTcpConnection for NoSocket {}
impl l4::GuestTcpListener for NoSocket {}
impl l4::GuestUdpSocket for NoSocket {}

impl l4_deny_config::Guest for Stub {
    fn configure() -> Result<l4::L4Impl, String> {
        Ok(l4::L4Impl::new(DenyL4))
    }
}

impl l4::Guest for Stub {
    type L4Impl = DenyL4;
    type TcpConnection = NoSocket;
    type TcpListener = NoSocket;
    type UdpSocket = NoSocket;

    fn default() -> l4::L4Impl {
        l4::L4Impl::new(DenyL4)
    }

    async fn connect(
        _l4: l4::L4ImplBorrow<'_>,
        _remote: SocketAddress,
    ) -> Result<l4::TcpConnection, L4Error> {
        Err(L4Error::Denied)
    }

    async fn listen(
        _l4: l4::L4ImplBorrow<'_>,
        _local: SocketAddress,
    ) -> Result<l4::TcpListener, L4Error> {
        Err(L4Error::Denied)
    }

    async fn accept(
        l: l4::TcpListenerBorrow<'_>,
    ) -> Result<(l4::TcpConnection, SocketAddress), L4Error> {
        l.get::<NoSocket>().unreachable()
    }

    fn listener_address(l: l4::TcpListenerBorrow<'_>) -> SocketAddress {
        l.get::<NoSocket>().unreachable()
    }

    fn peer_address(c: l4::TcpConnectionBorrow<'_>) -> SocketAddress {
        c.get::<NoSocket>().unreachable()
    }

    async fn send(
        c: l4::TcpConnectionBorrow<'_>,
        _src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        c.get::<NoSocket>().unreachable()
    }

    async fn recv(
        c: l4::TcpConnectionBorrow<'_>,
        _dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L4Error>) {
        c.get::<NoSocket>().unreachable()
    }

    async fn bind_udp(
        _l4: l4::L4ImplBorrow<'_>,
        _local: SocketAddress,
    ) -> Result<l4::UdpSocket, L4Error> {
        Err(L4Error::Denied)
    }

    fn udp_address(s: l4::UdpSocketBorrow<'_>) -> SocketAddress {
        s.get::<NoSocket>().unreachable()
    }

    async fn send_to(
        s: l4::UdpSocketBorrow<'_>,
        _remote: SocketAddress,
        _src: Buffer,
    ) -> (Buffer, Result<SendResult, L4Error>) {
        s.get::<NoSocket>().unreachable()
    }

    async fn recv_from(
        s: l4::UdpSocketBorrow<'_>,
        _dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, SocketAddress), L4Error>) {
        s.get::<NoSocket>().unreachable()
    }
}

export!(Stub);
