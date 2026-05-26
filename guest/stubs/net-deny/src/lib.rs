//! `net.deny` — the network capability, present but refusing.
//!
//! Targets the `eo9:net/deny` stub world: exports `eo9:net/net` where every operation
//! that could grant reachability (`connect`, `listen`, `bind-udp`) fails with net's own
//! `denied` error (see SPEC.md, "The capability algebra": refusal is meaningful for net,
//! so it gets a deny stub in the API's own vocabulary).
//!
//! Because no connection, listener, or socket can ever be created, the operations on
//! those resources (`accept`, `send`, `recv`, `send-to`, `recv-from`) are unreachable:
//! their resource types are uninhabited, which the match below makes explicit.

#![no_std]

extern crate alloc;

use alloc::string::String;

// Linked for the guest runtime profile (allocator + panic handler).
use eo9_guest as _;

wit_bindgen::generate!({
    world: "deny",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the exported net interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::net::deny_config;
use exports::eo9::net::net::{
    self, Buffer, NetError, RecvResult, SendResult, SocketAddress, TcpConnection, TcpListener,
    UdpSocket,
};
use exports::eo9::net::types;

/// The `net.deny` provider.
struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct DenyNet;

/// Uninhabited representation for the connection/listener/socket resources: `net.deny`
/// never creates one, so the operations on them can never be reached.
enum NoSocket {}

impl NoSocket {
    /// Statically-checked unreachability: having a borrow of an uninhabited resource is
    /// a contradiction, which the empty match discharges.
    fn unreachable<T>(&self) -> T {
        match *self {}
    }
}

impl types::Guest for Stub {
    type NetImpl = DenyNet;
}

impl types::GuestNetImpl for DenyNet {}

impl net::GuestTcpConnection for NoSocket {}
impl net::GuestTcpListener for NoSocket {}
impl net::GuestUdpSocket for NoSocket {}

impl deny_config::Guest for Stub {
    async fn configure() -> Result<types::NetImpl, String> {
        Ok(types::NetImpl::new(DenyNet))
    }
}

impl net::Guest for Stub {
    type TcpConnection = NoSocket;
    type TcpListener = NoSocket;
    type UdpSocket = NoSocket;

    fn default() -> types::NetImpl {
        types::NetImpl::new(DenyNet)
    }

    async fn connect(
        _n: net::NetImplBorrow<'_>,
        _remote: SocketAddress,
    ) -> Result<TcpConnection, NetError> {
        Err(NetError::Denied)
    }

    async fn listen(
        _n: net::NetImplBorrow<'_>,
        _local: SocketAddress,
    ) -> Result<TcpListener, NetError> {
        Err(NetError::Denied)
    }

    async fn accept(
        l: net::TcpListenerBorrow<'_>,
    ) -> Result<(TcpConnection, SocketAddress), NetError> {
        l.get::<NoSocket>().unreachable()
    }

    async fn send(
        c: net::TcpConnectionBorrow<'_>,
        _src: Buffer,
    ) -> (Buffer, Result<SendResult, NetError>) {
        c.get::<NoSocket>().unreachable()
    }

    async fn recv(
        c: net::TcpConnectionBorrow<'_>,
        _dst: Buffer,
    ) -> (Buffer, Result<RecvResult, NetError>) {
        c.get::<NoSocket>().unreachable()
    }

    async fn bind_udp(
        _n: net::NetImplBorrow<'_>,
        _local: SocketAddress,
    ) -> Result<UdpSocket, NetError> {
        Err(NetError::Denied)
    }

    async fn send_to(
        s: net::UdpSocketBorrow<'_>,
        _remote: SocketAddress,
        _src: Buffer,
    ) -> (Buffer, Result<SendResult, NetError>) {
        s.get::<NoSocket>().unreachable()
    }

    async fn recv_from(
        s: net::UdpSocketBorrow<'_>,
        _dst: Buffer,
    ) -> (Buffer, Result<(RecvResult, SocketAddress), NetError>) {
        s.get::<NoSocket>().unreachable()
    }
}

export!(Stub);
