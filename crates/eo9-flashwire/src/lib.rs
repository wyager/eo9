//! The network-flash wire protocol, factored out of `guest/examples/oskexec` so the
//! `stickflash` sibling (docs/board/usb-msd-plan.md §4.1) speaks it byte-identically.
//!
//! # The wire, in receive order (the receiver's view)
//!
//! ```text
//! "EO9L"                          magic (4 bytes)
//! <H len(secret)>                 secret length, little-endian u16 (1..=256)
//! secret                          preshared secret, cleartext (bench/LAN tool)
//!                                 -> 'A' (authenticated) or 'E' (refused)
//! <Q load_addr> <Q len> <Q x0>    24-byte header (serial-loader framing parity;
//!                                 load_addr/x0 are carried but the TCP receivers
//!                                 ignore them — the sink owns every address)
//!                                 -> 'E' if len is 0 or above the sink's ceiling
//! payload (len bytes)             <- 'k' ack per 64 KiB crossed (the host's flow
//!                                 control and stall alarm pace on these)
//! <I crc32(payload)>              -> 'K' (verified) or 'E' (mismatch)
//! "G"                             go-ahead: only after the host proves the 'K'
//!                                 arrived does the receiver commit, so the verdict
//!                                 cannot be lost when a kexec ends the machine
//! ```
//!
//! The sender is `boards/opi5-serial-loader/tools/send_image.py` (`--tcp`); the CRC is
//! IEEE reflected CRC-32, identical to Python's `binascii.crc32` (the vector test
//! below pins the same value as send_image.py `--selftest`). Default ports: oskexec
//! listens on [`KEXEC_PORT`] 9909, stickflash on [`STICKFLASH_PORT`] 9910 — same
//! protocol, different sink and ceiling.
//!
//! # What lives here vs. in the consumers
//!
//! This crate is sans-I/O. It owns every protocol *decision*: frame validation
//! ([`Handshake`]), length policy, CRC accounting and ack batching ([`Transfer`]),
//! and the reply-byte vocabulary (the `b'A'`/`b'E'`/`b'k'`/`b'K'`/`b'G'` constants).
//! Consumers own the transport (recv/send and their buffer-reuse pacing), the sink
//! (`kexec.stage` for oskexec, the fatwalk cluster overwrite for stickflash), session
//! retry, and narration. A refusal here tells the caller to send [`REFUSED`] and
//! how to report it; the caller's wire bytes stay exactly what oskexec always sent.

#![cfg_attr(not(test), no_std)]

/// Wire magic, in receive order — shared with the serial-loader stub's framing.
pub const MAGIC: [u8; 4] = *b"EO9L";
/// The secret-length frame: little-endian u16.
pub const SECRET_LEN_FRAME: usize = 2;
/// The serial-loader header frame: `<Q load_addr> <Q length> <Q x0>`, little-endian.
pub const HEADER_FRAME: usize = 24;
/// The trailing CRC frame: little-endian u32, IEEE reflected CRC-32 of the payload.
pub const CRC_FRAME: usize = 4;
/// One `'k'` progress ack per this many payload bytes (protocol parity with the
/// serial stub; the host's flow-control window and stall alarm count these).
pub const ACK_INTERVAL: u64 = 64 * 1024;
/// Minimum preshared-secret length (operator-enforced entropy floor).
pub const MIN_SECRET: usize = 16;
/// The largest secret-length frame a receiver believes (anything else is a refusal,
/// not an allocation).
pub const MAX_SECRET_FRAME: usize = 256;
/// Authentication / framing attempts a receiver grants: one retry, then exit
/// (no oracle loops).
pub const MAX_SESSIONS: u32 = 2;
/// oskexec's default listen port (chosen clear of the repo's telnet fixture port).
pub const KEXEC_PORT: u16 = 9909;
/// stickflash's default listen port (9909 is oskexec — usb-msd-plan §4.1).
pub const STICKFLASH_PORT: u16 = 9910;

