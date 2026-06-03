//! vnicheck — the virtual-NIC switch example program.
//!
//! Targets the `eo9-examples:vnicheck/vnicheck` world (see `wit/world.wit`): two
//! `eo9:net/l2` capabilities under the named slots `link-a` / `link-b` — the two ports
//! of one `net.l2.switch` — driven over an upstream `net.l2.echo` fixture. Verifies
//! everything a consumer can observe of the switching policy:
//!
//! 1. each port exposes exactly one interface with its own locally-administered MAC,
//!    and the two MACs differ;
//! 2. a frame sent through port A reaches the upstream with A's virtual MAC as its
//!    Ethernet source (the switch's source rewrite), and the reflected reply is
//!    delivered to A alone — port B never sees it (demux by destination, sibling
//!    isolation);
//! 3. a broadcast reply from the upstream is delivered to BOTH ports;
//! 4. a reply addressed to a MAC no port owns is delivered to NEITHER port (unknown
//!    unicast is dropped, never flooded).
//!
//! Modes: `echo` (the full two-port suite over the echo fixture), `through` (the
//! point-to-point form for stacked switches — everything except port B's own
//! exchange), `arp` (both ports ARP a real gateway), `arp-a` (port A alone — the
//! stacked point-to-point form on a real link).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

wit_bindgen::generate!({
    world: "vnicheck",
    path: "wit",
    with: {
        "eo9:io/buffers@0.1.0": eo9_guest::api::io::buffers,
        "eo9:text/types@0.1.0": eo9_guest::api::text::types,
        "eo9:text/text@0.1.0": eo9_guest::api::text::text,
    },
    generate_all,
});

use eo9_guest::buffer;

/// The delivery-probe ethertypes the `net.l2.echo` fixture answers (see its docs).
const PROBE_BROADCAST: u16 = 0xb0b0;
const PROBE_UNKNOWN: u16 = 0xb0b1;
/// An ethertype with no probe behavior: the fixture reflects it (source/dest swapped).
const PROBE_REFLECT: u16 = 0xb0b2;

/// Receive polls per expected frame (the fixture answers synchronously; a couple of
/// polls is plenty) and per must-stay-empty check.
const POLL_ATTEMPTS: u32 = 8;

type Mac = (u8, u8, u8, u8, u8, u8);

fn mac_bytes(mac: Mac) -> [u8; 6] {
    [mac.0, mac.1, mac.2, mac.3, mac.4, mac.5]
}

