//! l4check — the transport-layer (eo9:net/l4) example program.
//!
//! Targets the `eo9-examples:l4check/l4check` world (see `wit/world.wit`): bind a UDP
//! socket through the granted l4 capability, send a DNS query for `example.com` to the
//! QEMU user-net resolver (10.0.2.3), and report what came back; then attempt a TCP
//! connection to the gateway's discard port (10.0.2.2:9) and report its typed outcome.
//! A DNS answer proves datagrams travel both ways through the composed transport stack
//! (`net.virtio $ net.l4.over-l2` on metal); the TCP attempt proves a refused or
//! ignored SYN comes back as a typed error, never a trap. The program imports only
//! `eo9:net/l4` — what the resolver answered is carried in the program outcome itself.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::net::l4;
use eo9_guest::buffer;

eo9_guest::bindings!({
    world: "l4check",
    apis: [io, net_l4],
});

/// The DNS forwarder QEMU user-mode networking runs for its guest.
const RESOLVER: l4::IpAddress = l4::IpAddress::V4((10, 0, 2, 3));
/// The user-net gateway; nothing listens on its discard port, which is the point.
const GATEWAY: l4::IpAddress = l4::IpAddress::V4((10, 0, 2, 2));
/// The name the query asks about.
const QUERY_NAME: &[&str] = &["example", "com"];
/// A fixed query id (the reply must echo it back).
const QUERY_ID: u16 = 0xe09;
/// How many datagrams to inspect before giving up on the answer.
const RECEIVE_ATTEMPTS: u32 = 4;

/// The l4 API's own error, rendered into the world's failure variant.
fn net_failure(err: l4::L4Error) -> ProgramFailure {
    match err {
        l4::L4Error::Denied => ProgramFailure::Denied,
        other => ProgramFailure::Net(format!("{other:?}")),
    }
}

/// A DNS query: header asking for recursion, one A/IN question for [`QUERY_NAME`].
fn dns_query() -> Vec<u8> {
    let mut packet = Vec::with_capacity(32);
    packet.extend_from_slice(&QUERY_ID.to_be_bytes()); // id
    packet.extend_from_slice(&[0x01, 0x00]); // flags: recursion desired
    packet.extend_from_slice(&[0x00, 0x01]); // one question
    packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // no other records
    for label in QUERY_NAME {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0); // end of name
    packet.extend_from_slice(&[0x00, 0x01]); // type A
    packet.extend_from_slice(&[0x00, 0x01]); // class IN
    packet
}

/// The end of a (possibly compression-pointed) DNS name starting at `at`.
fn skip_name(packet: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *packet.get(at)? as usize;
        if len == 0 {
            return Some(at + 1);
        }
        if len & 0xc0 == 0xc0 {
            return Some(at + 2); // compression pointer: two bytes, then done
        }
        at += 1 + len;
    }
}

/// What the resolver said: the first A record if one can be extracted, otherwise a
/// summary of the answer header. `None` if this datagram is not an answer to us.
fn parse_reply(packet: &[u8]) -> Option<Result<String, String>> {
    if packet.len() < 12 {
        return None;
    }
    if packet[0..2] != QUERY_ID.to_be_bytes() {
        return None; // not our query
    }
    if packet[2] & 0x80 == 0 {
        return None; // not a response
    }
    let rcode = packet[3] & 0x0f;
    if rcode != 0 {
        return Some(Err(format!("the resolver answered with rcode {rcode}")));
    }
    let questions = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let answers = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    if answers == 0 {
        return Some(Err(String::from("the resolver answered with no records")));
    }

    // Walk past the question section, then look for the first A/IN answer.
    let mut at = 12;
    for _ in 0..questions {
        at = match skip_name(packet, at) {
            Some(next) => next + 4, // qtype + qclass
            None => return Some(Ok(format!("answered ({answers} records)"))),
        };
    }
    for _ in 0..answers {
        let after_name = match skip_name(packet, at) {
            Some(next) => next,
            None => break,
        };
        if packet.len() < after_name + 10 {
            break;
        }
        let rtype = u16::from_be_bytes([packet[after_name], packet[after_name + 1]]);
        let rdlength =
            u16::from_be_bytes([packet[after_name + 8], packet[after_name + 9]]) as usize;
        let rdata = after_name + 10;
        if rtype == 1 && rdlength == 4 && packet.len() >= rdata + 4 {
            return Some(Ok(format!(
                "{}.{}.{}.{}",
                packet[rdata],
                packet[rdata + 1],
                packet[rdata + 2],
                packet[rdata + 3]
            )));
        }
        at = rdata + rdlength;
    }
    Some(Ok(format!("answered ({answers} records)")))
}

eo9_guest::main! {
    async fn main() -> Result<ProgramSuccess, ProgramFailure> {
        let root = l4::default();

        // --- UDP: ask the user-net resolver about example.com -----------------------
        let socket = l4::bind_udp(
            &root,
            l4::SocketAddress { address: l4::IpAddress::V4((0, 0, 0, 0)), port: 0 },
        )
        .await
        .map_err(net_failure)?;

        let query = buffer::from_bytes(&dns_query());
        let resolver = l4::SocketAddress { address: RESOLVER, port: 53 };
        let (_query, sent) = l4::send_to(&socket, resolver, query).await;
        sent.map_err(net_failure)?;

        let mut answer: Option<String> = None;
        let mut last_problem = String::from("no datagram came back");
        for _ in 0..RECEIVE_ATTEMPTS {
            let dst = buffer::with_capacity(1536);
            let (dst, received) = l4::recv_from(&socket, dst).await;
            match received {
                Ok((result, _from)) => {
                    let datagram = buffer::prefix_to_vec(&dst, result.bytes_received);
                    match parse_reply(&datagram) {
                        Some(Ok(found)) => {
                            answer = Some(found);
                            break;
                        }
                        Some(Err(problem)) => {
                            last_problem = problem;
                            break;
                        }
                        None => {
                            last_problem = format!(
                                "received {} byte(s) that were not our answer",
                                result.bytes_received
                            );
                        }
                    }
                }
                Err(l4::L4Error::Denied) => return Err(ProgramFailure::Denied),
                Err(l4::L4Error::TimedOut) => {
                    last_problem = String::from("timed out waiting for the resolver");
                    break;
                }
                Err(other) => {
                    last_problem = format!("{other:?}");
                    break;
                }
            }
        }
        let Some(answer) = answer else {
            return Err(ProgramFailure::NoAnswer(last_problem));
        };

        // --- TCP: a connection attempt that should come back as a typed outcome -----
        let gateway = l4::SocketAddress { address: GATEWAY, port: 9 };
        let tcp_outcome = match l4::connect(&root, gateway).await {
            Ok(connection) => {
                let peer = l4::peer_address(&connection);
                format!("unexpectedly connected to port {}", peer.port)
            }
            Err(err) => format!("{err:?}"),
        };

        Ok(ProgramSuccess::Resolved(format!(
            "example.com is {answer}; tcp 10.0.2.2:9 -> {tcp_outcome}"
        )))
    }
}
