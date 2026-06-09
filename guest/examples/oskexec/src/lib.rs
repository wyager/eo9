//! oskexec — receive a new Eo9 image over TCP and kexec into it.
//!
//! Targets the `eo9-examples:oskexec/oskexec` world (see `wit/world.wit`, which also
//! pins the wire protocol and the security posture): listen on TCP :9909 (or
//! `--port`), accept ONE connection, authenticate it with the mandatory preshared
//! secret, stream the serial-loader-framed payload into `eo9:kexec.stage()`, verify
//! the CRC guest-side, and — after the host's go-ahead confirms the verdict byte
//! arrived — `commit()`. A verified commit never returns: the next thing on the
//! console is the new kernel's banner. Every failure is a typed refusal that leaves
//! the running system untouched.
//!
//! One-shot by design: one successful flash, or two failed/broken sessions, and the
//! program exits — the listener exists only while an operator is actively flashing.
//!
//! No deadline machinery: a peer that stalls mid-transfer parks this task on a recv
//! that never completes (the world deliberately imports no time capability). That is
//! the bench posture — the operator kills the task (or the board's watchdog never
//! stops being patted, because the kernel drive loop still runs and nothing was
//! committed); send_image.py carries the host-side stall alarm.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::net::l4;
use eo9_guest::buffer;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "oskexec",
    apis: [io, text, net_l4, kexec],
});

use eo9_guest::api::kexec::kexec;

/// The default listen port (chosen clear of the repo's telnet fixture port).
const DEFAULT_PORT: u16 = 9909;
/// Wire magic — the serial-loader protocol's, in receive order.
const MAGIC: [u8; 4] = *b"EO9L";
/// A `k` progress byte goes back after every this-many payload bytes (protocol parity
/// with the serial stub).
const ACK_INTERVAL: u64 = 64 * 1024;
/// Console narration every this-many payload bytes.
const PROGRESS_INTERVAL: u64 = 4 * 1024 * 1024;
/// Image ceiling: the kernel's staging-region capacity (64 MiB reservation minus the
/// 64 KiB stub slot — kernel/eo9-kernel/src/arch/aarch64/mmu.rs, kept in sync by the
/// check-kexec gate actually flashing a full image). Anything larger is refused before
/// a byte is staged.
const MAX_IMAGE: u64 = 64 * 1024 * 1024 - 64 * 1024;
/// Minimum preshared-secret length (operator-enforced entropy floor).
const MIN_SECRET: usize = 16;
/// Authentication / framing attempts: one retry, then exit (no oracle loops).
const MAX_SESSIONS: u32 = 2;
/// Transfer receive-buffer size (one buffer, reused across the whole stream — see the
/// allocation note at the streaming loop).
const CHUNK: u64 = 256 * 1024;

/// CRC-32 (IEEE, reflected) — must match send_image.py's binascii.crc32 and the
/// kernel's commit-side check.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc = (crc >> 8) ^ CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    crc
}

/// The l4 API's own error, rendered into the world's failure variant.
fn net_failure(err: l4::L4Error) -> ProgramFailure {
    match err {
        l4::L4Error::Denied => ProgramFailure::Denied,
        other => ProgramFailure::Net(format!("{other:?}")),
    }
}

fn say(line: &str) {
    let _ = text::write_out_line(line);
}

/// Receive exactly `want` bytes (used for the small protocol frames). `Ok(None)` means
/// the peer closed mid-frame.
async fn recv_exact(
    conn: &l4::TcpConnection,
    want: usize,
) -> Result<Option<Vec<u8>>, ProgramFailure> {
    let mut out = Vec::with_capacity(want);
    while out.len() < want {
        let dst = buffer::with_capacity((want - out.len()) as u64);
        let (dst, received) = l4::recv(conn, dst).await;
        let result = received.map_err(net_failure)?;
        if result.bytes_received == 0 {
            return Ok(None);
        }
        out.extend_from_slice(&buffer::prefix_to_vec(&dst, result.bytes_received));
    }
    Ok(Some(out))
}

/// Send all of `bytes` (the verdict/ack bytes are tiny; payload never flows this way).
async fn send_all(conn: &l4::TcpConnection, bytes: &[u8]) -> Result<(), ProgramFailure> {
    let mut at = 0usize;
    while at < bytes.len() {
        let src = buffer::from_bytes(&bytes[at..]);
        let (_src, sent) = l4::send(conn, src).await;
        let result = sent.map_err(net_failure)?;
        at += result.bytes_sent as usize;
        if result.bytes_sent == 0 {
            return Err(ProgramFailure::Net(String::from(
                "send made no progress (peer gone?)",
            )));
        }
    }
    Ok(())
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(array)
}

