//! bridgecheck — the 802.1D learning-bridge example program.
//!
//! Targets the `eo9-examples:bridgecheck/bridgecheck` world: two `eo9:net/l2`
//! capabilities under the named slots `link-a` / `link-b` — the two ports of one
//! `net.l2.bridge` (or the OUTER bridge of a stacked pair) — driven over an upstream
//! `net.l2.echo` fixture. Verifies everything a consumer can observe of the bridging
//! policy, deliberately mirroring `vnicheck`'s structure so the two providers'
//! contracts read side by side:
//!
//! 1. each port advertises its own locally-administered suggestion MAC (distinct);
//!    the consumers here deliberately source from CUSTOM MACs instead — the bridge
//!    must carry them unrewritten (the no-rewrite property that defines it);
//! 2. an unknown unicast destination is FLOODED (sibling + upstream) before learning,
//!    and forwarded one-way only after (the reply demuxes to the learned port alone);
//! 3. a destination learned on the sibling port is delivered LOCALLY — the upstream
//!    never sees the frame;
//! 4. broadcast goes to every other port (sibling + upstream), and an upstream reply
//!    addressed to a MAC nobody sourced is flooded to BOTH ports (the switch drops
//!    these — the behavioral line between the two providers);
//! 5. a MAC migrating between ports follows its last sighting (and migrates back);
//! 6. (`evict`/`keep` modes) the learning table holds [`64`] entries with
//!    least-recently-learned eviction: the 65th distinct source pushes the oldest
//!    entry out — observable because the probe addressed to it floods to the
//!    upstream — while 64 sources fit, keeping the probe local.
//!
//! Modes: `learn` (the full suite, items 1–5 — also runs unchanged over a STACKED
//! bridge pair: the fan-out story), `evict`, `keep`.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

