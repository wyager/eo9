//! l2check — the link-layer (eo9:net/l2) example program, grown into a one-shot link
//! probe (plan/09 D46 round 7).
//!
//! Targets the `eo9-examples:l2check/l2check` world (see `wit/world.wit`): list the
//! granted l2 capability's interfaces, open the first one, then for one bounded
//! receive window:
//!
//! * **ARP, retransmitted** — the who-has for the gateway is re-sent throughout the
//!   window (~once a second; a single shot is indefensible into a lossy or filtered
//!   port).
//! * **ARP responder** — any who-has for OUR address (`--source`) is answered, so a
//!   peer pinging us measures directly whether our ARP transmit crosses the switch.
//! * **UDP broadcast beacon** (`--beacon true`) — `source`:19099 →
//!   255.255.255.255:19099, payload `eo9-beacon <seq>`, ~twice a second. Ethernet
//!   broadcast needs no ARP resolution, so on a filtered port the beacon
//!   discriminates: beacon arrives + ARP does not = ARP-specific ingress filtering
//!   (DAI-class); nothing arrives = MAC-level port security.
//!
//! A final console line reports the probe counts (ARP sent / who-has answered /
//! beacons sent) on success AND failure, so every bench run advances a round.
//!
//! Pacing note: this world deliberately imports no clock — pacing rides the polling
//! cadence. One empty `recv-frame` poll is a few milliseconds (the l2 providers'
//! study-08-F2 calibration), so the window of 2048 polls is seconds-scale, ARP every
//! 192 polls ≈ once a second, beacons every 96 polls ≈ twice a second. Approximate by
//! design; the counts line reports what actually happened.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::net::l2;
use eo9_guest::buffer;
use eo9_guest::text;

// Three typed `main` options lower to more core-glue arguments than clippy's
// budget; the WIT signature is the real interface (the telnetd precedent).
#[allow(clippy::too_many_arguments)]
mod bindings_glue {
    eo9_guest::bindings!({
        world: "l2check",
        apis: [io, net_l2, text],
    });
}
use bindings_glue::*;

/// The default target: the QEMU user-mode-networking gateway every slirp instance
/// answers ARP for.
const DEFAULT_GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// The default source: the address slirp hands its guest. On a real LAN pass the
/// machine's address with `--source` — the sender protocol address travels in every
/// ARP we emit, and an off-subnet sender is exactly what DAI-class switch filtering
/// drops (and what a cautious gateway declines to answer).
const DEFAULT_SOURCE: [u8; 4] = [10, 0, 2, 15];
/// Receive polls in the probe window. Each empty poll is a few milliseconds, so the
/// window is seconds-scale; the gateway reply normally lands in the first few polls.
const RECEIVE_ATTEMPTS: u32 = 2048;
/// Re-send the ARP request every this many polls (~once a second).
const ARP_RESEND_INTERVAL: u32 = 192;
/// Send a beacon every this many polls (~twice a second), when enabled.
const BEACON_INTERVAL: u32 = 96;
/// The beacon's UDP port (source and destination): listen with `nc -ul 19099`.
const BEACON_PORT: u16 = 19099;

/// The l2 API's own error, rendered into the world's failure variant.
fn net_failure(err: l2::L2Error) -> ProgramFailure {
    match err {
        l2::L2Error::Denied => ProgramFailure::Denied,
        other => ProgramFailure::Net(format!("{other:?}")),
    }
}

