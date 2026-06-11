//! stickflash — receive an EO9.IMG over TCP and rewrite the boot stick's fixed FAT
//! slot in place (docs/board/usb-msd-plan.md §4; oskexec's sibling).
//!
//! Targets the `eo9-examples:stickflash/stickflash` world (see `wit/world.wit`, which
//! pins the wire protocol and the security posture): listen on TCP :9910 (or
//! `--port`), accept ONE connection, authenticate it with the mandatory preshared
//! secret, receive the slot-sized image into memory, CRC-verify it, overwrite
//! EO9.IMG's clusters in chain order through the composed eo9:disk (the FAT32
//! partition window — `disk.part --partition 1` on a whole-stick disk), read every
//! written range back and CRC-verify, and ONLY THEN answer the 'K' verdict
//! (write-then-verify before declaring success — usb-msd-plan §3.2). Every failure
//! before the first cluster write is a typed refusal that leaves the stick untouched;
//! any failure after it is the typed `torn` with LOUD narration (the boot-time CRC
//! gate fails a torn stick through to the prompt; recovery is a re-run or serial).
//!
//! The wire protocol (EO9L magic, secret frame, 24-byte header, ack-paced payload,
//! CRC verdict, 'G' confirmation) lives in the shared `eo9-flashwire` crate —
//! host-tested, spoken byte-identically by oskexec. The FAT walk and the same-size
//! overwrite plan live in `eo9-fatwalk` — also host-tested, and the same code path
//! `cargo xtask build-stick` self-checks the built stick with. This program owns the
//! transport, the disk sink, session retry, and narration.
//!
//! WHO PADS: the host. `send_image.py --stick` zero-pads the image to the slot size
//! before CRC'ing and sending, exactly as `build-stick` pads at stick-build time, so
//! the header length always EQUALS the slot, the wire CRC covers the padded slot
//! (the same value build-stick would bake), and the same-size in-place overwrite
//! discipline needs no FAT logic here. A header length differing from the slot is a
//! typed refusal before a payload byte flows.
//!
//! fatwalk's metadata reads are synchronous (`SectorRead`) while eo9:disk is async;
//! the bridge is a demand-fetch cache: run the walk against cached sectors, and on a
//! miss await the one sector read and re-run. The walk is deterministic and the
//! cache only grows, so the loop is bounded by the volume's metadata footprint (and
//! belt-and-braces capped — the loop-safe-exit discipline). Metadata sectors and
//! EO9.IMG's data clusters are disjoint, so the cache can never go stale from our
//! own writes.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_fatwalk::{FileMap, Run, SECTOR, Volume};
use eo9_flashwire::{
    AUTHENTICATED, CRC_FRAME, Crc32, GO_AHEAD, HEADER_FRAME, Handshake, MAGIC, MAX_SESSIONS,
    MIN_SECRET, PROGRESS_ACK, REFUSED, Refusal, SECRET_LEN_FRAME, STICKFLASH_PORT, Transfer,
    VERIFIED,
};
use eo9_guest::api::disk::disk;
use eo9_guest::api::net::l4;
use eo9_guest::buffer;
use eo9_guest::text;
use eo9_guest::time;

eo9_guest::bindings!({
    world: "stickflash",
    apis: [io, text, time, net_l4, disk],
});

