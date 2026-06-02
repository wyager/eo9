//! `net.l2.echo` — the frame-reflector test fixture (see the `l2-echo` world in
//! `wit/net/net.wit`).
//!
//! A self-contained `eo9:net/l2` with one interface (`echo0`, MAC `02:e0:09:ec:00:01`)
//! that answers every sent frame deterministically, so link-layer plumbing — the
//! virtual-NIC switch above all — is testable with no real network:
//!
//! * **ARP request** (ethertype `0x0806`, opcode 1): answered with a proper ARP reply —
//!   the fixture owns every IP it is asked about, answering with its own MAC. A whole
//!   TCP/IP stack above can therefore resolve its gateway.
//! * **IPv4/UDP** (ethertype `0x0800`, protocol 17): echoed back with Ethernet,
//!   IP, and UDP source/destination swapped; the UDP checksum is zeroed (legal for
//!   IPv4: "no checksum") and the IPv4 header checksum recomputed.
//! * **`0xb0b0`** (delivery probe): answered with a *broadcast* frame.
//! * **`0xb0b1`** (delivery probe): answered to a fixed unknown unicast MAC
//!   (`02:e0:09:ee:ee:ee`) that no port owns.
//! * **anything else**: reflected with source and destination MACs swapped.
//!
//! Every reply's payload starts with the six bytes of the original frame's Ethernet
//! *source* address as the fixture saw it — so a test above a switch can verify which
//! source MAC actually reached the upstream link — followed by the original payload.
//! Replies are queued (bounded) and served by `recv-frame`; an empty queue returns
//! `bytes-received: 0` (nothing waiting).

#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "l2-echo",
    path: "../../../wit/net",
    // Pull in bindings for eo9:io/buffers, which the exported l2 interface uses but
    // the world does not name directly.
    generate_all,
});

use exports::eo9::net::l2::{self, Buffer, InterfaceInfo, L2Error, RecvResult, SendResult};

/// The fixture's own MAC address.
const ECHO_MAC: [u8; 6] = [0x02, 0xe0, 0x09, 0xec, 0x00, 0x01];
/// The unknown unicast destination used by the `0xb0b1` probe.
const UNKNOWN_MAC: [u8; 6] = [0x02, 0xe0, 0x09, 0xee, 0xee, 0xee];
/// The Ethernet broadcast address.
const BROADCAST: [u8; 6] = [0xff; 6];
/// Replies the fixture holds before the oldest is dropped.
const QUEUE_CAP: usize = 64;

/// The delivery-probe ethertypes.
const PROBE_BROADCAST: u16 = 0xb0b0;
const PROBE_UNKNOWN: u16 = 0xb0b1;

struct EchoState {
    pending: VecDeque<Vec<u8>>,
}

static STATE: ProviderState<EchoState> = ProviderState::new();

fn with_state<R>(f: impl FnOnce(&mut EchoState) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(EchoState {
            pending: VecDeque::new(),
        });
    }
    STATE.with(f)
}

fn enqueue(frame: Vec<u8>) {
    with_state(|state| {
        if state.pending.len() == QUEUE_CAP {
            state.pending.pop_front();
        }
        state.pending.push_back(frame);
    });
}

/// Build an Ethernet frame: destination, source, ethertype, then the payload — which
/// by fixture convention starts with the original frame's source MAC.
fn frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, seen_src: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + seen_src.len() + payload.len());
    out.extend_from_slice(&dst);
    out.extend_from_slice(&src);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(seen_src);
    out.extend_from_slice(payload);
    out
}

