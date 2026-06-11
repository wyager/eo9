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
//! The wire protocol itself (EO9L magic, secret frame, 24-byte header, ack-paced
//! payload, CRC verdict, 'G' go-ahead) lives in the shared `eo9-flashwire` crate —
//! host-tested, and spoken byte-identically by the stickflash sibling
//! (docs/board/usb-msd-plan.md §4.1). This program owns the transport (recv/send and
//! the buffer-reuse pacing), the kexec sink, session retry, and narration.
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

use eo9_flashwire::{
    AUTHENTICATED, CRC_FRAME, GO_AHEAD, HEADER_FRAME, Handshake, KEXEC_PORT, MAGIC, MAX_SESSIONS,
    MIN_SECRET, PROGRESS_ACK, REFUSED, Refusal, SECRET_LEN_FRAME, Transfer, VERIFIED,
};
use eo9_guest::api::net::l4;
use eo9_guest::buffer;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "oskexec",
    apis: [io, text, net_l4, kexec],
});

use eo9_guest::api::kexec::kexec;

/// The default listen port (eo9_flashwire::KEXEC_PORT, chosen clear of the repo's
/// telnet fixture port).
const DEFAULT_PORT: u16 = KEXEC_PORT;
/// Console narration every this-many payload bytes (narration pacing is this
/// program's, not the protocol's: the 'k' ack cadence lives in eo9-flashwire).
const PROGRESS_INTERVAL: u64 = 4 * 1024 * 1024;
/// Image ceiling: the kernel's staging-region capacity (64 MiB reservation minus the
/// 64 KiB stub slot — kernel/eo9-kernel/src/arch/aarch64/mmu.rs, kept in sync by the
/// check-kexec gate actually flashing a full image). Anything larger is refused before
/// a byte is staged.
const MAX_IMAGE: u64 = 64 * 1024 * 1024 - 64 * 1024;
/// Transfer receive-buffer size (one buffer, reused across the whole stream — see the
/// allocation note at the streaming loop).
const CHUNK: u64 = 256 * 1024;

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