/// Full-length byte compare: examines every wire byte regardless of where the first
/// mismatch is (and folds the length difference in), so the comparison's timing does
/// not narrate the match prefix. Overkill at this layer — the wire is cleartext — but
/// it costs three lines and removes the timing-oracle pattern outright.
fn secret_matches(wire: &[u8], expected: &[u8]) -> bool {
    let mut diff = (wire.len() ^ expected.len()) as u32;
    for (i, &byte) in wire.iter().enumerate() {
        diff |= u32::from(byte ^ expected[i % expected.len().max(1)]);
    }
    diff == 0
}

/// One accepted session up to (but not including) the payload: magic, secret frame,
/// verdict byte. `Ok(true)` = authenticated, proceed on this connection;
/// `Ok(false)` = refused (already answered 'E'), caller may retry once.
async fn authenticate(conn: &l4::TcpConnection, secret: &str) -> Result<bool, ProgramFailure> {
    let Some(magic) = recv_exact(conn, MAGIC.len()).await? else {
        say("oskexec: peer closed before the magic");
        return Ok(false);
    };
    if magic != MAGIC {
        say("oskexec: bad magic — refused");
        let _ = send_all(conn, b"E").await;
        return Ok(false);
    }
    let Some(len_bytes) = recv_exact(conn, 2).await? else {
        say("oskexec: peer closed before the secret");
        return Ok(false);
    };
    let wire_len = usize::from(u16::from_le_bytes([len_bytes[0], len_bytes[1]]));
    if wire_len == 0 || wire_len > 256 {
        say("oskexec: unbelievable secret length — refused");
        let _ = send_all(conn, b"E").await;
        return Ok(false);
    }
    // Read the whole frame before judging it (no short-circuit on length).
    let Some(wire_secret) = recv_exact(conn, wire_len).await? else {
        say("oskexec: peer closed inside the secret");
        return Ok(false);
    };
    if !secret_matches(&wire_secret, secret.as_bytes()) {
        say("oskexec: authentication failed — refused");
        let _ = send_all(conn, b"E").await;
        return Ok(false);
    }
    send_all(conn, b"A").await?;
    Ok(true)
}