/// The IPv4 header checksum: 16-bit ones'-complement sum over the header with the
/// checksum field zeroed.
fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut at = 0;
    while at + 1 < header.len() {
        let word = if at == 10 {
            0 // the checksum field itself counts as zero
        } else {
            u16::from_be_bytes([header[at], header[at + 1]]) as u32
        };
        sum += word;
        at += 2;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Answer one sent frame per the behavior table. `bytes` is the full Ethernet frame.
fn answer(bytes: &[u8]) {
    if bytes.len() < 14 {
        return;
    }
    let src: [u8; 6] = bytes[6..12].try_into().expect("six bytes");
    let ethertype = u16::from_be_bytes([bytes[12], bytes[13]]);
    let payload = &bytes[14..];

    match ethertype {
        // ARP request -> proper ARP reply (the fixture owns every IP).
        0x0806 if payload.len() >= 28 => {
            let oper = u16::from_be_bytes([payload[6], payload[7]]);
            if oper != 1 {
                return; // only requests are answered
            }
            let sender_hw = &payload[8..14];
            let sender_ip = &payload[14..18];
            let target_ip = &payload[24..28];
            let mut arp = Vec::with_capacity(28);
            arp.extend_from_slice(&payload[0..6]); // htype, ptype, hlen, plen
            arp.extend_from_slice(&2u16.to_be_bytes()); // oper: reply
            arp.extend_from_slice(&ECHO_MAC); // sha: the fixture
            arp.extend_from_slice(target_ip); // spa: the IP that was asked about
            arp.extend_from_slice(sender_hw); // tha: the asker
            arp.extend_from_slice(sender_ip); // tpa: the asker's IP
            // ARP replies do not carry the seen-src prefix: stacks parse them.
            let mut out = Vec::with_capacity(14 + 28);
            out.extend_from_slice(&src);
            out.extend_from_slice(&ECHO_MAC);
            out.extend_from_slice(&0x0806u16.to_be_bytes());
            out.extend_from_slice(&arp);
            enqueue(out);
        }
        // IPv4/UDP -> echo with addresses and ports swapped (no seen-src prefix:
        // stacks parse these).
        0x0800 if payload.len() >= 20 => {
            let ihl = ((payload[0] & 0x0f) as usize) * 4;
            if payload.len() < ihl + 8 || payload[9] != 17 {
                return; // not UDP (or truncated)
            }
            let mut ip = Vec::from(payload);
            // Swap IPv4 source (12..16) and destination (16..20).
            for index in 0..4 {
                ip.swap(12 + index, 16 + index);
            }
            // Swap UDP source and destination ports.
            for index in 0..2 {
                ip.swap(ihl + index, ihl + 2 + index);
            }
            // Zero the UDP checksum (legal for IPv4) and recompute the IP checksum.
            ip[ihl + 6] = 0;
            ip[ihl + 7] = 0;
            let checksum = ipv4_checksum(&ip[0..ihl]);
            ip[10..12].copy_from_slice(&checksum.to_be_bytes());
            let mut out = Vec::with_capacity(14 + ip.len());
            out.extend_from_slice(&src);
            out.extend_from_slice(&ECHO_MAC);
            out.extend_from_slice(&0x0800u16.to_be_bytes());
            out.extend_from_slice(&ip);
            enqueue(out);
        }
        // Delivery probe: answer with a broadcast.
        PROBE_BROADCAST => {
            enqueue(frame(BROADCAST, ECHO_MAC, PROBE_BROADCAST, &src, payload));
        }
        // Delivery probe: answer to a fixed unknown unicast MAC.
        PROBE_UNKNOWN => {
            enqueue(frame(UNKNOWN_MAC, ECHO_MAC, PROBE_UNKNOWN, &src, payload));
        }
        // Anything else: reflect with source and destination swapped.
        _ => {
            let dst: [u8; 6] = bytes[0..6].try_into().expect("six bytes");
            enqueue(frame(src, dst, ethertype, &src, payload));
        }
    }
}

struct Stub;

/// The root-handle resource: a token — there is no state behind it.
struct EchoL2;

/// The opened interface: likewise a token (there is exactly one).
struct EchoIface;

impl l2::GuestL2Impl for EchoL2 {}
impl l2::GuestL2Interface for EchoIface {}

impl l2::Guest for Stub {
    type L2Impl = EchoL2;
    type L2Interface = EchoIface;

    fn default() -> l2::L2Impl {
        l2::L2Impl::new(EchoL2)
    }

    async fn list_interfaces(_l2: l2::L2ImplBorrow<'_>) -> Result<Vec<InterfaceInfo>, L2Error> {
        Ok(alloc::vec![InterfaceInfo {
            name: String::from("echo0"),
            mac: (
                ECHO_MAC[0],
                ECHO_MAC[1],
                ECHO_MAC[2],
                ECHO_MAC[3],
                ECHO_MAC[4],
                ECHO_MAC[5]
            ),
            mtu: 1500,
            up: true,
        }])
    }

    async fn open_interface(
        _l2: l2::L2ImplBorrow<'_>,
        name: String,
    ) -> Result<l2::L2Interface, L2Error> {
        if name == "echo0" {
            Ok(l2::L2Interface::new(EchoIface))
        } else {
            Err(L2Error::NoSuchInterface)
        }
    }

    fn info(_iface: l2::L2InterfaceBorrow<'_>) -> InterfaceInfo {
        InterfaceInfo {
            name: String::from("echo0"),
            mac: (
                ECHO_MAC[0],
                ECHO_MAC[1],
                ECHO_MAC[2],
                ECHO_MAC[3],
                ECHO_MAC[4],
                ECHO_MAC[5],
            ),
            mtu: 1500,
            up: true,
        }
    }

    async fn send_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        frame: Buffer,
    ) -> (Buffer, Result<SendResult, L2Error>) {
        let len = frame.len();
        let bytes = frame.read(0, len);
        answer(&bytes);
        (frame, Ok(SendResult { bytes_sent: len }))
    }

    async fn recv_frame(
        _iface: l2::L2InterfaceBorrow<'_>,
        dst: Buffer,
    ) -> (Buffer, Result<RecvResult, L2Error>) {
        match with_state(|state| state.pending.pop_front()) {
            Some(reply) => {
                let take = (reply.len() as u64).min(dst.len());
                dst.write(0, &reply[..take as usize]);
                (
                    dst,
                    Ok(RecvResult {
                        bytes_received: take,
                    }),
                )
            }
            None => (dst, Ok(RecvResult { bytes_received: 0 })),
        }
    }
}

export!(Stub);