/// `aa:bb:cc:dd:ee:ff`.
fn format_mac(mac: &[u8]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// `a.b.c.d`.
fn format_ip(ip: &[u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

/// Parse a dotted quad (`--gateway 10.20.3.1`).
fn parse_ip(text: &str) -> Result<[u8; 4], ProgramFailure> {
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
    Ok(octets)
}

// ------------------------------------------------------------------------------------------
// Frame builders and parsers (plain RFC 826 / 791 / 768 layouts)
// ------------------------------------------------------------------------------------------

/// A broadcast ARP request: who-has `gateway`, tell `our_mac`/`source` (42 bytes).
fn arp_request(our_mac: &[u8; 6], source: &[u8; 4], gateway: &[u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(42);
    frame.extend_from_slice(&[0xff; 6]); // destination: broadcast
    frame.extend_from_slice(our_mac); // source
    frame.extend_from_slice(&[0x08, 0x06]); // ethertype: ARP
    frame.extend_from_slice(&[0x00, 0x01]); // htype: Ethernet
    frame.extend_from_slice(&[0x08, 0x00]); // ptype: IPv4
    frame.extend_from_slice(&[0x06, 0x04]); // hlen, plen
    frame.extend_from_slice(&[0x00, 0x01]); // oper: request
    frame.extend_from_slice(our_mac); // sender hardware address
    frame.extend_from_slice(source); // sender protocol address
    frame.extend_from_slice(&[0x00; 6]); // target hardware address: unknown
    frame.extend_from_slice(gateway); // target protocol address
    frame
}

/// An ARP reply answering `requester` (who asked for `source`): `source` is-at
/// `our_mac` (42 bytes, unicast to the requester).
fn arp_reply(
    our_mac: &[u8; 6],
    source: &[u8; 4],
    requester_mac: &[u8; 6],
    requester_ip: &[u8; 4],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(42);
    frame.extend_from_slice(requester_mac); // destination: the asker
    frame.extend_from_slice(our_mac); // source
    frame.extend_from_slice(&[0x08, 0x06]); // ethertype: ARP
    frame.extend_from_slice(&[0x00, 0x01]); // htype: Ethernet
    frame.extend_from_slice(&[0x08, 0x00]); // ptype: IPv4
    frame.extend_from_slice(&[0x06, 0x04]); // hlen, plen
    frame.extend_from_slice(&[0x00, 0x02]); // oper: reply
    frame.extend_from_slice(our_mac); // sender hardware address: us
    frame.extend_from_slice(source); // sender protocol address: us
    frame.extend_from_slice(requester_mac); // target hardware address
    frame.extend_from_slice(requester_ip); // target protocol address
    frame
}

/// If `frame` is an ARP reply from `gateway`, the gateway's MAC address.
fn arp_reply_from_gateway(frame: &[u8], gateway: &[u8; 4]) -> Option<[u8; 6]> {
    if frame.len() < 42 {
        return None;
    }
    if frame[12..14] != [0x08, 0x06] {
        return None; // not ARP
    }
    if frame[20..22] != [0x00, 0x02] {
        return None; // not a reply
    }
    if frame[28..32] != *gateway {
        return None; // someone else answering
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame[22..28]);
    Some(mac)
}

/// If `frame` is an ARP request asking who-has `source` (and not from us — our own
/// retransmissions loop back through some providers), the requester's (MAC, IP).
fn who_has_for_us(frame: &[u8], our_mac: &[u8; 6], source: &[u8; 4]) -> Option<([u8; 6], [u8; 4])> {
    if frame.len() < 42 {
        return None;
    }
    if frame[12..14] != [0x08, 0x06] {
        return None; // not ARP
    }
    if frame[20..22] != [0x00, 0x01] {
        return None; // not a request
    }
    if frame[38..42] != *source {
        return None; // asking about someone else
    }
    if frame[22..28] == *our_mac {
        return None; // our own request echoed back
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame[22..28]);
    let mut ip = [0u8; 4];
    ip.copy_from_slice(&frame[28..32]);
    Some((mac, ip))
}

/// The RFC 791 IPv4 header checksum: one's-complement sum of the header words.
fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut index = 0;
    while index + 1 < header.len() {
        sum += u32::from(u16::from_be_bytes([header[index], header[index + 1]]));
        index += 2;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The UDP broadcast beacon: Ethernet broadcast / IPv4 `source` → 255.255.255.255 /
/// UDP 19099 → 19099, payload `eo9-beacon <seq>`. Broadcast needs no ARP resolution,
/// which is the point: it probes the transmit path independently of ARP handling.
/// (UDP checksum 0 = "not computed", legal for IPv4 per RFC 768.)
fn beacon_frame(our_mac: &[u8; 6], source: &[u8; 4], sequence: u32) -> Vec<u8> {
    let payload = format!("eo9-beacon {sequence}\n");
    let udp_len = 8 + payload.len() as u16;
    let ip_len = 20 + udp_len;

    let mut ip_header = Vec::with_capacity(20);
    ip_header.extend_from_slice(&[0x45, 0x00]); // version 4, IHL 5, TOS 0
    ip_header.extend_from_slice(&ip_len.to_be_bytes());
    ip_header.extend_from_slice(&(sequence as u16).to_be_bytes()); // identification
    ip_header.extend_from_slice(&[0x00, 0x00]); // no flags, no fragment offset
    ip_header.extend_from_slice(&[64, 17]); // TTL 64, protocol UDP
    ip_header.extend_from_slice(&[0x00, 0x00]); // checksum placeholder
    ip_header.extend_from_slice(source);
    ip_header.extend_from_slice(&[255, 255, 255, 255]);
    let checksum = ipv4_checksum(&ip_header);
    ip_header[10..12].copy_from_slice(&checksum.to_be_bytes());

    let mut frame = Vec::with_capacity(14 + usize::from(ip_len));
    frame.extend_from_slice(&[0xff; 6]); // destination: broadcast
    frame.extend_from_slice(our_mac);
    frame.extend_from_slice(&[0x08, 0x00]); // ethertype: IPv4
    frame.extend_from_slice(&ip_header);
    frame.extend_from_slice(&BEACON_PORT.to_be_bytes()); // source port
    frame.extend_from_slice(&BEACON_PORT.to_be_bytes()); // destination port
    frame.extend_from_slice(&udp_len.to_be_bytes());
    frame.extend_from_slice(&[0x00, 0x00]); // UDP checksum: not computed
    frame.extend_from_slice(payload.as_bytes());
    frame
}

eo9_guest::main! {
    async fn main(
        gateway: Option<String>,
        source: Option<String>,
        beacon: Option<bool>,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));

        let gateway = match gateway {
            Some(text) => parse_ip(&text)?,
            None => DEFAULT_GATEWAY,
        };
        let source = match source {
            Some(text) => parse_ip(&text)?,
            None => DEFAULT_SOURCE,
        };
        let beacon = beacon.unwrap_or(false);

        let root = l2::default();
        let interfaces = l2::list_interfaces(&root).await.map_err(net_failure)?;
        let first = interfaces.first().ok_or_else(|| {
            ProgramFailure::Net(String::from("the l2 capability exposes no interfaces"))
        })?;
        let (a, b, c, d, e, f) = first.mac;
        let our_mac = [a, b, c, d, e, f];
        text::write_out_line(&format!(
            "l2check: interface {} ({}, mtu {}), probing {} as {}{}",
            first.name,
            format_mac(&our_mac),
            first.mtu,
            format_ip(&gateway),
            format_ip(&source),
            if beacon { ", beacon on" } else { "" },
        ))
        .map_err(io_failure)?;

        let iface = l2::open_interface(&root, first.name.clone())
            .await
            .map_err(net_failure)?;

        // Probe counters, reported in the final counts line on every path.
        let mut arp_sent: u32 = 0;
        let mut who_has_answered: u32 = 0;
        let mut beacons_sent: u32 = 0;
        // A send that fails mid-window does not end the run (one refused transmit
        // must not kill the probe), but the last error is kept for the report.
        let mut last_error = String::from("no frames were received");
        let mut resolved: Option<[u8; 6]> = None;

        // Send one frame, best effort: count on success, remember the error
        // otherwise (`denied` still aborts — composition said no).
        macro_rules! send_frame {
            ($bytes:expr, $counter:ident) => {{
                let (_buffer, sent) = l2::send_frame(&iface, buffer::from_bytes(&$bytes)).await;
                match sent {
                    Ok(_) => {
                        $counter += 1;
                    }
                    Err(l2::L2Error::Denied) => return Err(ProgramFailure::Denied),
                    Err(other) => {
                        last_error = format!("send: {other:?}");
                    }
                }
            }};
        }

        // The opening ARP request.
        send_frame!(arp_request(&our_mac, &source, &gateway), arp_sent);

        for poll in 0..RECEIVE_ATTEMPTS {
            // Retransmit + beacon pacing rides the poll cadence (module docs).
            if poll > 0 && poll % ARP_RESEND_INTERVAL == 0 {
                send_frame!(arp_request(&our_mac, &source, &gateway), arp_sent);
            }
            if beacon && poll % BEACON_INTERVAL == 0 {
                let frame = beacon_frame(&our_mac, &source, beacons_sent);
                send_frame!(frame, beacons_sent);
            }

            let dst = buffer::with_capacity(2048);
            let (dst, received) = l2::recv_frame(&iface, dst).await;
            match received {
                Ok(result) => {
                    if result.bytes_received == 0 {
                        continue; // nothing waiting yet; poll again (each poll is a few ms)
                    }
                    let frame = buffer::prefix_to_vec(&dst, result.bytes_received);
                    if let Some(gateway_mac) = arp_reply_from_gateway(&frame, &gateway) {
                        resolved = Some(gateway_mac);
                        break;
                    }
                    if let Some((requester_mac, requester_ip)) =
                        who_has_for_us(&frame, &our_mac, &source)
                    {
                        // Answer: `source` is-at our MAC — the peer-side measurement
                        // of whether our transmit crosses the switch.
                        send_frame!(
                            arp_reply(&our_mac, &source, &requester_mac, &requester_ip),
                            who_has_answered
                        );
                        continue;
                    }
                    last_error = format!(
                        "received {} byte(s) that were not the gateway's ARP reply",
                        result.bytes_received
                    );
                }
                Err(l2::L2Error::Denied) => return Err(ProgramFailure::Denied),
                Err(other) => {
                    last_error = format!("{other:?}");
                    break;
                }
            }
        }

        // The counts line, success or failure — every bench run must advance a round.
        text::write_out_line(&format!(
            "l2check: probes - arp sent {arp_sent}, who-has answered {who_has_answered}, \
             beacons sent {beacons_sent}",
        ))
        .map_err(io_failure)?;

        match resolved {
            Some(gateway_mac) => {
                let rendered = format_mac(&gateway_mac);
                text::write_out_line(&format!(
                    "l2check: {} is at {rendered}",
                    format_ip(&gateway)
                ))
                .map_err(io_failure)?;
                Ok(ProgramSuccess::Resolved(rendered))
            }
            None => Err(ProgramFailure::NoReply(last_error)),
        }
    }
}
