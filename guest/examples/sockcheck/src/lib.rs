//! sockcheck — the transport-layer (eo9:net/l4) example program.
//!
//! Targets the `eo9-examples:sockcheck/sockcheck` world (see `wit/world.wit`): listens
//! on an ephemeral loopback port, checks the listen/connect refusal cases (duplicate
//! bind, dead port), queues two connections on the listener's backlog before accepting
//! either, echoes distinct payloads across both accepted pairs (proving FIFO accept
//! order and that the streams are not crossed), then round-trips one UDP datagram
//! between two ephemeral sockets — all against whatever `eo9:net/l4` provider it is
//! composed with (`net.l4.loopback` in the tests, a real transport later).
//!
//! The listen call comes first deliberately: composed over a transport whose link is
//! denied or down (`net.l2.deny $ net.l4.over-l2 $ sockcheck`), the program's failure is
//! the *listen* path's typed error, which is what the integration tests pin.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::net::l4;
use eo9_guest::buffer;

eo9_guest::bindings!({
    world: "sockcheck",
    apis: [io, net_l4],
});

/// The l4 API's own error, rendered into the world's failure variant.
fn net_failure(err: l4::L4Error) -> ProgramFailure {
    ProgramFailure::Net(format!("{err:?}"))
}

/// 127.0.0.1:`port`.
fn loopback(port: u16) -> l4::SocketAddress {
    l4::SocketAddress {
        address: l4::IpAddress::V4((127, 0, 0, 1)),
        port,
    }
}

/// 0.0.0.0:`port` — the unspecified address: "bind to whatever this provider's local
/// address is". The portable listen spelling across l4 providers (the loopback stub
/// canonicalizes it to 127.0.0.1, a real transport binds its interface address); binding
/// a literal 127.0.0.1 only works on providers that actually have a loopback interface.
fn any(port: u16) -> l4::SocketAddress {
    l4::SocketAddress {
        address: l4::IpAddress::V4((0, 0, 0, 0)),
        port,
    }
}

/// A copy of a socket address (the generated record is not `Copy`; sockcheck needs to
/// hand the listener's address to several calls).
fn dup(addr: &l4::SocketAddress) -> l4::SocketAddress {
    let address = match &addr.address {
        l4::IpAddress::V4(octets) => l4::IpAddress::V4(*octets),
        l4::IpAddress::V6(groups) => l4::IpAddress::V6(*groups),
    };
    l4::SocketAddress {
        address,
        port: addr.port,
    }
}

/// Send `bytes` from `tx` and receive them on `rx`, checking they arrive intact.
/// Returns the number of bytes verified.
async fn echo(
    tx: &l4::TcpConnection,
    rx: &l4::TcpConnection,
    bytes: &[u8],
) -> Result<u64, ProgramFailure> {
    let src = buffer::from_bytes(bytes);
    let (_src, sent) = l4::send(tx, src).await;
    let sent = sent.map_err(net_failure)?;
    if sent.bytes_sent != bytes.len() as u64 {
        return Err(ProgramFailure::Mismatch(String::from("tcp short send")));
    }
    let dst = buffer::with_capacity(bytes.len() as u64);
    let (dst, received) = l4::recv(rx, dst).await;
    let received = received.map_err(net_failure)?;
    if buffer::prefix_to_vec(&dst, received.bytes_received) != bytes {
        return Err(ProgramFailure::Mismatch(String::from("tcp payload")));
    }
    Ok(received.bytes_received)
}

eo9_guest::main! {
    async fn main(payload: String) -> Result<ProgramSuccess, ProgramFailure> {
        if payload.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from(
                "payload must not be empty",
            )));
        }

        let root = l4::default();

        // --- TCP: listen on an ephemeral port of the provider's local address. This is
        //     deliberately the first transport operation (see the crate header).
        let listener = l4::listen(&root, any(0)).await.map_err(net_failure)?;
        let server_addr = l4::listener_address(&listener);

        // The port the listener holds cannot be bound a second time (whatever local
        // address spelling is used to try).
        match l4::listen(&root, any(server_addr.port)).await {
            Err(l4::L4Error::AddressInUse) => {}
            Ok(_) => {
                return Err(ProgramFailure::Mismatch(String::from(
                    "two listeners bound the same port",
                )));
            }
            Err(other) => return Err(net_failure(other)),
        }

        // Connecting where nothing listens is a typed refusal, never a hang.
        match l4::connect(&root, loopback(1)).await {
            Err(l4::L4Error::ConnectionRefused) => {}
            Ok(_) => {
                return Err(ProgramFailure::Mismatch(String::from(
                    "connecting to a dead port succeeded",
                )));
            }
            Err(other) => return Err(net_failure(other)),
        }

        // --- Backlog: two clients connect before anything is accepted; both sit queued on
        //     the listener until accept hands them back, first-come first-served.
        let client_a = l4::connect(&root, dup(&server_addr))
            .await
            .map_err(net_failure)?;
        let client_b = l4::connect(&root, dup(&server_addr))
            .await
            .map_err(net_failure)?;
        for client in [&client_a, &client_b] {
            if l4::peer_address(client).port != server_addr.port {
                return Err(ProgramFailure::Mismatch(String::from(
                    "client peer address is not the listener",
                )));
            }
        }
        let (server_a, _peer_a) = l4::accept(&listener).await.map_err(net_failure)?;
        let (server_b, _peer_b) = l4::accept(&listener).await.map_err(net_failure)?;

        // Distinct payloads prove the first accepted connection is wired to the first
        // client (FIFO backlog) and the two pairs' streams are not crossed; `echo`
        // reports a mismatch if either property fails.
        let mut verified: u64 = 0;
        let first = format!("a:{payload}");
        let second = format!("b:{payload}");
        verified += echo(&client_a, &server_a, first.as_bytes()).await?;
        verified += echo(&client_b, &server_b, second.as_bytes()).await?;
        // And an accepted connection can talk back to its client.
        let reversed: Vec<u8> = payload.as_bytes().iter().rev().copied().collect();
        verified += echo(&server_a, &client_a, &reversed).await?;

        // --- UDP: two ephemeral sockets, one datagram across.
        let sender = l4::bind_udp(&root, any(0)).await.map_err(net_failure)?;
        let receiver = l4::bind_udp(&root, any(0)).await.map_err(net_failure)?;
        let src = buffer::from_bytes(payload.as_bytes());
        let (_src, sent) = l4::send_to(&sender, l4::udp_address(&receiver), src).await;
        sent.map_err(net_failure)?;
        let dst = buffer::with_capacity(payload.len() as u64);
        let (dst, received) = l4::recv_from(&receiver, dst).await;
        let (received, from) = received.map_err(net_failure)?;
        if buffer::prefix_to_vec(&dst, received.bytes_received) != payload.as_bytes() {
            return Err(ProgramFailure::Mismatch(String::from("udp payload")));
        }
        if from.port != l4::udp_address(&sender).port {
            return Err(ProgramFailure::Mismatch(String::from("udp sender address")));
        }
        verified += received.bytes_received;

        Ok(ProgramSuccess::Echoed(verified))
    }
}
