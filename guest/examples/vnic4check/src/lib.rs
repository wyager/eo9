//! vnic4check — two whole TCP/IP stacks over one switched link.
//!
//! Targets the `eo9-examples:vnic4check/vnic4check` world (see `wit/world.wit`): two
//! `eo9:net/l4` capabilities under the named slots `left` and `right` — each a
//! `net.l4.over-l2` middleware riding its own virtual NIC of one `net.l2.switch` —
//! and one UDP round-trip is completed on each, independently. The success value
//! reports both, so the composer can see two stacks really shared one link.
//!
//! Modes: `echo` expects the sent payload back (the `net.l2.echo` fixture upstream);
//! `dns` sends a DNS query for `example.com` and accepts any well-formed answer (the
//! QEMU user-net resolver at 10.0.2.3 when `net.virtio` owns the real link).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

wit_bindgen::generate!({
    world: "vnic4check",
    path: "wit",
    with: {
        "eo9:io/buffers@0.1.0": eo9_guest::api::io::buffers,
        "eo9:text/types@0.1.0": eo9_guest::api::text::types,
        "eo9:text/text@0.1.0": eo9_guest::api::text::text,
    },
    generate_all,
});

use eo9_guest::buffer;

/// A fixed DNS query id (the reply must echo it back).
const QUERY_ID: u16 = 0xe09;
/// Datagrams to inspect per stack before giving up on the answer.
const RECEIVE_ATTEMPTS: u32 = 4;

/// A DNS query: one A/IN question for example.com, recursion desired.
fn dns_query() -> Vec<u8> {
    let mut packet = Vec::with_capacity(32);
    packet.extend_from_slice(&QUERY_ID.to_be_bytes());
    packet.extend_from_slice(&[0x01, 0x00]); // flags: recursion desired
    packet.extend_from_slice(&[0x00, 0x01]); // one question
    packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in ["example", "com"] {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&[0x00, 0x01]); // type A
    packet.extend_from_slice(&[0x00, 0x01]); // class IN
    packet
}

/// Whether `packet` is a well-formed answer to our query.
fn dns_answered(packet: &[u8]) -> Result<(), String> {
    if packet.len() < 12 {
        return Err(String::from("the reply is shorter than a DNS header"));
    }
    if packet[0..2] != QUERY_ID.to_be_bytes() {
        return Err(String::from("the reply does not echo our query id"));
    }
    if packet[2] & 0x80 == 0 {
        return Err(String::from("the reply is not a response"));
    }
    let rcode = packet[3] & 0x0f;
    if rcode != 0 {
        return Err(format!("the resolver answered with rcode {rcode}"));
    }
    let answers = u16::from_be_bytes([packet[6], packet[7]]);
    if answers == 0 {
        return Err(String::from("the resolver answered with no records"));
    }
    Ok(())
}

fn parse_peer(text: &str) -> Result<(u8, u8, u8, u8), ProgramFailure> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in text.split('.') {
        if count == 4 {
            return Err(ProgramFailure::Check(format!(
                "not a dotted quad: {text:?}"
            )));
        }
        octets[count] = part
            .parse::<u8>()
            .map_err(|_| ProgramFailure::Check(format!("not a dotted quad: {text:?}")))?;
        count += 1;
    }
    if count != 4 {
        return Err(ProgramFailure::Check(format!(
            "not a dotted quad: {text:?}"
        )));
    }
    Ok((octets[0], octets[1], octets[2], octets[3]))
}

/// The per-stack round-trip, written once and instantiated for each named slot (each
/// slot mints its own nominal types, so this is a macro rather than a generic).
macro_rules! stack_driver {
    ($module:ident, $round_trip:ident) => {
        /// Bind, send the mode's datagram to the peer, and wait for the mode's reply.
        /// Returns a short report for the success value.
        async fn $round_trip(
            peer: (u8, u8, u8, u8),
            peer_port: u16,
            mode: &str,
            marker: &str,
        ) -> Result<String, ProgramFailure> {
            let failure = |err: $module::L4Error| match err {
                $module::L4Error::Denied => ProgramFailure::Denied,
                other => ProgramFailure::Net(format!("{}: {other:?}", stringify!($module))),
            };

            let root = $module::default();
            let socket = $module::bind_udp(
                &root,
                $module::SocketAddress {
                    address: $module::IpAddress::V4((0, 0, 0, 0)),
                    port: 0,
                },
            )
            .await
            .map_err(failure)?;

            let datagram = match mode {
                "dns" => dns_query(),
                _ => Vec::from(marker.as_bytes()),
            };
            let to = $module::SocketAddress {
                address: $module::IpAddress::V4(peer),
                port: peer_port,
            };
            let (_sent, send_outcome) =
                $module::send_to(&socket, to, buffer::from_bytes(&datagram)).await;
            send_outcome.map_err(failure)?;

            for _ in 0..RECEIVE_ATTEMPTS {
                let dst = buffer::with_capacity(1536);
                let (dst, received) = $module::recv_from(&socket, dst).await;
                match received {
                    Ok((result, from)) => {
                        let reply = buffer::prefix_to_vec(&dst, result.bytes_received);
                        if mode == "dns" {
                            dns_answered(&reply).map_err(ProgramFailure::Check)?;
                            return Ok(format!("dns answered ({} bytes)", reply.len()));
                        }
                        if reply == datagram {
                            let port = from.port;
                            return Ok(format!("echoed {} bytes from port {port}", reply.len()));
                        }
                        // Not ours (stray datagram): keep waiting.
                    }
                    Err($module::L4Error::TimedOut) => {
                        return Err(ProgramFailure::Check(format!(
                            "{}: timed out waiting for the {mode} reply",
                            stringify!($module)
                        )));
                    }
                    Err(err) => return Err(failure(err)),
                }
            }
            Err(ProgramFailure::Check(format!(
                "{}: no {mode} reply within {RECEIVE_ATTEMPTS} datagrams",
                stringify!($module)
            )))
        }
    };
}

stack_driver!(left, round_trip_left);
stack_driver!(right, round_trip_right);

eo9_guest::main! {
    async fn main(
        peer: String,
        peer_port: u16,
        mode: String,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        let peer = parse_peer(&peer)?;
        let left_report = round_trip_left(peer, peer_port, &mode, "vnic4-left").await?;
        let right_report = round_trip_right(peer, peer_port, &mode, "vnic4-right").await?;
        Ok(ProgramSuccess::Verified(format!(
            "left={left_report} right={right_report}"
        )))
    }
}