wit_bindgen::generate!({
    world: "bridgecheck",
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

/// Receive polls per expected frame and per must-stay-empty check.
const POLL_ATTEMPTS: u32 = 8;

/// The bridge's learning-table capacity (mirrors the provider's documented bound).
const LEARN_CAP: usize = 64;

/// The custom source MACs the two consumers claim (locally-administered unicast,
/// distinct from anything the bridge advertises — the no-rewrite proof rides on the
/// upstream seeing exactly these).
const CUSTOM_A: [u8; 6] = [0x02, 0xbc, 0x00, 0x00, 0x00, 0x0a];
const CUSTOM_B: [u8; 6] = [0x02, 0xbc, 0x00, 0x00, 0x00, 0x0b];
/// A unicast destination nobody owns (the flood/learning probe target).
const UNKNOWN_DST: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x99];
/// The fixed unknown unicast MAC the echo fixture's `0xb0b1` probe replies to.
const ECHO_UNKNOWN_MAC: [u8; 6] = [0x02, 0xe0, 0x09, 0xee, 0xee, 0xee];
/// The Ethernet broadcast address.
const BROADCAST: [u8; 6] = [0xff; 6];

type Mac = (u8, u8, u8, u8, u8, u8);

fn mac_bytes(mac: Mac) -> [u8; 6] {
    [mac.0, mac.1, mac.2, mac.3, mac.4, mac.5]
}

fn mac_text_bytes(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
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
    src: [u8; 6],
    ethertype: u16,
    payload: Vec<u8>,
}

fn parse(bytes: &[u8]) -> Option<Received> {
    if bytes.len() < 14 {
        return None;
    }
    Some(Received {
        dst: bytes[0..6].try_into().expect("six bytes"),
        src: bytes[6..12].try_into().expect("six bytes"),
        ethertype: u16::from_be_bytes([bytes[12], bytes[13]]),
        payload: Vec::from(&bytes[14..]),
    })
}

/// The per-port plumbing, written once per named slot (each slot mints its own
/// nominal types, so this is a macro rather than a generic) — vnicheck's pattern.
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

/// Map an l2 error (rendered) onto the failure variant, preserving refusals.
fn net_failure(slot: &str, rendered: String, _err: &impl core::fmt::Debug) -> ProgramFailure {
    if rendered == "Denied" {
        ProgramFailure::Denied
    } else {
        ProgramFailure::Net(format!("{slot}: {rendered}"))
    }
}

port_driver!(link_a, open_a, send_a, recv_a, expect_empty_a);
port_driver!(link_b, open_b, send_b, recv_b, expect_empty_b);

fn check(condition: bool, message: &str) -> Result<(), ProgramFailure> {
    if condition {
        Ok(())
    } else {
        Err(ProgramFailure::Check(String::from(message)))
    }
}

/// Receive one frame or fail with a located message.
macro_rules! must_recv {
    ($recv:ident, $iface:expr, $what:expr) => {
        $recv($iface).await?.ok_or_else(|| {
            ProgramFailure::Check(format!("{}: {} never arrived", stringify!($recv), $what))
        })?
    };
}

eo9_guest::main! {
    async fn main(mode: String) -> Result<ProgramSuccess, ProgramFailure> {
        // --- the ports and their advertised (suggestion) MACs --------------------------
        let (iface_a, info_a) = open_a().await?;
        let (iface_b, info_b) = open_b().await?;
        let adv_a = mac_bytes(info_a.mac);
        let adv_b = mac_bytes(info_b.mac);

        if adv_a == adv_b {
            return Err(ProgramFailure::Check(format!(
                "the two ports advertise the same MAC: {}",
                mac_text_bytes(adv_a)
            )));
        }
        for (name, mac) in [("link-a", adv_a), ("link-b", adv_b)] {
            if mac[0] & 0x02 == 0 || mac[0] & 0x01 != 0 {
                return Err(ProgramFailure::Check(format!(
                    "{name}: the advertised MAC is not locally-administered unicast: {}",
                    mac_text_bytes(mac)
                )));
            }
        }

        match mode.as_str() {
            "learn" => learn_suite(&iface_a, &iface_b, adv_a, adv_b).await,
            "evict" => table_suite(&iface_a, &iface_b, true).await,
            "keep" => table_suite(&iface_a, &iface_b, false).await,
            other => Err(ProgramFailure::Check(format!("unknown mode {other:?}"))),
        }
    }
}

/// Items 1–5: the full 802.1D suite over the echo fixture (single bridge or a
/// stacked pair — the assertions are identical).
async fn learn_suite(
    iface_a: &link_a::L2Interface,
    iface_b: &link_b::L2Interface,
    adv_a: [u8; 6],
    adv_b: [u8; 6],
) -> Result<ProgramSuccess, ProgramFailure> {
    // --- 1+2a. flood before learning, no rewrite, learned reply demux ---------------
    // A sends from its CUSTOM MAC to a destination nobody has sourced from: the
    // bridge must flood it (sibling + upstream) and must NOT touch the source.
    let marker_a = b"bridge-probe-a";
    send_a(
        iface_a,
        &frame(UNKNOWN_DST, CUSTOM_A, PROBE_REFLECT, marker_a),
    )
    .await?;

    // The reflected reply (echo swaps src/dst, prefixes the source it saw): it is
    // addressed to CUSTOM_A — learned on A from the send — so it reaches A alone.
    let reply = must_recv!(recv_a, iface_a, "the reflected unicast");
    check(
        reply.ethertype == PROBE_REFLECT && reply.dst == CUSTOM_A,
        "link-a: the reflected reply must be addressed to the custom source MAC",
    )?;
    check(
        reply.payload.len() >= 6 + marker_a.len()
            && &reply.payload[6..6 + marker_a.len()] == marker_a,
        "link-a: the reflected payload is malformed",
    )?;
    if reply.payload[0..6] != CUSTOM_A {
        return Err(ProgramFailure::Check(format!(
            "the upstream saw source {} instead of the consumer's custom MAC {} — the bridge \
             must not rewrite",
            mac_text_bytes(reply.payload[0..6].try_into().expect("six bytes")),
            mac_text_bytes(CUSTOM_A),
        )));
    }
    // The sibling holds the FLOODED COPY of the outbound probe (unknown unicast at
    // send time): dst still the unknown target, source still CUSTOM_A, raw payload.
    let flooded = must_recv!(recv_b, iface_b, "the flooded copy of A's probe");
    check(
        flooded.dst == UNKNOWN_DST && flooded.src == CUSTOM_A && flooded.payload == marker_a,
        "link-b: the flooded copy must be A's outbound frame, verbatim",
    )?;

    // --- 2b. after learning: known unicast goes one way only ------------------------
    // The probe destination is now learned on the upstream (the reply's source), so
    // B's identical probe is forwarded upstream ONLY — no sibling copy this time —
    // and B's reply demuxes to B. Through a STACKED pair this is the fan-out payoff:
    // both consumers complete custom-MAC exchanges (the switch's MAC-NAT cannot).
    let marker_b = b"bridge-probe-b";
    send_b(
        iface_b,
        &frame(UNKNOWN_DST, CUSTOM_B, PROBE_REFLECT, marker_b),
    )
    .await?;
    let reply_b = must_recv!(recv_b, iface_b, "the reflected unicast");
    check(
        reply_b.dst == CUSTOM_B && reply_b.payload.len() >= 6 && reply_b.payload[0..6] == CUSTOM_B,
        "link-b: the reply must carry B's custom MAC unrewritten and demux to B",
    )?;
    expect_empty_a(
        iface_a,
        "a learned-unicast probe must not flood to the sibling",
    )
    .await?;

    // --- 3. local port-to-port delivery (the upstream never sees it) ----------------
    let marker_local = b"bridge-local";
    send_a(
        iface_a,
        &frame(CUSTOM_B, CUSTOM_A, PROBE_REFLECT, marker_local),
    )
    .await?;
    let local = must_recv!(recv_b, iface_b, "the locally-bridged frame");
    check(
        local.dst == CUSTOM_B && local.src == CUSTOM_A && local.payload == marker_local,
        "link-b: the locally-bridged frame must arrive verbatim",
    )?;
    // No reflection ever arrives: the echo fixture answers everything it receives,
    // so silence on A proves the local frame never reached the upstream.
    expect_empty_a(
        iface_a,
        "a locally-bridged frame must not reach the upstream",
    )
    .await?;

    // --- 4a. broadcast: every other port (sibling + upstream) -----------------------
    let marker_bcast = b"bridge-bcast";
    send_a(
        iface_a,
        &frame(BROADCAST, CUSTOM_A, PROBE_BROADCAST, marker_bcast),
    )
    .await?;
    let bcast_a = must_recv!(recv_a, iface_a, "the broadcast reply");
    check(
        bcast_a.dst == BROADCAST && bcast_a.ethertype == PROBE_BROADCAST,
        "link-a: the upstream's broadcast reply must arrive as a broadcast",
    )?;
    let bcast_b_copy = must_recv!(recv_b, iface_b, "the flooded broadcast copy");
    check(
        bcast_b_copy.dst == BROADCAST && bcast_b_copy.src == CUSTOM_A,
        "link-b: the sibling must receive A's outbound broadcast verbatim",
    )?;
    let bcast_b_reply = must_recv!(recv_b, iface_b, "the broadcast reply");
    check(
        bcast_b_reply.dst == BROADCAST
            && bcast_b_reply.payload.len() >= 6
            && bcast_b_reply.payload[0..6] == CUSTOM_A,
        "link-b: the upstream's broadcast reply must reach the sibling too",
    )?;

    // --- 4b. unknown unicast from the upstream: FLOODED to both ports ----------------
    // (The behavioral line between bridge and switch: vnicheck asserts the switch
    // delivers this to NEITHER port; the bridge floods it to BOTH.)
    send_a(
        iface_a,
        &frame(UNKNOWN_DST, CUSTOM_A, PROBE_UNKNOWN, b"bridge-unknown"),
    )
    .await?;
    let unknown_a = must_recv!(recv_a, iface_a, "the unknown-unicast flood");
    check(
        unknown_a.dst == ECHO_UNKNOWN_MAC && unknown_a.ethertype == PROBE_UNKNOWN,
        "link-a: the unknown-unicast reply must be flooded to A",
    )?;
    let unknown_b = must_recv!(recv_b, iface_b, "the unknown-unicast flood");
    check(
        unknown_b.dst == ECHO_UNKNOWN_MAC && unknown_b.ethertype == PROBE_UNKNOWN,
        "link-b: the unknown-unicast reply must be flooded to B",
    )?;

    // --- 5. MAC migration: the last sighting wins, both directions ------------------
    let marker_migrate = b"bridge-migrate";
    send_b(
        iface_b,
        &frame(UNKNOWN_DST, CUSTOM_A, PROBE_REFLECT, marker_migrate),
    )
    .await?;
    let migrated = must_recv!(recv_b, iface_b, "the migrated reply");
    check(
        migrated.dst == CUSTOM_A
            && migrated.payload.len() >= 6 + marker_migrate.len()
            && &migrated.payload[6..6 + marker_migrate.len()] == marker_migrate,
        "link-b: after migration the reply to the custom MAC must demux to B",
    )?;
    expect_empty_a(
        iface_a,
        "the migrated MAC's reply must not reach its old port",
    )
    .await?;

    let marker_return = b"bridge-return";
    send_a(
        iface_a,
        &frame(UNKNOWN_DST, CUSTOM_A, PROBE_REFLECT, marker_return),
    )
    .await?;
    let returned = must_recv!(recv_a, iface_a, "the re-migrated reply");
    check(
        returned.dst == CUSTOM_A
            && returned.payload.len() >= 6 + marker_return.len()
            && &returned.payload[6..6 + marker_return.len()] == marker_return,
        "link-a: after migrating back the reply must demux to A again",
    )?;
    expect_empty_b(iface_b, "the re-migrated MAC's reply must not reach B").await?;

    Ok(ProgramSuccess::Verified(format!(
        "mac-a={} mac-b={}",
        mac_text_bytes(adv_a),
        mac_text_bytes(adv_b)
    )))
}

/// The bounded learning table, both directions. `evict`: a target plus 63 fillers
/// fill the table; the 65th distinct source (B's probe) evicts the target, so the
/// probe addressed to it FLOODS — observable as the reflection reaching B and the
/// flooded copy reaching A. `keep` (the control): one fewer filler, the target
/// survives, the probe is delivered LOCALLY to A and the upstream stays silent.
async fn table_suite(
    iface_a: &link_a::L2Interface,
    iface_b: &link_b::L2Interface,
    evict: bool,
) -> Result<ProgramSuccess, ProgramFailure> {
    let target: [u8; 6] = [0x02, 0xbd, 0x00, 0x00, 0x00, 0x00];

    // Teach the target, then the fillers, all as SELF-ADDRESSED frames: learned and
    // then filtered (destination on the ingress port), so nothing is delivered
    // anywhere and no upstream traffic muddies the table.
    send_a(iface_a, &frame(target, target, PROBE_REFLECT, b"fill")).await?;
    let fillers = if evict { LEARN_CAP - 1 } else { LEARN_CAP - 2 };
    for index in 0..fillers {
        let filler: [u8; 6] = [0x02, 0xbd, 0x00, 0x00, 0x01, index as u8];
        send_a(iface_a, &frame(filler, filler, PROBE_REFLECT, b"fill")).await?;
    }

    // B's probe to the target is the next distinct source: in `evict` it is the 65th
    // (the table holds target + 63 fillers), so learning B's source pushes the
    // target out and the lookup floods; in `keep` it is the 64th, the target
    // survives, and the lookup delivers locally to A.
    let marker = if evict {
        b"evict-probe".as_slice()
    } else {
        b"keep-probe".as_slice()
    };
    send_b(iface_b, &frame(target, CUSTOM_B, PROBE_REFLECT, marker)).await?;

    if evict {
        let reflected = must_recv!(recv_b, iface_b, "the evicted-probe reflection");
        check(
            reflected.dst == CUSTOM_B
                && reflected.payload.len() >= 6 + marker.len()
                && reflected.payload[0..6] == CUSTOM_B
                && &reflected.payload[6..6 + marker.len()] == marker,
            "link-b: the probe must have flooded to the upstream (the target was evicted)",
        )?;
        let flooded = must_recv!(recv_a, iface_a, "the flooded probe copy");
        check(
            flooded.dst == target && flooded.src == CUSTOM_B && flooded.payload == marker,
            "link-a: the flooded probe copy must arrive verbatim",
        )?;
        Ok(ProgramSuccess::Verified(String::from("evicted")))
    } else {
        let local = must_recv!(recv_a, iface_a, "the locally-delivered probe");
        check(
            local.dst == target && local.src == CUSTOM_B && local.payload == marker,
            "link-a: the probe must be delivered locally (the target was retained)",
        )?;
        expect_empty_b(
            iface_b,
            "a retained target's probe must not reach the upstream",
        )
        .await?;
        Ok(ProgramSuccess::Verified(String::from("retained")))
    }
}