fn mac_text(mac: Mac) -> String {
    let m = mac_bytes(mac);
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// Build an Ethernet frame.
fn frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.extend_from_slice(&dst);
    out.extend_from_slice(&src);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// What one received frame looks like to the checks.
struct Received {
    dst: [u8; 6],
    ethertype: u16,
    payload: Vec<u8>,
}

fn parse(bytes: &[u8]) -> Option<Received> {
    if bytes.len() < 14 {
        return None;
    }
    Some(Received {
        dst: bytes[0..6].try_into().expect("six bytes"),
        ethertype: u16::from_be_bytes([bytes[12], bytes[13]]),
        payload: Vec::from(&bytes[14..]),
    })
}

/// The per-port plumbing, written once and instantiated for each named slot (each slot
/// mints its own nominal types, so this is a macro rather than a generic).
macro_rules! port_driver {
    ($module:ident, $open:ident, $send:ident, $recv_one:ident, $expect_empty:ident) => {
        /// Open the slot's single interface; returns (interface, info).
        async fn $open() -> Result<($module::L2Interface, $module::InterfaceInfo), ProgramFailure> {
            let root = $module::default();
            let interfaces = $module::list_interfaces(&root)
                .await
                .map_err(|err| net_failure(stringify!($module), format!("{err:?}"), &err))?;
            if interfaces.len() != 1 {
                return Err(ProgramFailure::Check(format!(
                    "{}: expected exactly one interface, got {}",
                    stringify!($module),
                    interfaces.len()
                )));
            }
            let info = interfaces.into_iter().next().expect("one interface");
            let iface = $module::open_interface(&root, info.name.clone())
                .await
                .map_err(|err| net_failure(stringify!($module), format!("{err:?}"), &err))?;
            Ok((iface, info))
        }

        /// Send one frame through the port.
        async fn $send(iface: &$module::L2Interface, bytes: &[u8]) -> Result<(), ProgramFailure> {
            let buf = buffer::from_bytes(bytes);
            let (_buf, sent) = $module::send_frame(iface, buf).await;
            sent.map_err(|err| net_failure(stringify!($module), format!("{err:?}"), &err))?;
            Ok(())
        }

        /// Receive one frame (bounded polls); `None` when nothing arrived.
        async fn $recv_one(
            iface: &$module::L2Interface,
        ) -> Result<Option<Received>, ProgramFailure> {
            for _ in 0..POLL_ATTEMPTS {
                let dst = buffer::with_capacity(2048);
                let (dst, received) = $module::recv_frame(iface, dst).await;
                let result = received
                    .map_err(|err| net_failure(stringify!($module), format!("{err:?}"), &err))?;
                if result.bytes_received > 0 {
                    let bytes = buffer::prefix_to_vec(&dst, result.bytes_received);
                    return Ok(parse(&bytes));
                }
            }
            Ok(None)
        }

        /// Assert nothing is waiting on the port.
        async fn $expect_empty(
            iface: &$module::L2Interface,
            what: &str,
        ) -> Result<(), ProgramFailure> {
            for _ in 0..POLL_ATTEMPTS {
                let dst = buffer::with_capacity(2048);
                let (_dst, received) = $module::recv_frame(iface, dst).await;
                let result = received
                    .map_err(|err| net_failure(stringify!($module), format!("{err:?}"), &err))?;
                if result.bytes_received > 0 {
                    return Err(ProgramFailure::Check(format!(
                        "{}: expected no frame ({what}), but one arrived ({} bytes)",
                        stringify!($module),
                        result.bytes_received
                    )));
                }
            }
            Ok(())
        }
    };
}

/// Map an l2 error (rendered) onto the failure variant, preserving refusals. The
/// `_err` parameter keeps the macro call sites uniform across the two slots' nominal
/// error types; refusal detection rides on the rendered text.
fn net_failure(slot: &str, rendered: String, _err: &impl core::fmt::Debug) -> ProgramFailure {
    if rendered == "Denied" {
        ProgramFailure::Denied
    } else {
        ProgramFailure::Net(format!("{slot}: {rendered}"))
    }
}

port_driver!(link_a, open_a, send_a, recv_a, expect_empty_a);
port_driver!(link_b, open_b, send_b, recv_b, expect_empty_b);

/// The gateway QEMU user-mode networking answers ARP for, and the per-port sender
/// IPs the `arp` mode claims (distinct, so the two exchanges are fully independent).
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const SENDER_IP_A: [u8; 4] = [10, 0, 2, 15];
const SENDER_IP_B: [u8; 4] = [10, 0, 2, 16];

/// An ARP request for the gateway, broadcast from `mac` claiming `sender_ip`.
fn arp_request(mac: [u8; 6], sender_ip: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(42);
    out.extend_from_slice(&[0xff; 6]); // destination: broadcast
    out.extend_from_slice(&mac); // source
    out.extend_from_slice(&[0x08, 0x06]); // ethertype: ARP
    out.extend_from_slice(&[0x00, 0x01]); // htype: Ethernet
    out.extend_from_slice(&[0x08, 0x00]); // ptype: IPv4
    out.extend_from_slice(&[0x06, 0x04]); // hlen, plen
    out.extend_from_slice(&[0x00, 0x01]); // oper: request
    out.extend_from_slice(&mac); // sender hardware address
    out.extend_from_slice(&sender_ip); // sender protocol address
    out.extend_from_slice(&[0x00; 6]); // target hardware address: unknown
    out.extend_from_slice(&GATEWAY_IP); // target protocol address
    out
}

/// If `received` is an ARP reply for the gateway, the gateway's MAC.
fn arp_reply_mac(received: &Received) -> Option<[u8; 6]> {
    if received.ethertype != 0x0806 || received.payload.len() < 28 {
        return None;
    }
    let arp = &received.payload;
    if arp[6..8] != [0x00, 0x02] || arp[14..18] != GATEWAY_IP {
        return None; // not a reply, or not from the gateway
    }
    Some(arp[8..14].try_into().expect("six bytes"))
}

eo9_guest::main! {
    async fn main(mode: String) -> Result<ProgramSuccess, ProgramFailure> {
        // --- the ports and their MACs -------------------------------------------------
        let (iface_a, info_a) = open_a().await?;
        let (iface_b, info_b) = open_b().await?;
        let mac_a = mac_bytes(info_a.mac);
        let mac_b = mac_bytes(info_b.mac);

        if mac_a == mac_b {
            return Err(ProgramFailure::Check(format!(
                "the two ports share a MAC: {}",
                mac_text(info_a.mac)
            )));
        }
        for (name, mac) in [("link-a", mac_a), ("link-b", mac_b)] {
            if mac[0] & 0x02 == 0 || mac[0] & 0x01 != 0 {
                return Err(ProgramFailure::Check(format!(
                    "{name}: the port MAC is not locally-administered unicast: {}",
                    mac_text((mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]))
                )));
            }
        }

        if mode == "arp" || mode == "arp-a" {
            // --- the real-link sharing proof: ARP-resolve the gateway through both
            // ports independently; each reply must come back unicast to its own
            // port's virtual MAC (the demux at work on a real link). `arp-a` runs the
            // port-A half alone — the point-to-point form for STACKED switches, where
            // the sibling's return path is the open fan-out question (the switch's
            // source rewrite has no reverse mapping; plan/09 D37).
            send_a(&iface_a, &arp_request(mac_a, SENDER_IP_A)).await?;
            let mut gw_a: Option<([u8; 6], [u8; 6])> = None;
            for _ in 0..POLL_ATTEMPTS {
                if let Some(received) = recv_a(&iface_a).await?
                    && let Some(gw) = arp_reply_mac(&received)
                {
                    gw_a = Some((gw, received.dst));
                    break;
                }
            }
            let (gw_a, dst_a) = gw_a.ok_or_else(|| {
                ProgramFailure::Check(String::from("link-a: no ARP reply from the gateway"))
            })?;
            if dst_a != mac_a {
                return Err(ProgramFailure::Check(format!(
                    "link-a: the gateway's reply was addressed to {}, not port A's virtual MAC",
                    mac_text((dst_a[0], dst_a[1], dst_a[2], dst_a[3], dst_a[4], dst_a[5]))
                )));
            }

            if mode == "arp-a" {
                return Ok(ProgramSuccess::Verified(format!(
                    "mac-a={} gw-a={}",
                    mac_text(info_a.mac),
                    mac_text((gw_a[0], gw_a[1], gw_a[2], gw_a[3], gw_a[4], gw_a[5])),
                )));
            }

            send_b(&iface_b, &arp_request(mac_b, SENDER_IP_B)).await?;
            let mut gw_b: Option<([u8; 6], [u8; 6])> = None;
            for _ in 0..POLL_ATTEMPTS {
                if let Some(received) = recv_b(&iface_b).await?
                    && let Some(gw) = arp_reply_mac(&received)
                {
                    gw_b = Some((gw, received.dst));
                    break;
                }
            }
            let (gw_b, dst_b) = gw_b.ok_or_else(|| {
                ProgramFailure::Check(String::from("link-b: no ARP reply from the gateway"))
            })?;
            if dst_b != mac_b {
                return Err(ProgramFailure::Check(format!(
                    "link-b: the gateway's reply was addressed to {}, not port B's virtual MAC",
                    mac_text((dst_b[0], dst_b[1], dst_b[2], dst_b[3], dst_b[4], dst_b[5]))
                )));
            }

            return Ok(ProgramSuccess::Verified(format!(
                "mac-a={} gw-a={} mac-b={} gw-b={}",
                mac_text(info_a.mac),
                mac_text((gw_a[0], gw_a[1], gw_a[2], gw_a[3], gw_a[4], gw_a[5])),
                mac_text(info_b.mac),
                mac_text((gw_b[0], gw_b[1], gw_b[2], gw_b[3], gw_b[4], gw_b[5])),
            )));
        }
        if mode != "echo" && mode != "through" {
            return Err(ProgramFailure::Check(format!("unknown mode {mode:?}")));
        }
        // `through` is the point-to-point form of `echo` for STACKED switches: the full
        // port-A suite (reflect + source-rewrite proof + sibling isolation), broadcast
        // fan-out to both ports, and unknown-unicast to neither — everything except
        // port B's own exchange, whose return path through a stack is the open
        // fan-out/reverse-mapping question (plan/09 D37).

        // --- 1. unicast reflect: source rewrite + delivery to A alone ------------------
        let marker = b"vnic-unicast";
        let probe = frame([0x02, 0, 0, 0, 0, 0x99], mac_a, PROBE_REFLECT, marker);
        send_a(&iface_a, &probe).await?;
        let reply = recv_a(&iface_a)
            .await?
            .ok_or_else(|| ProgramFailure::Check(String::from(
                "link-a: the reflected unicast never arrived",
            )))?;
        if reply.ethertype != PROBE_REFLECT || reply.dst != mac_a {
            return Err(ProgramFailure::Check(format!(
                "link-a: unexpected reflected frame (ethertype {:#06x}, dst {})",
                reply.ethertype,
                mac_text((reply.dst[0], reply.dst[1], reply.dst[2], reply.dst[3], reply.dst[4], reply.dst[5])),
            )));
        }
        // The fixture's payload convention: the source MAC it saw, then the payload.
        if reply.payload.len() < 6 + marker.len() || &reply.payload[6..6 + marker.len()] != marker {
            return Err(ProgramFailure::Check(String::from(
                "link-a: the reflected payload is malformed",
            )));
        }
        if reply.payload[0..6] != mac_a {
            return Err(ProgramFailure::Check(format!(
                "the upstream saw source {} instead of port A's virtual MAC {} — the switch did not rewrite the source",
                mac_text((reply.payload[0], reply.payload[1], reply.payload[2], reply.payload[3], reply.payload[4], reply.payload[5])),
                mac_text(info_a.mac),
            )));
        }
        // Port B saw none of that exchange.
        expect_empty_b(&iface_b, "A's unicast must not reach B").await?;

        // The mirror image: B's own exchange works and stays invisible to A. (Skipped
        // in `through` mode — through a stack, B's replies come back addressed to the
        // inner layer's port MAC, which only port A's derivation matches.)
        if mode == "echo" {
            let marker_b = b"vnic-unicast-b";
            send_b(&iface_b, &frame([0x02, 0, 0, 0, 0, 0x99], mac_b, PROBE_REFLECT, marker_b)).await?;
            let reply_b = recv_b(&iface_b)
                .await?
                .ok_or_else(|| ProgramFailure::Check(String::from(
                    "link-b: the reflected unicast never arrived",
                )))?;
            if reply_b.dst != mac_b || reply_b.payload[0..6] != mac_b {
                return Err(ProgramFailure::Check(String::from(
                    "link-b: the reflected frame does not carry port B's virtual MAC",
                )));
            }
            expect_empty_a(&iface_a, "B's unicast must not reach A").await?;
        }

        // --- 2. broadcast: delivered to both ports -------------------------------------
        send_a(&iface_a, &frame([0x02, 0, 0, 0, 0, 0x99], mac_a, PROBE_BROADCAST, b"vnic-broadcast")).await?;
        let on_a = recv_a(&iface_a).await?.ok_or_else(|| {
            ProgramFailure::Check(String::from("link-a: the broadcast never arrived"))
        })?;
        let on_b = recv_b(&iface_b).await?.ok_or_else(|| {
            ProgramFailure::Check(String::from(
                "link-b: the broadcast was not delivered to the sibling port",
            ))
        })?;
        if on_a.dst != [0xff; 6] || on_b.dst != [0xff; 6] {
            return Err(ProgramFailure::Check(String::from(
                "the broadcast frames arrived with a non-broadcast destination",
            )));
        }

        // --- 3. unknown unicast: delivered to neither port ------------------------------
        send_a(&iface_a, &frame([0x02, 0, 0, 0, 0, 0x99], mac_a, PROBE_UNKNOWN, b"vnic-unknown")).await?;
        expect_empty_a(&iface_a, "unknown unicast must not reach A").await?;
        expect_empty_b(&iface_b, "unknown unicast must not reach B").await?;

        Ok(ProgramSuccess::Verified(format!(
            "mac-a={} mac-b={}",
            mac_text(info_a.mac),
            mac_text(info_b.mac)
        )))
    }
}
