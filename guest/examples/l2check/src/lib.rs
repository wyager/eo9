//! l2check — the link-layer (eo9:net/l2) example program.
//!
//! Targets the `eo9-examples:l2check/l2check` world (see `wit/world.wit`): list the
//! granted l2 capability's interfaces, open the first one, broadcast an ARP request for
//! the QEMU user-net gateway (10.0.2.2), and wait for the reply. An answer proves whole
//! Ethernet frames move in both directions through the composed provider — `net.virtio`
//! driving real virtio hardware on metal, or any mock l2 that chooses to answer ARP.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::net::l2;
use eo9_guest::buffer;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "l2check",
    apis: [io, net_l2, text],
});

/// The QEMU user-mode-networking gateway every slirp instance answers ARP for.
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// The address slirp hands its guest; only used as the ARP sender protocol address.
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
/// How many received frames to inspect before giving up on a reply.
const RECEIVE_ATTEMPTS: u32 = 8;

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

/// A broadcast ARP request: who-has `GATEWAY`, tell `our_mac`/`OUR_IP` (42 bytes).
fn arp_request(our_mac: &[u8; 6]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(42);
    frame.extend_from_slice(&[0xff; 6]); // destination: broadcast
    frame.extend_from_slice(our_mac); // source
    frame.extend_from_slice(&[0x08, 0x06]); // ethertype: ARP
    frame.extend_from_slice(&[0x00, 0x01]); // htype: Ethernet
    frame.extend_from_slice(&[0x08, 0x00]); // ptype: IPv4
    frame.extend_from_slice(&[0x06, 0x04]); // hlen, plen
    frame.extend_from_slice(&[0x00, 0x01]); // oper: request
    frame.extend_from_slice(our_mac); // sender hardware address
    frame.extend_from_slice(&OUR_IP); // sender protocol address
    frame.extend_from_slice(&[0x00; 6]); // target hardware address: unknown
    frame.extend_from_slice(&GATEWAY); // target protocol address
    frame
}

/// If `frame` is an ARP reply from the gateway, the gateway's MAC address.
fn arp_reply_from_gateway(frame: &[u8]) -> Option<[u8; 6]> {
    if frame.len() < 42 {
        return None;
    }
    if frame[12..14] != [0x08, 0x06] {
        return None; // not ARP
    }
    if frame[20..22] != [0x00, 0x02] {
        return None; // not a reply
    }
    if frame[28..32] != GATEWAY {
        return None; // someone else answering
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame[22..28]);
    Some(mac)
}

eo9_guest::main! {
    async fn main() -> Result<ProgramSuccess, ProgramFailure> {
        let io_failure = |err: text::TextError| ProgramFailure::Io(format!("{err:?}"));

        let root = l2::default();
        let interfaces = l2::list_interfaces(&root).await.map_err(net_failure)?;
        let first = interfaces.first().ok_or_else(|| {
            ProgramFailure::Net(String::from("the l2 capability exposes no interfaces"))
        })?;
        let (a, b, c, d, e, f) = first.mac;
        let our_mac = [a, b, c, d, e, f];
        text::write_out_line(&format!(
            "l2check: interface {} ({}, mtu {})",
            first.name,
            format_mac(&our_mac),
            first.mtu,
        ))
        .map_err(io_failure)?;

        let iface = l2::open_interface(&root, first.name.clone())
            .await
            .map_err(net_failure)?;

        // Ask who has the gateway address.
        let request = buffer::from_bytes(&arp_request(&our_mac));
        let (_request, sent) = l2::send_frame(&iface, request).await;
        sent.map_err(net_failure)?;

        // Inspect the next few delivered frames for the reply.
        let mut last_error = String::from("no frames were received");
        for _ in 0..RECEIVE_ATTEMPTS {
            let dst = buffer::with_capacity(2048);
            let (dst, received) = l2::recv_frame(&iface, dst).await;
            match received {
                Ok(result) => {
                    let frame = buffer::prefix_to_vec(&dst, result.bytes_received);
                    if let Some(gateway_mac) = arp_reply_from_gateway(&frame) {
                        let rendered = format_mac(&gateway_mac);
                        text::write_out_line(&format!(
                            "l2check: 10.0.2.2 is at {rendered}"
                        ))
                        .map_err(io_failure)?;
                        return Ok(ProgramSuccess::Resolved(rendered));
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
        Err(ProgramFailure::NoReply(last_error))
    }
}