// The user-facing manual, embedded as the `eo9-manual` custom section and rendered by
// `man stickflash` in eosh (docs/design/component-manuals.md; this component is in
// the required-manual set — usb-msd-plan §7).
eo9_guest::manual! {
    name: "stickflash",
    synopsis: "rewrite the boot stick's EO9.IMG slot from a network-received image, verified",
    description: [
        "Listens once on TCP :9910 for the eo9-flashwire protocol (oskexec's wire, a disk sink:",
        "the host side is send_image.py --stick, which zero-pads the image to the stick's fixed",
        "slot size before CRC'ing). After the preshared-secret handshake it receives the",
        "slot-sized image into memory, verifies the wire CRC, overwrites EO9.IMG's clusters in",
        "chain order through the composed eo9:disk (the FAT32 partition window - compose",
        "disk.part --partition 1 over the raw stick), reads every written range back, and only",
        "after the read-back CRC matches does it answer the 'K' verdict and exit: reset to boot",
        "the new image (v1 never auto-resets, and BOOT.SCR is not rewritten - it is the",
        "unconditional-go script today). Failures before the first write refuse typed with the",
        "stick untouched; failures after it are the typed `torn`, narrated loudly - a torn",
        "EO9.IMG fails the boot-time CRC gate through to the prompt, and recovery is a re-run",
        "or the serial loader. SECURITY: the secret travels cleartext on the LAN; trusted-LAN /",
        "bench tool only (the oskexec posture - see the world docs).",
    ],
    args: [
        { name: "port", ty: "u16", optional,
          doc: "TCP port to listen on (default 9910; 9909 is oskexec)", kind: "port" },
        { name: "secret", ty: "string", required,
          doc: "preshared secret, >= 16 bytes (the same value given to send_image.py --stick)" },
    ],
    examples: [
        { line: "net.virtio $ net.l4.over-l2 $ usb.ohci-pci $ usb.msd $ disk.part $ stickflash --secret ...",
          doc: "QEMU: pci-ohci + usb-storage carrying a build-stick image (the check-stickflash gate)" },
        { line: "net.rtl8125 $ (net.l4.over-l2 --address dhcp) $ usb.ohci $ usb.msd $ disk.part $ stickflash --secret ...",
          doc: "the board: the stick in a USB2-A port; flash from the Mac with send_image.py --stick" },
    ],
    see_also: "oskexec, usb.msd, disk.part, mdcheck",
}

/// The default listen port (eo9_flashwire::STICKFLASH_PORT; 9909 is oskexec).
const DEFAULT_PORT: u16 = STICKFLASH_PORT;
/// Console narration every this-many payload/flash bytes (narration pacing is this
/// program's, not the protocol's: the 'k' ack cadence lives in eo9-flashwire).
const PROGRESS_INTERVAL: u64 = 4 * 1024 * 1024;
/// Transfer receive-buffer size (one buffer, reused across the whole stream — the
/// oskexec allocation lesson: a fresh host buffer per chunk decays the pace).
const CHUNK: u64 = 256 * 1024;
/// Disk I/O chunk for the write plan and the read-back: bounds the host-buffer copy
/// per call (usb.msd loops its own 64 KiB commands under each call either way).
const IO_CHUNK: usize = 4 * 1024 * 1024;
/// Belt-and-braces bound on demand-fetched metadata sectors (boot sector + FAT +
/// root directory; a 62 MiB partition's first FAT is ~1024 sectors — 65536 covers
/// any stick the structural boot cap admits, and a walk that wants more is foreign).
const MAX_META_FETCHES: u32 = 65536;

fn say(line: &str) {
    let _ = text::write_out_line(line);
}

/// The l4 API's own error, rendered into the world's failure variant.
fn net_failure(err: l4::L4Error) -> ProgramFailure {
    match err {
        l4::L4Error::Denied => ProgramFailure::Denied,
        other => ProgramFailure::Net(format!("{other:?}")),
    }
}

/// Milliseconds between two monotonic instants (saturating: time.frozen reads 0).
fn elapsed_ms(from: time::Instant, to: time::Instant) -> u64 {
    to.nanoseconds.saturating_sub(from.nanoseconds) / 1_000_000
}

// ------------------------------------------------------------------------------------------
// The sync-fatwalk-over-async-disk bridge: a demand-fetch sector cache
// ------------------------------------------------------------------------------------------

/// A `SectorRead` over cached sectors only: a miss reports the wanted LBA as the
/// device error, the async caller fetches it, and the walk re-runs. Deterministic
/// walks + a grow-only cache = guaranteed progress, one new sector per round.
struct CachedSectors<'c> {
    sectors: &'c BTreeMap<u64, [u8; SECTOR]>,
}

impl eo9_fatwalk::SectorRead for CachedSectors<'_> {
    type Error = u64; // the missing LBA

    fn read_sector(&mut self, lba: u64, out: &mut [u8; SECTOR]) -> Result<(), u64> {
        match self.sectors.get(&lba) {
            Some(bytes) => {
                out.copy_from_slice(bytes);
                Ok(())
            }
            None => Err(lba),
        }
    }
}