/// One accepted session up to (but not including) the payload: magic, secret frame,
/// verdict byte — every judgment is eo9-flashwire's. `Ok(true)` = authenticated,
/// proceed on this connection; `Ok(false)` = refused (already answered 'E'), caller
/// may retry once.
async fn authenticate(conn: &l4::TcpConnection, secret: &str) -> Result<bool, ProgramFailure> {
    let handshake = Handshake::new(secret.as_bytes());
    let Some(magic) = recv_exact(conn, MAGIC.len()).await? else {
        say("oskexec: peer closed before the magic");
        return Ok(false);
    };
    if handshake.feed_magic(&magic).is_err() {
        say("oskexec: bad magic — refused");
        let _ = send_all(conn, &[REFUSED]).await;
        return Ok(false);
    }
    let Some(len_bytes) = recv_exact(conn, SECRET_LEN_FRAME).await? else {
        say("oskexec: peer closed before the secret");
        return Ok(false);
    };
    let Ok(wire_len) = handshake.feed_secret_length(&[len_bytes[0], len_bytes[1]]) else {
        say("oskexec: unbelievable secret length — refused");
        let _ = send_all(conn, &[REFUSED]).await;
        return Ok(false);
    };
    // Read the whole frame before judging it (no short-circuit on length).
    let Some(wire_secret) = recv_exact(conn, wire_len).await? else {
        say("oskexec: peer closed inside the secret");
        return Ok(false);
    };
    if handshake.feed_secret(&wire_secret).is_err() {
        say("oskexec: authentication failed — refused");
        let _ = send_all(conn, &[REFUSED]).await;
        return Ok(false);
    }
    send_all(conn, &[AUTHENTICATED]).await?;
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
        let mut transfer = Transfer::new(MAX_IMAGE);
        let Some(header) = recv_exact(&conn, HEADER_FRAME).await? else {
            return Err(ProgramFailure::Protocol("peer closed before the header".into()));
        };
        let header = match transfer.feed_header(&header) {
            Ok(header) => header,
            Err(Refusal::BadImageLength { length, .. }) => {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Protocol(format!(
                    "refused image length {length} (1..={MAX_IMAGE})"
                )));
            }
            // feed_header refuses only on length; never trap a service on a
            // can't-happen path — surface it as the typed failure it would be.
            Err(other) => {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Protocol(format!(
                    "unexpected header refusal {other:?}"
                )));
            }
        };
        let len = header.length;
        say(&format!(
            "oskexec: image {len} bytes incoming (header load_addr {:#x} / \
             x0 {:#x} noted and ignored — the kexec dance owns the addresses)",
            header.load_addr, header.x0
        ));

        // Stream to stage(): CRC as we go (eo9-flashwire's accounting), a 'k' per
        // 64 KiB, narration per 4 MiB.
        // The receive buffer and the ack byte's buffer are allocated ONCE and ride the
        // owned-buffer round-trip (the pattern the io API is built around): allocating
        // a fresh host buffer per chunk measurably decayed the transfer pace over a
        // 60 MiB stream (the check-kexec gate's first runs — see GAPS, kexec entry).
        // The reusable buffer is full-size; near the payload's end it is swapped for
        // exact-remainder buffers so a recv can never swallow the trailing CRC bytes.
        let mut next_progress = PROGRESS_INTERVAL;
        let mut dst = buffer::with_capacity(CHUNK);
        let mut dst_capacity = CHUNK;
        while !transfer.complete() {
            let remaining = transfer.remaining();
            if remaining < dst_capacity {
                dst = buffer::with_capacity(remaining);
                dst_capacity = remaining;
            }
            let (returned, received) = l4::recv(&conn, dst).await;
            dst = returned;
            let result = received.map_err(net_failure)?;
            if result.bytes_received == 0 {
                return Err(ProgramFailure::Protocol(format!(
                    "peer closed mid-payload at {}/{len} bytes",
                    transfer.offset()
                )));
            }
            let chunk = buffer::prefix_to_vec(&dst, result.bytes_received);
            let progress = transfer.feed_payload(&chunk);
            if let Err(error) = kexec::stage(&kx, progress.offset - chunk.len() as u64, chunk).await {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Refused(format!("stage: {error:?}")));
            }
            // Batch every ack this chunk earned into ONE send ('k' per 64 KiB crossed;
            // the host counts bytes, not segments) — transport calls are the expensive
            // unit on this path, so one burst beats one send per boundary.
            if progress.acks_due > 0 {
                send_all(&conn, &alloc::vec![PROGRESS_ACK; progress.acks_due]).await?;
            }
            if progress.offset >= next_progress || progress.offset == len {
                say(&format!("oskexec: staged {}/{len} bytes", progress.offset));
                next_progress += PROGRESS_INTERVAL;
            }
        }

        let Some(wire_crc) = recv_exact(&conn, CRC_FRAME).await? else {
            return Err(ProgramFailure::Protocol("peer closed before the crc".into()));
        };
        let crc = match transfer.feed_crc(&wire_crc) {
            Ok(crc) => crc,
            Err(Refusal::CrcMismatch { computed, wire }) => {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Refused(format!(
                    "crc mismatch: received bytes {computed:08x}, wire said {wire:08x} — \
                     nothing committed, system untouched"
                )));
            }
            // feed_crc refuses only on mismatch; same never-trap posture as above.
            Err(other) => {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Protocol(format!(
                    "unexpected crc refusal {other:?}"
                )));
            }
        };

        // Verdict, then the go-ahead: the host answering 'G' proves the 'K' arrived,
        // so the success byte cannot be lost when commit ends this machine.
        send_all(&conn, &[VERIFIED]).await?;
        let Some(go) = recv_exact(&conn, 1).await? else {
            return Err(ProgramFailure::Protocol(
                "peer closed before the go-ahead — not committing".into(),
            ));
        };
        if transfer.feed_go_ahead(go[0]).is_err() {
            return Err(ProgramFailure::Protocol(format!(
                "expected the '{}' go-ahead, got {go:?} — not committing",
                GO_AHEAD as char
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
