//! l4check — the transport-layer (eo9:net/l4) example program.
//!
//! Targets the `eo9-examples:l4check/l4check` world (see `wit/world.wit`): bind a UDP
//! socket through the granted l4 capability, send a DNS query for `example.com` to the
//! resolver, and report what came back; then attempt a TCP connection to the tcp-target
//! host's discard port (:9) and report its typed outcome. A DNS answer proves
//! datagrams travel both ways through the composed transport stack
//! (`net.virtio $ net.l4.over-l2` on QEMU metal, `net.rtl8125 $ net.l4.over-l2` on the
//! board); the TCP attempt proves a refused or ignored SYN comes back as a typed
//! error, never a trap. The targets default to QEMU user-net's layout (resolver
//! 10.0.2.3, tcp-target 10.0.2.2); `--resolver`/`--tcp-target` aim them at a real LAN's
//! addresses. The program imports only `eo9:net/l4` — what the resolver answered is
//! carried in the program outcome itself.

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

/// The DNS forwarder QEMU user-mode networking runs for its guest (the default
/// `--resolver`).
const DEFAULT_RESOLVER: (u8, u8, u8, u8) = (10, 0, 2, 3);
/// The user-net gateway; nothing listens on its discard port, which is the point
/// (the default `--tcp-target`).
const DEFAULT_TCP_TARGET: (u8, u8, u8, u8) = (10, 0, 2, 2);
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

/// A DNS query: header asking for recursion, one A/IN question for [`QUERY_NAME`]
/// (the shared encoder in `eo9-dns`, host-tested; the encoding is pinned there
/// byte-for-byte against what this example built by hand before the factor-out).
fn dns_query() -> Result<Vec<u8>, ProgramFailure> {
    eo9_dns::query(QUERY_ID, QUERY_NAME.iter().copied()).map_err(|err| {
        // Unreachable for the constant, valid [`QUERY_NAME`]; kept typed, never a trap.
        ProgramFailure::Net(format!("encoding the {QUERY_NAME:?} query: {err:?}"))
    })
}

/// What the resolver said: the first A record if one can be extracted, otherwise a
/// summary of the answer header. `None` if this datagram is not an answer to us.
/// (The wire walk lives in `eo9-dns`; the messages here are this example's own and
/// unchanged by the factor-out.)
fn parse_reply(packet: &[u8]) -> Option<Result<String, String>> {
    let parsed = eo9_dns::parse_reply(packet, QUERY_ID)?;
    Some(match parsed {
        Ok(eo9_dns::Reply::A(a, b, c, d)) => Ok(format!("{a}.{b}.{c}.{d}")),
        Ok(eo9_dns::Reply::Answered(answers)) => Ok(format!("answered ({answers} records)")),
        Err(eo9_dns::ReplyError::Rcode(rcode)) => {
            Err(format!("the resolver answered with rcode {rcode}"))
        }
        Err(eo9_dns::ReplyError::NoRecords) => {
            Err(String::from("the resolver answered with no records"))
        }
    })
}

/// Parse a dotted quad (`--resolver 10.20.3.1`).
fn parse_ip(text: &str) -> Result<(u8, u8, u8, u8), ProgramFailure> {
    let mut octets = [0u8; 4];
    let mut count = 0;
    for part in text.split('.') {
        if count == 4 {
            return Err(ProgramFailure::BadArguments(format!(
                "not a dotted quad: {text:?}"
            )));
        }
        octets[count] = part
            .parse::<u8>()
            .map_err(|_| ProgramFailure::BadArguments(format!("not a dotted quad: {text:?}")))?;
        count += 1;
    }
    if count != 4 {
        return Err(ProgramFailure::BadArguments(format!(
            "not a dotted quad: {text:?}"
        )));
    }
    Ok((octets[0], octets[1], octets[2], octets[3]))
}

eo9_guest::main! {
    async fn main(
        resolver: Option<String>,
        tcp_target: Option<String>,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        let resolver_ip = match &resolver {
            Some(text) => parse_ip(text)?,
            None => DEFAULT_RESOLVER,
        };
        let probe_ip = match &tcp_target {
            Some(text) => parse_ip(text)?,
            None => DEFAULT_TCP_TARGET,
        };
        let root = l4::default();

        // --- UDP: ask the user-net resolver about example.com -----------------------
        let socket = l4::bind_udp(
            &root,
            l4::SocketAddress { address: l4::IpAddress::V4((0, 0, 0, 0)), port: 0 },
        )
        .await
        .map_err(net_failure)?;

        let query = buffer::from_bytes(&dns_query()?);
        let resolver = l4::SocketAddress { address: l4::IpAddress::V4(resolver_ip), port: 53 };
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
        let gateway = l4::SocketAddress { address: l4::IpAddress::V4(probe_ip), port: 9 };
        let tcp_outcome = match l4::connect(&root, gateway).await {
            Ok(connection) => {
                let peer = l4::peer_address(&connection);
                format!("unexpectedly connected to port {}", peer.port)
            }
            Err(err) => format!("{err:?}"),
        };

        let (pa, pb, pc, pd) = probe_ip;
        Ok(ProgramSuccess::Resolved(format!(
            "example.com is {answer}; tcp {pa}.{pb}.{pc}.{pd}:9 -> {tcp_outcome}"
        )))
    }
}