/// Why the FAT discovery failed: a disk error (the stick untouched) or a typed
/// fatwalk refusal (foreign layout / missing file — also untouched).
enum FatFailure {
    Disk(String),
    Refused(String),
}

impl From<FatFailure> for ProgramFailure {
    fn from(err: FatFailure) -> Self {
        match err {
            FatFailure::Disk(text) => ProgramFailure::Disk(text),
            FatFailure::Refused(text) => ProgramFailure::Refused(text),
        }
    }
}

/// Run one deterministic fatwalk operation against the cache, fetching missing
/// sectors through eo9:disk until it completes (or refuses typed). The first fetch
/// is also the provider's lazy bring-up (usb.msd enumerates on its first awaited
/// operation — the mdcheck wake convention).
async fn demand_fat<T>(
    dev: &disk::DiskImpl,
    cache: &mut BTreeMap<u64, [u8; SECTOR]>,
    what: &str,
    mut op: impl FnMut(&mut CachedSectors<'_>) -> Result<T, eo9_fatwalk::Error<u64>>,
) -> Result<T, FatFailure> {
    let mut fetches = 0u32;
    loop {
        let mut view = CachedSectors { sectors: cache };
        match op(&mut view) {
            Ok(value) => return Ok(value),
            Err(eo9_fatwalk::Error::Fat(refusal)) => {
                return Err(FatFailure::Refused(format!(
                    "{what}: the volume refused: {refusal:?} (foreign layouts are never \
                     written - only xtask-built sticks are supported)"
                )));
            }
            Err(eo9_fatwalk::Error::Device(lba)) => {
                fetches += 1;
                if fetches > MAX_META_FETCHES {
                    return Err(FatFailure::Refused(format!(
                        "{what}: the metadata walk wanted more than {MAX_META_FETCHES} \
                         sectors - not a stick this flasher believes in"
                    )));
                }
                let (dst, outcome) = disk::read(
                    dev,
                    lba * SECTOR as u64,
                    buffer::with_capacity(SECTOR as u64),
                )
                .await;
                let read = outcome
                    .map_err(|err| FatFailure::Disk(format!("{what}: sector {lba}: {err:?}")))?;
                if read.bytes_read != SECTOR as u64 {
                    return Err(FatFailure::Disk(format!(
                        "{what}: sector {lba}: short read ({} of {SECTOR} bytes)",
                        read.bytes_read
                    )));
                }
                let bytes = buffer::prefix_to_vec(&dst, SECTOR as u64);
                let mut sector = [0u8; SECTOR];
                sector.copy_from_slice(&bytes);
                cache.insert(lba, sector);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Transport glue (the oskexec shape; eo9-flashwire owns every protocol decision)
// ------------------------------------------------------------------------------------------

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
        say("stickflash: peer closed before the magic");
        return Ok(false);
    };
    if handshake.feed_magic(&magic).is_err() {
        say("stickflash: bad magic - refused");
        let _ = send_all(conn, &[REFUSED]).await;
        return Ok(false);
    }
    let Some(len_bytes) = recv_exact(conn, SECRET_LEN_FRAME).await? else {
        say("stickflash: peer closed before the secret");
        return Ok(false);
    };
    let Ok(wire_len) = handshake.feed_secret_length(&[len_bytes[0], len_bytes[1]]) else {
        say("stickflash: unbelievable secret length - refused");
        let _ = send_all(conn, &[REFUSED]).await;
        return Ok(false);
    };
    // Read the whole frame before judging it (no short-circuit on length).
    let Some(wire_secret) = recv_exact(conn, wire_len).await? else {
        say("stickflash: peer closed inside the secret");
        return Ok(false);
    };
    if handshake.feed_secret(&wire_secret).is_err() {
        say("stickflash: authentication failed - refused");
        let _ = send_all(conn, &[REFUSED]).await;
        return Ok(false);
    }
    send_all(conn, &[AUTHENTICATED]).await?;
    Ok(true)
}

/// The loud torn-stick exit: every failure after the first cluster write lands here.
/// Sends 'E' (best effort — the peer may already be gone), narrates the recovery
/// story, and returns the typed failure.
async fn torn(conn: &l4::TcpConnection, detail: String) -> ProgramFailure {
    say("stickflash: !!! FAILURE AFTER CLUSTER WRITES BEGAN - THE STICK IS POSSIBLY TORN !!!");
    say(&format!("stickflash: {detail}"));
    say(
        "stickflash: a torn EO9.IMG fails the boot-time crc gate through to the prompt; \
         re-run the flash, or serial-boot if the stick no longer boots",
    );
    let _ = send_all(conn, &[REFUSED]).await;
    ProgramFailure::Torn(detail)
}

// ------------------------------------------------------------------------------------------
// The flash itself
// ------------------------------------------------------------------------------------------

/// Execute the write plan: EO9.IMG's clusters overwritten in chain order, bounded
/// copies per disk call. Returns the failure ready-made (already narrated + 'E' sent).
async fn run_write_plan(
    dev: &disk::DiskImpl,
    conn: &l4::TcpConnection,
    volume: &Volume,
    map: &FileMap,
    payload: &[u8],
    slot: u64,
) -> Result<(), ProgramFailure> {
    // Planning is pure and refuses BEFORE any byte moves (size mismatch can't happen
    // here — the header equality check pinned the length — but never trap on it).
    let plan = match volume.write_plan(map, payload) {
        Ok(plan) => plan,
        Err(refusal) => {
            let _ = send_all(conn, &[REFUSED]).await;
            return Err(ProgramFailure::Refused(format!(
                "write plan refused: {refusal:?} - nothing written, the stick is untouched"
            )));
        }
    };
    say(&format!(
        "stickflash: write plan: {} run(s), {slot} bytes - executing",
        plan.len()
    ));
    let mut written = 0u64;
    let mut next_progress = PROGRESS_INTERVAL;
    for op in &plan {
        let mut at = 0usize;
        while at < op.data.len() {
            let end = usize::min(at + IO_CHUNK, op.data.len());
            let chunk = &op.data[at..end];
            let offset = op.lba * SECTOR as u64 + at as u64;
            let (_src, outcome) = disk::write(dev, offset, buffer::from_bytes(chunk)).await;
            match outcome {
                Ok(result) if result.bytes_written == chunk.len() as u64 => {}
                Ok(result) => {
                    return Err(torn(
                        conn,
                        format!(
                            "short write at partition offset {offset}: {} of {} bytes",
                            result.bytes_written,
                            chunk.len()
                        ),
                    )
                    .await);
                }
                Err(error) => {
                    return Err(torn(
                        conn,
                        format!("write at partition offset {offset}: {error:?}"),
                    )
                    .await);
                }
            }
            written += chunk.len() as u64;
            at = end;
            if written >= next_progress || written == slot {
                say(&format!("stickflash: wrote {written}/{slot} bytes"));
                next_progress += PROGRESS_INTERVAL;
            }
        }
    }
    Ok(())
}

/// Read every written range back (in chain order — the same byte order the wire CRC
/// covers) and verify. Returns the agreed CRC.
async fn verify_read_back(
    dev: &disk::DiskImpl,
    conn: &l4::TcpConnection,
    runs: &[Run],
    wire_crc: u32,
    slot: u64,
) -> Result<u32, ProgramFailure> {
    let mut crc = Crc32::new();
    let mut read_total = 0u64;
    let mut next_progress = PROGRESS_INTERVAL;
    for run in runs {
        let run_bytes = u64::from(run.sectors) * SECTOR as u64;
        let mut at = 0u64;
        while at < run_bytes {
            let want = u64::min(IO_CHUNK as u64, run_bytes - at);
            let offset = run.lba * SECTOR as u64 + at;
            let (dst, outcome) = disk::read(dev, offset, buffer::with_capacity(want)).await;
            match outcome {
                Ok(result) if result.bytes_read == want => {}
                Ok(result) => {
                    return Err(torn(
                        conn,
                        format!(
                            "short read-back at partition offset {offset}: {} of {want} bytes",
                            result.bytes_read
                        ),
                    )
                    .await);
                }
                Err(error) => {
                    return Err(torn(
                        conn,
                        format!("read-back at partition offset {offset}: {error:?}"),
                    )
                    .await);
                }
            }
            crc.update(&buffer::prefix_to_vec(&dst, want));
            at += want;
            read_total += want;
            if read_total >= next_progress || read_total == slot {
                say(&format!("stickflash: read back {read_total}/{slot} bytes"));
                next_progress += PROGRESS_INTERVAL;
            }
        }
    }
    let computed = crc.finalize();
    if computed != wire_crc {
        return Err(torn(
            conn,
            format!(
                "read-back crc {computed:08x} does not match the verified wire crc \
                 {wire_crc:08x} - the device did not store what it acknowledged"
            ),
        )
        .await);
    }
    Ok(computed)
}

eo9_guest::main! {
    async fn main(
        port: Option<u16>,
        secret: String,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        if secret.len() < MIN_SECRET {
            return Err(ProgramFailure::BadArguments(format!(
                "the preshared secret must be at least {MIN_SECRET} bytes (got {})",
                secret.len()
            )));
        }
        let port = port.unwrap_or(DEFAULT_PORT);

        let net = l4::default();
        let dev = disk::default();

        // FAT discovery BEFORE listening: a foreign stick (or a missing EO9.IMG)
        // refuses while no peer is waiting, and the slot size — the transfer ceiling —
        // is known up front. The first sector fetch doubles as the provider's lazy
        // bring-up (usb.msd enumerates on its first awaited operation).
        let mut meta: BTreeMap<u64, [u8; SECTOR]> = BTreeMap::new();
        let volume =
            demand_fat(&dev, &mut meta, "open", |view| Volume::open(view)).await?;
        let map = demand_fat(&dev, &mut meta, "locate EO9.IMG", |view| {
            volume.locate(view, "EO9.IMG")
        })
        .await?;
        let slot = u64::from(map.size);
        if slot == 0 || !(map.size as usize).is_multiple_of(SECTOR) {
            return Err(ProgramFailure::Refused(format!(
                "EO9.IMG is {slot} bytes - not a whole-sector fixed slot; this stick \
                 was not built by `cargo xtask build-stick` (foreign layouts are never \
                 written)"
            )));
        }
        // The read-back map, computed once while the chain is fresh (pure; the chain
        // is already walked and size-checked).
        let runs = volume.runs(&map, 0, slot).map_err(|refusal| {
            ProgramFailure::Refused(format!("runs over the located chain: {refusal:?}"))
        })?;
        say(&format!(
            "stickflash: EO9.IMG slot {slot} bytes in {} run(s) of {}-byte clusters on \
             the composed FAT window",
            runs.len(),
            volume.cluster_bytes(),
        ));

        let listener = l4::listen(
            &net,
            l4::SocketAddress { address: l4::IpAddress::V4((0, 0, 0, 0)), port },
        )
        .await
        .map_err(net_failure)?;
        say(&format!(
            "stickflash: listening on :{port} - one flash, preshared-secret gated \
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
            say(&format!("stickflash: connection from {peer_text} (attempt {attempt})"));
            if authenticate(&candidate, &secret).await? {
                conn = Some(candidate);
                break;
            }
            drop(candidate);
        }
        let Some(conn) = conn else {
            return Err(ProgramFailure::Protocol(format!(
                "no authenticated session within {MAX_SESSIONS} attempts - exiting \
                 (one-shot; restart to listen again)"
            )));
        };

        // Header: load_addr and x0 are framing parity with the serial wire — ignored
        // (the slot owns every address); the length must EQUAL the slot (the host
        // pads — see the module docs; flashwire's own check refuses 0 and > slot).
        let mut transfer = Transfer::new(slot);
        let Some(header) = recv_exact(&conn, HEADER_FRAME).await? else {
            return Err(ProgramFailure::Protocol("peer closed before the header".into()));
        };
        let header = match transfer.feed_header(&header) {
            Ok(header) => header,
            Err(Refusal::BadImageLength { length, .. }) => {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Refused(format!(
                    "refused image length {length} (the EO9.IMG slot is {slot} bytes; \
                     send_image.py --stick pads to the slot)"
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
        if header.length != slot {
            let _ = send_all(&conn, &[REFUSED]).await;
            return Err(ProgramFailure::Refused(format!(
                "image length {} != the {slot}-byte EO9.IMG slot - stick images arrive \
                 pre-padded (send_image.py --stick pads and CRCs the padded slot); \
                 nothing written",
                header.length
            )));
        }
        let len = header.length;
        say(&format!(
            "stickflash: image {len} bytes incoming (header load_addr {:#x} / x0 {:#x} \
             noted and ignored - the slot owns every address)",
            header.load_addr, header.x0
        ));

        // Receive the whole padded slot into memory: CRC as we go (eo9-flashwire's
        // accounting), a 'k' per 64 KiB, narration per 4 MiB. The receive buffer is
        // allocated ONCE and rides the owned-buffer round-trip (the oskexec lesson);
        // near the payload's end it is swapped for exact-remainder buffers so a recv
        // can never swallow the trailing CRC bytes.
        let t_receive = time::monotonic_now();
        let mut payload: Vec<u8> = Vec::with_capacity(len as usize);
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
                    "peer closed mid-payload at {}/{len} bytes - nothing written",
                    transfer.offset()
                )));
            }
            let chunk = buffer::prefix_to_vec(&dst, result.bytes_received);
            let progress = transfer.feed_payload(&chunk);
            payload.extend_from_slice(&chunk);
            // Batch every ack this chunk earned into ONE send ('k' per 64 KiB crossed;
            // the host counts bytes, not segments).
            if progress.acks_due > 0 {
                send_all(&conn, &alloc::vec![PROGRESS_ACK; progress.acks_due]).await?;
            }
            if progress.offset >= next_progress || progress.offset == len {
                say(&format!("stickflash: received {}/{len} bytes", progress.offset));
                next_progress += PROGRESS_INTERVAL;
            }
        }

        let Some(wire_crc) = recv_exact(&conn, CRC_FRAME).await? else {
            return Err(ProgramFailure::Protocol(
                "peer closed before the crc - nothing written".into(),
            ));
        };
        let crc = match transfer.feed_crc(&wire_crc) {
            Ok(crc) => crc,
            Err(Refusal::CrcMismatch { computed, wire }) => {
                let _ = send_all(&conn, &[REFUSED]).await;
                return Err(ProgramFailure::Refused(format!(
                    "crc mismatch: received bytes {computed:08x}, wire said {wire:08x} - \
                     nothing written, the stick is untouched"
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
        let t_flash = time::monotonic_now();
        say(&format!(
            "stickflash: wire crc {crc:08x} verified over {len} bytes in {} ms; \
             flashing the slot",
            elapsed_ms(t_receive, t_flash),
        ));

        // THE COMMIT: clusters overwritten in chain order, then every written range
        // read back and CRC-verified. 'K' goes out only after the verify — a failure
        // in here is the loud `torn` path (usb-msd-plan §3.2).
        run_write_plan(&dev, &conn, &volume, &map, &payload, slot).await?;
        let t_verify = time::monotonic_now();
        let verified = verify_read_back(&dev, &conn, &runs, crc, slot).await?;
        let t_done = time::monotonic_now();
        say(&format!(
            "stickflash: wrote {slot} bytes in {} ms; read back and verified crc \
             {verified:08x} in {} ms - matches the wire",
            elapsed_ms(t_flash, t_verify),
            elapsed_ms(t_verify, t_done),
        ));

        // Verdict, then the peer's confirmation. The stick already carries the
        // verified image — the 'G' frame (shared with oskexec, where it gates the
        // irreversible commit) here only tells us the operator's host saw the 'K'.
        send_all(&conn, &[VERIFIED]).await?;
        let confirmation = match recv_exact(&conn, 1).await? {
            Some(byte) if transfer.feed_go_ahead(byte[0]).is_ok() => "host confirmed",
            Some(byte) => {
                say(&format!(
                    "stickflash: expected the '{}' confirmation, got {byte:?} - the \
                     verdict may not have landed host-side (the stick is verified \
                     either way)",
                    GO_AHEAD as char
                ));
                "unconfirmed (unexpected byte)"
            }
            None => {
                say(
                    "stickflash: peer closed before confirming the verdict - the \
                     stick is verified either way",
                );
                "unconfirmed (peer closed)"
            }
        };

        say(&format!(
            "stickflash: EO9.IMG rewritten and verified ({slot} bytes, crc \
             {verified:08x}, {confirmation}); BOOT.SCR untouched (unconditional-go \
             v1). Reset to boot it."
        ));
        Ok(ProgramSuccess::Flashed(format!(
            "{slot} bytes, crc {verified:08x} - reset to boot it"
        )))
    }
}