/// Reply byte: the secret matched — the payload may flow.
pub const AUTHENTICATED: u8 = b'A';
/// Reply byte: refused (bad magic / bad secret / bad length / CRC mismatch).
pub const REFUSED: u8 = b'E';
/// Reply byte: progress ack, one per [`ACK_INTERVAL`] payload bytes crossed.
pub const PROGRESS_ACK: u8 = b'k';
/// Reply byte: payload received and CRC-verified — awaiting the go-ahead.
pub const VERIFIED: u8 = b'K';
/// The host's go-ahead byte: its arrival proves the [`VERIFIED`] verdict landed, so
/// the receiver may commit (a kexec commit never returns to send anything).
pub const GO_AHEAD: u8 = b'G';

/// Why a frame was refused. The receiver answers [`REFUSED`] on the wire (except
/// [`Refusal::NotGoAhead`], where the session just ends uncommitted) and renders its
/// own narration from the carried values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The first four bytes were not [`MAGIC`].
    BadMagic,
    /// The secret-length frame said 0 or more than [`MAX_SECRET_FRAME`].
    BadSecretLength { length: usize },
    /// The wire secret did not match (judged in constant time over the full frame).
    BadSecret,
    /// The header's length was 0 or above the sink's ceiling.
    BadImageLength { length: u64, max: u64 },
    /// The trailing CRC frame disagreed with the received bytes.
    CrcMismatch { computed: u32, wire: u32 },
    /// The post-verdict byte was not [`GO_AHEAD`] — do not commit.
    NotGoAhead { byte: u8 },
}

/// IEEE reflected CRC-32 — matches Python's `binascii.crc32` (send_image.py) and the
/// kernel's kexec commit-side check. The table is computed at compile time.
pub struct Crc32(u32);

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

impl Crc32 {
    pub fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 >> 8) ^ CRC_TABLE[((self.0 ^ u32::from(b)) & 0xFF) as usize];
        }
    }

    pub fn finalize(&self) -> u32 {
        !self.0
    }

    /// One-shot CRC of a complete buffer.
    pub fn of(bytes: &[u8]) -> u32 {
        let mut crc = Crc32::new();
        crc.update(bytes);
        crc.finalize()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

/// Full-length byte compare: examines every wire byte regardless of where the first
/// mismatch is (and folds the length difference in), so the comparison's timing does
/// not narrate the match prefix. Overkill at this layer — the wire is cleartext — but
/// it costs three lines and removes the timing-oracle pattern outright.
pub fn secret_matches(wire: &[u8], expected: &[u8]) -> bool {
    // An empty `expected` made the original's `i % len.max(1)` index into an empty
    // slice (a latent panic oskexec could never hit: it floors its secret at
    // MIN_SECRET before listening). Judged explicitly here so the helper is total.
    if expected.is_empty() {
        return wire.is_empty();
    }
    let mut diff = (wire.len() ^ expected.len()) as u32;
    for (i, &byte) in wire.iter().enumerate() {
        diff |= u32::from(byte ^ expected[i % expected.len()]);
    }
    diff == 0
}

/// The 24-byte serial-loader header, parsed. The TCP receivers note `load_addr` and
/// `x0` (framing parity — the sink owns every address) and enforce only `length`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub load_addr: u64,
    pub length: u64,
    pub x0: u64,
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(array)
}

impl Header {
    /// Parse the [`HEADER_FRAME`]-sized header. The frame must be exactly 24 bytes —
    /// the caller's `recv_exact` guarantees it.
    pub fn parse(frame: &[u8]) -> Header {
        debug_assert_eq!(frame.len(), HEADER_FRAME);
        Header {
            load_addr: le_u64(&frame[0..]),
            length: le_u64(&frame[8..]),
            x0: le_u64(&frame[16..]),
        }
    }
}

/// The authentication phase of one accepted connection: magic, secret-length frame,
/// secret frame, judged in that order. On `Ok(())` from [`Handshake::feed_secret`]
/// the caller sends [`AUTHENTICATED`]; on any refusal it sends [`REFUSED`] and may
/// retry with a fresh connection (up to [`MAX_SESSIONS`]).
pub struct Handshake<'s> {
    secret: &'s [u8],
}

impl<'s> Handshake<'s> {
    pub fn new(secret: &'s [u8]) -> Self {
        Handshake { secret }
    }

