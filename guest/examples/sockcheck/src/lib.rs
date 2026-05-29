//! sockcheck — the transport-layer (eo9:net/l4) example program.
//!
//! Targets the `eo9-examples:sockcheck/sockcheck` world (see `wit/world.wit`): listens
//! on an ephemeral loopback port, connects to it, accepts, echoes the payload across
//! the TCP pair in both directions, then round-trips one UDP datagram between two
//! ephemeral sockets — all against whatever `eo9:net/l4` provider it is composed with
//! (`net.l4.loopback` in the tests, a real transport later).

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

        // --- TCP: listen on an ephemeral loopback port, connect, accept, echo both ways.
        let listener = l4::listen(&root, loopback(0)).await.map_err(net_failure)?;
        let server_addr = l4::listener_address(&listener);
        let client = l4::connect(&root, server_addr).await.map_err(net_failure)?;
        let (server, _client_addr) = l4::accept(&listener).await.map_err(net_failure)?;

        let mut verified: u64 = 0;
        verified += echo(&client, &server, payload.as_bytes()).await?;
        let reversed: Vec<u8> = payload.as_bytes().iter().rev().copied().collect();
        verified += echo(&server, &client, &reversed).await?;

        // --- UDP: two ephemeral sockets, one datagram across.
        let first = l4::bind_udp(&root, loopback(0)).await.map_err(net_failure)?;
        let second = l4::bind_udp(&root, loopback(0)).await.map_err(net_failure)?;
        let src = buffer::from_bytes(payload.as_bytes());
        let (_src, sent) = l4::send_to(&first, l4::udp_address(&second), src).await;
        sent.map_err(net_failure)?;
        let dst = buffer::with_capacity(payload.len() as u64);
        let (dst, received) = l4::recv_from(&second, dst).await;
        let (received, from) = received.map_err(net_failure)?;
        if buffer::prefix_to_vec(&dst, received.bytes_received) != payload.as_bytes() {
            return Err(ProgramFailure::Mismatch(String::from("udp payload")));
        }
        if from.port != l4::udp_address(&first).port {
            return Err(ProgramFailure::Mismatch(String::from("udp sender address")));
        }
        verified += received.bytes_received;

        Ok(ProgramSuccess::Echoed(verified))
    }
}