eo9_guest::main! {
    async fn main(
        port: Option<u16>,
        secret: String,
        bootargs: String,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        if secret.len() < MIN_SECRET {
            return Err(ProgramFailure::BadArguments(format!(
                "the preshared secret must be at least {MIN_SECRET} bytes (got {})",
                secret.len()
            )));
        }
        let port = port.unwrap_or(DEFAULT_PORT);

        let net = l4::default();
        let kx = kexec::default();

        let listener = l4::listen(
            &net,
            l4::SocketAddress { address: l4::IpAddress::V4((0, 0, 0, 0)), port },
        )
        .await
        .map_err(net_failure)?;
        say(&format!(
            "oskexec: listening on :{port} — one-shot, preshared-secret gated \
             (cleartext on the LAN: bench tool, see the world docs)"
        ));

        // One successful session; one retry for a refused/broken one.
        let mut conn = None;
        for attempt in 1..=MAX_SESSIONS {
            let (candidate, peer) = l4::accept(&listener).await.map_err(net_failure)?;
            let peer_text = match peer.address {
                l4::IpAddress::V4((a, b, c, d)) => format!("{a}.{b}.{c}.{d}:{}", peer.port),
                l4::IpAddress::V6(_) => format!("[v6]:{}", peer.port),
            };
            say(&format!("oskexec: connection from {peer_text} (attempt {attempt})"));
            if authenticate(&candidate, &secret).await? {
                conn = Some(candidate);
                break;
            }
            drop(candidate);
        }
        let Some(conn) = conn else {
            return Err(ProgramFailure::Protocol(format!(
                "no authenticated session within {MAX_SESSIONS} attempts — exiting \
                 (one-shot; restart to listen again)"
            )));
        };

        // Header: load_addr and x0 are framing parity with the serial wire — ignored
        // here (the kernel's dance owns every address); length is enforced.
        let Some(header) = recv_exact(&conn, 24).await? else {
            return Err(ProgramFailure::Protocol("peer closed before the header".into()));
        };
        let (load_addr, len, x0) = (le_u64(&header[0..]), le_u64(&header[8..]), le_u64(&header[16..]));
        if len == 0 || len > MAX_IMAGE {
            let _ = send_all(&conn, b"E").await;
            return Err(ProgramFailure::Protocol(format!(
                "refused image length {len} (1..={MAX_IMAGE})"
            )));
        }
        say(&format!(
            "oskexec: image {len} bytes incoming (header load_addr {load_addr:#x} / \
             x0 {x0:#x} noted and ignored — the kexec dance owns the addresses)"
        ));

        // Stream to stage(): CRC as we go, a 'k' per 64 KiB, narration per 4 MiB.
        // The receive buffer and the ack byte's buffer are allocated ONCE and ride the
        // owned-buffer round-trip (the pattern the io API is built around): allocating
        // a fresh host buffer per chunk measurably decayed the transfer pace over a
        // 60 MiB stream (the check-kexec gate's first runs — see GAPS, kexec entry).
        // The reusable buffer is full-size; near the payload's end it is swapped for
        // exact-remainder buffers so a recv can never swallow the trailing CRC bytes.
        let mut offset = 0u64;
        let mut crc = 0xFFFF_FFFFu32;
        let mut next_ack = ACK_INTERVAL;
        let mut next_progress = PROGRESS_INTERVAL;
        let mut dst = buffer::with_capacity(CHUNK);
        let mut dst_capacity = CHUNK;
        while offset < len {
            let remaining = len - offset;
            if remaining < dst_capacity {
                dst = buffer::with_capacity(remaining);
                dst_capacity = remaining;
            }
            let (returned, received) = l4::recv(&conn, dst).await;
            dst = returned;
            let result = received.map_err(net_failure)?;
            if result.bytes_received == 0 {
                return Err(ProgramFailure::Protocol(format!(
                    "peer closed mid-payload at {offset}/{len} bytes"
                )));
            }
            let chunk = buffer::prefix_to_vec(&dst, result.bytes_received);
            crc = crc32_update(crc, &chunk);
            let staged_len = chunk.len() as u64;
            if let Err(error) = kexec::stage(&kx, offset, chunk).await {
                let _ = send_all(&conn, b"E").await;
                return Err(ProgramFailure::Refused(format!("stage: {error:?}")));
            }
            offset += staged_len;
            // Batch every ack this chunk earned into ONE send ('k' per 64 KiB crossed;
            // the host counts bytes, not segments) — transport calls are the expensive
            // unit on this path, so one burst beats one send per boundary.
            let mut due = 0usize;
            while offset >= next_ack {
                due += 1;
                next_ack += ACK_INTERVAL;
            }
            if due > 0 {
                send_all(&conn, &alloc::vec![b'k'; due]).await?;
            }
            if offset >= next_progress || offset == len {
                say(&format!("oskexec: staged {offset}/{len} bytes"));
                next_progress += PROGRESS_INTERVAL;
            }
        }
        let crc = !crc;

        let Some(wire_crc) = recv_exact(&conn, 4).await? else {
            return Err(ProgramFailure::Protocol("peer closed before the crc".into()));
        };
        let wire_crc = u32::from_le_bytes([wire_crc[0], wire_crc[1], wire_crc[2], wire_crc[3]]);
        if wire_crc != crc {
            let _ = send_all(&conn, b"E").await;
            return Err(ProgramFailure::Refused(format!(
                "crc mismatch: received bytes {crc:08x}, wire said {wire_crc:08x} — \
                 nothing committed, system untouched"
            )));
        }

        // Verdict, then the go-ahead: the host answering 'G' proves the 'K' arrived,
        // so the success byte cannot be lost when commit ends this machine.
        send_all(&conn, b"K").await?;
        let Some(go) = recv_exact(&conn, 1).await? else {
            return Err(ProgramFailure::Protocol(
                "peer closed before the go-ahead — not committing".into(),
            ));
        };
        if go != b"G" {
            return Err(ProgramFailure::Protocol(format!(
                "expected the 'G' go-ahead, got {go:?} — not committing"
            )));
        }

        say(&format!(
            "oskexec: verified {len} bytes (crc {crc:08x}); committing — the kernel \
             takes it from here"
        ));
        match kexec::commit(&kx, len, crc, bootargs).await {
            // Unreachable in practice: a verified commit never returns.
            Ok(()) => Ok(ProgramSuccess::Committed(format!(
                "commit({len} bytes, crc {crc:08x}) returned — the kernel declined to jump"
            ))),
            Err(error) => Err(ProgramFailure::Refused(format!("commit: {error:?}"))),
        }
    }
}