    /// Judge the 4-byte magic frame.
    pub fn feed_magic(&self, frame: &[u8]) -> Result<(), Refusal> {
        if frame == MAGIC {
            Ok(())
        } else {
            Err(Refusal::BadMagic)
        }
    }

    /// Judge the secret-length frame; `Ok` carries how many secret bytes to read
    /// next. The whole secret frame is read before judgment (no short-circuit on
    /// length — see [`secret_matches`]), which this bound keeps allocation-safe.
    pub fn feed_secret_length(&self, frame: &[u8; SECRET_LEN_FRAME]) -> Result<usize, Refusal> {
        let length = usize::from(u16::from_le_bytes(*frame));
        if length == 0 || length > MAX_SECRET_FRAME {
            Err(Refusal::BadSecretLength { length })
        } else {
            Ok(length)
        }
    }

    /// Judge the secret frame (constant-time, full-frame).
    pub fn feed_secret(&self, wire: &[u8]) -> Result<(), Refusal> {
        if secret_matches(wire, self.secret) {
            Ok(())
        } else {
            Err(Refusal::BadSecret)
        }
    }
}

/// Payload-chunk accounting: the new running offset and how many [`PROGRESS_ACK`]
/// bytes this chunk earned (the caller batches them into one send — transport calls
/// are the expensive unit on this path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub offset: u64,
    pub acks_due: usize,
}

/// The transfer phase of an authenticated session: header, payload stream, trailing
/// CRC, go-ahead. Owns the CRC accumulator and the ack clock; the caller owns the
/// sink and the recv pacing (including swapping to exact-remainder buffers near the
/// end so a recv never swallows the trailing CRC bytes — see oskexec).
pub struct Transfer {
    max_image: u64,
    length: u64,
    offset: u64,
    next_ack: u64,
    crc: Crc32,
}

impl Transfer {
    /// `max_image` is the sink's ceiling: the kexec staging capacity for oskexec, the
    /// stick's fixed slot size for stickflash.
    pub fn new(max_image: u64) -> Self {
        Transfer {
            max_image,
            length: 0,
            offset: 0,
            next_ack: ACK_INTERVAL,
            crc: Crc32::new(),
        }
    }

    /// Parse and judge the 24-byte header; `Ok` arms the payload phase.
    pub fn feed_header(&mut self, frame: &[u8]) -> Result<Header, Refusal> {
        let header = Header::parse(frame);
        if header.length == 0 || header.length > self.max_image {
            return Err(Refusal::BadImageLength {
                length: header.length,
                max: self.max_image,
            });
        }
        self.length = header.length;
        Ok(header)
    }

    /// The header's payload length (0 until [`Transfer::feed_header`] accepts one).
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Payload bytes received so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Payload bytes still owed by the peer.
    pub fn remaining(&self) -> u64 {
        self.length - self.offset
    }

    /// True once the whole payload has been fed.
    pub fn complete(&self) -> bool {
        self.length > 0 && self.offset == self.length
    }

    /// Account one received payload chunk: CRC it, advance the offset, and report
    /// how many acks it earned. The caller must not feed past the header's length
    /// (its exact-remainder recv buffers guarantee that).
    pub fn feed_payload(&mut self, chunk: &[u8]) -> Progress {
        debug_assert!(chunk.len() as u64 <= self.remaining());
        self.crc.update(chunk);
        self.offset += chunk.len() as u64;
        let mut acks_due = 0usize;
        while self.offset >= self.next_ack {
            acks_due += 1;
            self.next_ack += ACK_INTERVAL;
        }
        Progress {
            offset: self.offset,
            acks_due,
        }
    }

    /// Judge the trailing CRC frame against the received bytes; `Ok` carries the
    /// agreed CRC (the caller sends [`VERIFIED`] and hands the CRC to its sink's
    /// commit). `Err` means send [`REFUSED`]: nothing committed.
    pub fn feed_crc(&mut self, frame: &[u8]) -> Result<u32, Refusal> {
        debug_assert_eq!(frame.len(), CRC_FRAME);
        debug_assert!(self.complete());
        let wire = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let computed = self.crc.finalize();
        if wire == computed {
            Ok(computed)
        } else {
            Err(Refusal::CrcMismatch { computed, wire })
        }
    }

    /// Judge the post-verdict go-ahead byte. `Ok` means commit; `Err` means the
    /// session ends with nothing committed (no [`REFUSED`] reply — the verdict
    /// already went out, the peer just failed to confirm it).
    pub fn feed_go_ahead(&self, byte: u8) -> Result<(), Refusal> {
        if byte == GO_AHEAD {
            Ok(())
        } else {
            Err(Refusal::NotGoAhead { byte })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"correct-horse-battery";

    /// The same vector send_image.py --selftest pins: both ends of the wire agree on
    /// what a CRC is.
    #[test]
    fn crc_matches_binascii() {
        assert_eq!(Crc32::of(b"123456789"), 0xCBF4_3926);
        // Streaming in pieces is the same CRC as one shot.
        let mut crc = Crc32::new();
        crc.update(b"1234");
        crc.update(b"56789");
        assert_eq!(crc.finalize(), 0xCBF4_3926);
        assert_eq!(Crc32::of(b""), 0);
    }

    #[test]
    fn secret_compare_is_exact_and_total() {
        assert!(secret_matches(SECRET, SECRET));
        assert!(!secret_matches(b"correct-horse-battery!", SECRET)); // longer
        assert!(!secret_matches(b"correct-horse-batter", SECRET)); // shorter
        assert!(!secret_matches(b"Correct-horse-battery", SECRET)); // one byte off
        assert!(!secret_matches(b"", SECRET));
        assert!(!secret_matches(b"x", b"")); // empty expected never matches non-empty
        assert!(secret_matches(b"", b""));
    }

    #[test]
    fn header_parses_little_endian_fields() {
        let mut frame = [0u8; HEADER_FRAME];
        frame[0..8].copy_from_slice(&0x0020_0000u64.to_le_bytes());
        frame[8..16].copy_from_slice(&0x0123_4567u64.to_le_bytes());
        frame[16..24].copy_from_slice(&0xEB9F_6C38u64.to_le_bytes());
        assert_eq!(
            Header::parse(&frame),
            Header {
                load_addr: 0x0020_0000,
                length: 0x0123_4567,
                x0: 0xEB9F_6C38,
            }
        );
    }

    #[test]
    fn handshake_judges_each_frame() {
        let hs = Handshake::new(SECRET);
        assert_eq!(hs.feed_magic(b"EO9L"), Ok(()));
        assert_eq!(hs.feed_magic(b"EO9l"), Err(Refusal::BadMagic));
        assert_eq!(hs.feed_magic(b"EO9"), Err(Refusal::BadMagic));

        assert_eq!(
            hs.feed_secret_length(&(SECRET.len() as u16).to_le_bytes()),
            Ok(SECRET.len())
        );
        assert_eq!(
            hs.feed_secret_length(&0u16.to_le_bytes()),
            Err(Refusal::BadSecretLength { length: 0 })
        );
        assert_eq!(hs.feed_secret_length(&256u16.to_le_bytes()), Ok(256));
        assert_eq!(
            hs.feed_secret_length(&257u16.to_le_bytes()),
            Err(Refusal::BadSecretLength { length: 257 })
        );

        assert_eq!(hs.feed_secret(SECRET), Ok(()));
        assert_eq!(hs.feed_secret(b"wrong"), Err(Refusal::BadSecret));
    }

    #[test]
    fn transfer_refuses_bad_lengths() {
        let mut t = Transfer::new(1024);
        let mut frame = [0u8; HEADER_FRAME];
        assert_eq!(
            t.feed_header(&frame),
            Err(Refusal::BadImageLength {
                length: 0,
                max: 1024
            })
        );
        frame[8..16].copy_from_slice(&1025u64.to_le_bytes());
        assert_eq!(
            t.feed_header(&frame),
            Err(Refusal::BadImageLength {
                length: 1025,
                max: 1024
            })
        );
        frame[8..16].copy_from_slice(&1024u64.to_le_bytes());
        assert!(t.feed_header(&frame).is_ok());
        assert_eq!(t.length(), 1024);
        assert_eq!(t.remaining(), 1024);
    }

    /// Ack batching: 'k' per 64 KiB crossed, however the chunks slice the stream —
    /// the arithmetic oskexec has always used, pinned.
    #[test]
    fn ack_batching_counts_boundaries_not_chunks() {
        let mut t = Transfer::new(u64::MAX - 1);
        let mut frame = [0u8; HEADER_FRAME];
        frame[8..16].copy_from_slice(&(4 * ACK_INTERVAL).to_le_bytes());
        t.feed_header(&frame).unwrap();

        // 100 KiB: crosses 64 KiB once.
        let chunk = vec![0u8; 100 * 1024];
        assert_eq!(t.feed_payload(&chunk).acks_due, 1);
        // +60 KiB = 160 KiB: crosses 128 KiB once more.
        let chunk = vec![0u8; 60 * 1024];
        assert_eq!(t.feed_payload(&chunk).acks_due, 1);
        // +96 KiB = 256 KiB: crosses 192 KiB and 256 KiB — two acks in one send.
        let chunk = vec![0u8; 96 * 1024];
        let progress = t.feed_payload(&chunk);
        assert_eq!(progress.acks_due, 2);
        assert_eq!(progress.offset, 4 * ACK_INTERVAL);
        assert!(t.complete());
    }

    /// A whole scripted session, replies pinned: the bytes a conforming receiver
    /// sends are exactly oskexec's (A, k…, K), in order.
    #[test]
    fn scripted_good_session() {
        let payload: Vec<u8> = (0u32..200_000).map(|i| (i * 31 % 251) as u8).collect();
        let hs = Handshake::new(SECRET);
        let mut replies: Vec<u8> = Vec::new();

        hs.feed_magic(&MAGIC).unwrap();
        let want = hs
            .feed_secret_length(&(SECRET.len() as u16).to_le_bytes())
            .unwrap();
        assert_eq!(want, SECRET.len());
        hs.feed_secret(SECRET).unwrap();
        replies.push(AUTHENTICATED);

        let mut t = Transfer::new(1024 * 1024);
        let mut frame = [0u8; HEADER_FRAME];
        frame[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        t.feed_header(&frame).unwrap();

        let mut sunk: Vec<u8> = Vec::new(); // the "sink" (stage / cluster overwrite)
        for chunk in payload.chunks(7000) {
            let progress = t.feed_payload(chunk);
            sunk.extend_from_slice(chunk);
            replies.extend(vec![PROGRESS_ACK; progress.acks_due]);
        }
        assert!(t.complete());
        assert_eq!(sunk, payload);

        let crc = t.feed_crc(&Crc32::of(&payload).to_le_bytes()).unwrap();
        assert_eq!(crc, Crc32::of(&payload));
        replies.push(VERIFIED);
        t.feed_go_ahead(GO_AHEAD).unwrap();

        // 200_000 bytes cross the 64 KiB boundary 3 times (192 KiB < 200_000 bytes
        // < 256 KiB): A, kkk, K.
        assert_eq!(replies, b"AkkkK");
    }

    #[test]
    fn scripted_crc_mismatch_and_bad_go_ahead() {
        let payload = vec![0xA5u8; 1000];
        let mut t = Transfer::new(4096);
        let mut frame = [0u8; HEADER_FRAME];
        frame[8..16].copy_from_slice(&1000u64.to_le_bytes());
        t.feed_header(&frame).unwrap();
        t.feed_payload(&payload);
        let computed = Crc32::of(&payload);
        assert_eq!(
            t.feed_crc(&(computed ^ 1).to_le_bytes()),
            Err(Refusal::CrcMismatch {
                computed,
                wire: computed ^ 1
            })
        );

        // A fresh session that verifies but never gets 'G': no commit.
        let mut t = Transfer::new(4096);
        t.feed_header(&frame).unwrap();
        t.feed_payload(&payload);
        t.feed_crc(&computed.to_le_bytes()).unwrap();
        assert_eq!(
            t.feed_go_ahead(b'X'),
            Err(Refusal::NotGoAhead { byte: b'X' })
        );
    }
}
