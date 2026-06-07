//! `net.text` — standard text streams served over a TCP connection.
//!
//! Targets the crate-local `eo9:net-text/net-text` world: imports the transport layer
//! (`eo9:net/l4` — whichever provider is composed below it) and exports `eo9:text/text`,
//! so the unmodified shell becomes a network shell by composition alone:
//!
//! ```text
//! net.virtio $ net.l4.over-l2 $ net.text $ eosh
//! ```
//!
//! **SECURITY: cleartext, unauthenticated.** This provider speaks a minimal telnet-style
//! NVT with no authentication and no encryption: whoever can reach the configured port
//! owns the composed session. It is a trusted-LAN/dev tool (the QEMU slirp-hostfwd
//! bring-up path), never an exposure surface; SSH is explicitly deferred (owner ruling).
//!
//! Behavior, in the order a session sees it:
//!
//! * **One session per instance.** The first `read-line` brings the provider up: it
//!   listens on the configured port (default 23), accepts exactly one connection, then
//!   **drops the listener** — later connection attempts are answered by the transport
//!   itself (a TCP RST: immediate, deterministic refusal). Concurrent sessions need
//!   concurrent stacks, which a single fused task cannot hold (see plan/09); the
//!   supervisor (`telnetd`) serves sessions sequentially instead.
//! * **Lines in.** Received bytes pass a refuse-all telnet negotiator (every `WILL` is
//!   answered `DONT`, every `DO` answered `WONT`, negatives are never answered,
//!   subnegotiations are skipped, `IAC IAC` is a literal 0xff; nothing else of the
//!   telnet protocol is implemented — no ECHO, no SGA, no NAWS, no urgent data). Lines
//!   end at CR LF, CR NUL, bare CR, or bare LF; the line cap is [`LINE_CAP`] bytes
//!   (longer input is split). Non-UTF-8 bytes are replaced (`from_utf8_lossy`).
//! * **Text out.** `write` is synchronous in the WIT, so output is buffered (capped at
//!   [`OUT_CAP`]; overflow is dropped and flagged once) and **delivered at `read-line`
//!   boundaries** — exactly right for a prompt-driven consumer like eosh, which writes
//!   its prompt immediately before reading. `\n` is sent as CR LF; `out` and `err`
//!   share the connection in write order.
//! * **Session end.** The peer closing the connection is end of input: `read-line`
//!   answers `none` and the consumer above exits cleanly. The exact line `exit` is
//!   intercepted *here*, at the NVT layer: the provider sends a goodbye, closes the
//!   connection (FIN), and answers `none` — because after the consumer exits nothing
//!   would ever run this provider again to perform the close handshake (the recorded
//!   l4 gap: there is no explicit `close`/flush operation, so the FIN is pumped out by
//!   a bounded throwaway `accept` on an ephemeral listener — see plan/09).
//!
//! Everything is bounded per step: accepts and receives ride the transport's own
//! per-operation deadlines (`timed-out` is retried, so an idle session waits like a
//! console does), buffers are capped, and transport errors are typed text errors —
//! never traps.

#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "net-text",
    path: "wit",
    // Pull in bindings for eo9:io/buffers, which the imported l4 interface uses but
    // the world does not name directly.
    generate_all,
});

use eo9::net::l4;
use exports::eo9::text::net_text_config;
use exports::eo9::text::text::{self, OutputStream, TextError};
use exports::eo9::text::types;

// ------------------------------------------------------------------------------------------
// Bounds and defaults (all documented in the crate header).
// ------------------------------------------------------------------------------------------

/// The documented default port (telnet), used when the provider is composed without
/// `configure`.
const DEFAULT_PORT: u16 = 23;
/// Pending-output cap: bytes buffered between `read-line` boundaries. Overflow is
/// dropped (and flagged once), never a trap.
const OUT_CAP: usize = 64 * 1024;
/// Input line cap: a longer line is split at the cap.
const LINE_CAP: usize = 4096;
/// One receive buffer (matches the transport's typical segment ceiling).
const RECV_BUFFER_BYTES: u64 = 2048;
/// One send chunk (stays well inside the transport's per-direction buffer).
const SEND_CHUNK: usize = 4096;

/// What the session greets a fresh connection with — the security posture, said loudly.
const GREETING: &[u8] =
    b"eo9 net.text: cleartext telnet session - unauthenticated; trusted networks only\r\n";
/// The goodbye sent when the NVT layer intercepts `exit` and closes the connection.
const GOODBYE: &[u8] = b"eo9 net.text: session closed\r\n";
/// The one-time marker appended after buffered output had to be dropped.
const DROPPED_MARKER: &[u8] = b"\r\n[net.text: some output was dropped]\r\n";

// ------------------------------------------------------------------------------------------
// Telnet NVT: IAC command bytes (RFC 854/855), refuse-all negotiation.
// ------------------------------------------------------------------------------------------

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

/// The telnet command parser's state, kept across receive segments.
#[derive(Clone, Copy)]
enum Nvt {
    /// Plain data bytes.
    Data,
    /// Saw IAC; the next byte is a command.
    Iac,
    /// Saw IAC WILL/WONT/DO/DONT; the next byte is the option.
    Verb(u8),
    /// Inside a subnegotiation (IAC SB … IAC SE); bytes are skipped.
    Sub,
    /// Inside a subnegotiation and saw IAC.
    SubIac,
}

// ------------------------------------------------------------------------------------------
// Provider state.
// ------------------------------------------------------------------------------------------

/// Where the session is in its life.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No connection yet; the first `read-line` listens and accepts.
    Idle,
    /// One connection live (the handle sits in `conn`, taken out around awaits).
    Live,
    /// The session ended (peer closed, `exit` intercepted, or transport failure):
    /// `read-line` answers `none`, `write` answers `closed`, forever.
    Closed,
}

struct NetText {
    phase: Phase,
    /// The accepted connection; `None` while an operation has it taken out.
    conn: Option<l4::TcpConnection>,
    /// Pending output (already NVT-encoded), delivered at `read-line` boundaries.
    out: Vec<u8>,
    /// Whether output had to be dropped since the last marker.
    out_dropped: bool,
    /// Complete input lines not yet handed to the consumer.
    lines: VecDeque<String>,
    /// Bytes of the current, incomplete line.
    partial: Vec<u8>,
    /// The telnet command parser's state.
    nvt: Nvt,
    /// A just-seen CR ended a line; an immediately following LF or NUL is swallowed.
    swallow_after_cr: bool,
}

impl NetText {
    const fn new() -> NetText {
        NetText {
            phase: Phase::Idle,
            conn: None,
            out: Vec::new(),
            out_dropped: false,
            lines: VecDeque::new(),
            partial: Vec::new(),
            nvt: Nvt::Data,
            swallow_after_cr: false,
        }
    }

    /// Queue raw, already-NVT-encoded bytes for delivery, honoring the cap.
    fn queue_out(&mut self, bytes: &[u8]) {
        let room = OUT_CAP.saturating_sub(self.out.len());
        if room >= bytes.len() {
            self.out.extend_from_slice(bytes);
        } else {
            self.out.extend_from_slice(&bytes[..room]);
            self.out_dropped = true;
        }
    }

    /// Finish the current partial line (lossy UTF-8) into the line queue.
    fn end_line(&mut self) {
        let line = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        self.lines.push_back(line);
    }

    /// Feed one received byte through the telnet negotiator and the line assembler.
    /// Negotiation refusals are queued as output (flushed at the next boundary).
    fn feed(&mut self, byte: u8) {
        match self.nvt {
            Nvt::Data => match byte {
                IAC => self.nvt = Nvt::Iac,
                b'\r' => {
                    self.end_line();
                    self.swallow_after_cr = true;
                }
                b'\n' | 0 if self.swallow_after_cr => {
                    self.swallow_after_cr = false;
                }
                b'\n' => self.end_line(),
                other => {
                    self.swallow_after_cr = false;
                    self.partial.push(other);
                    if self.partial.len() >= LINE_CAP {
                        self.end_line();
                    }
                }
            },
            Nvt::Iac => match byte {
                IAC => {
                    // Escaped literal 0xff data byte.
                    self.partial.push(IAC);
                    self.nvt = Nvt::Data;
                }
                WILL | WONT | DO | DONT => self.nvt = Nvt::Verb(byte),
                SB => self.nvt = Nvt::Sub,
                // NOP, DM, BRK, IP, AO, AYT, EC, EL, GA, …: ignored.
                _ => self.nvt = Nvt::Data,
            },
            Nvt::Verb(verb) => {
                // Refuse-all negotiation: a WILL is answered DONT, a DO answered WONT;
                // negatives (WONT/DONT) are never answered — answering a refusal is how
                // option-negotiation loops start (RFC 854).
                match verb {
                    WILL => self.queue_out(&[IAC, DONT, byte]),
                    DO => self.queue_out(&[IAC, WONT, byte]),
                    _ => {}
                }
                self.nvt = Nvt::Data;
            }
            Nvt::Sub => {
                if byte == IAC {
                    self.nvt = Nvt::SubIac;
                }
            }
            Nvt::SubIac => {
                self.nvt = if byte == SE { Nvt::Data } else { Nvt::Sub };
            }
        }
    }
}

static STATE: ProviderState<NetText> = ProviderState::new();
/// Set exactly once, by `configure`; absent for an unconfigured provider (default 23).
static PORT: ProviderState<u16> = ProviderState::new();

fn with_state<R>(f: impl FnOnce(&mut NetText) -> R) -> R {
    if !STATE.is_set() {
        STATE.set(NetText::new());
    }
    STATE.with(f)
}

fn port() -> u16 {
    if PORT.is_set() {
        PORT.with(|p| *p)
    } else {
        DEFAULT_PORT
    }
}

fn io_error(doing: &str, err: l4::L4Error) -> TextError {
    TextError::Io(format!("net.text: {doing}: {err:?}"))
}

/// The transport's "wait expired" answer — retried (an idle console waits forever too).
fn timed_out(err: &l4::L4Error) -> bool {
    matches!(err, l4::L4Error::TimedOut)
}

/// Errors that mean the session is over rather than broken (the peer went away).
fn peer_gone(err: &l4::L4Error) -> bool {
    matches!(
        err,
        l4::L4Error::ConnectionReset | l4::L4Error::NotConnected
    )
}

// ------------------------------------------------------------------------------------------
// Connection plumbing: take the handle out around awaits (the ProviderState discipline:
// no borrow is ever held across an await), put it back when the operation completes.
// ------------------------------------------------------------------------------------------

fn take_conn() -> Result<l4::TcpConnection, TextError> {
    with_state(|s| s.conn.take()).ok_or_else(|| {
        TextError::Io(String::from(
            "net.text: another text operation on this session is in progress",
        ))
    })
}

fn put_conn(conn: l4::TcpConnection) {
    with_state(|s| s.conn = Some(conn));
}

/// Send everything queued. On a dead peer the session closes and `Err` comes back;
/// the caller decides whether that is end-of-input (`read-line`) or an error.
async fn flush() -> Result<(), TextError> {
    loop {
        let chunk: Vec<u8> = with_state(|s| {
            if s.out.is_empty() {
                if s.out_dropped {
                    s.out_dropped = false;
                    s.out.extend_from_slice(DROPPED_MARKER);
                } else {
                    return Vec::new();
                }
            }
            let take = s.out.len().min(SEND_CHUNK);
            s.out.drain(..take).collect()
        });
        if chunk.is_empty() {
            return Ok(());
        }
        let conn = take_conn()?;
        let buffer = eo9::io::buffers::Buffer::new(chunk.len() as u64);
        buffer.write(0, &chunk);
        let (_buffer, sent) = l4::send(&conn, buffer).await;
        put_conn(conn);
        match sent {
            Ok(result) => {
                let queued = (result.bytes_sent as usize).min(chunk.len());
                if queued < chunk.len() {
                    // Put the unsent tail back at the front and try again.
                    with_state(|s| {
                        let mut rest = chunk[queued..].to_vec();
                        rest.extend_from_slice(&s.out);
                        s.out = rest;
                    });
                }
            }
            Err(err) if peer_gone(&err) => {
                close_now();
                return Err(TextError::Closed);
            }
            Err(err) => return Err(io_error("send", err)),
        }
    }
}

/// Drop the connection immediately (no goodbye, no FIN pumping — for paths where the
/// peer is already gone or the transport failed).
fn close_now() {
    with_state(|s| {
        s.conn = None; // dropping the handle queues the transport-level close
        s.phase = Phase::Closed;
    });
}

/// Orderly close: flush what is pending (plus `goodbye`), drop the connection (which
/// queues the FIN in the transport), then pump the close handshake out with a bounded
/// throwaway accept on an ephemeral listener — the transport has no explicit
/// close/flush operation, and after the consumer exits nothing would ever run this
/// provider again (the recorded l4 gap; see the crate header).
async fn close_session(goodbye: Option<&[u8]>) {
    if let Some(text) = goodbye {
        with_state(|s| s.queue_out(text));
    }
    let _ = flush().await;
    let still_open = with_state(|s| {
        let open = s.conn.is_some();
        s.conn = None;
        s.phase = Phase::Closed;
        open
    });
    if still_open {
        // Bounded: listen on an ephemeral port and accept once. Nothing will connect;
        // the accept's own deadline (a few seconds) pumps the FIN/ACK exchange of the
        // just-dropped connection through the link, then the listener is dropped.
        let root = l4::default();
        let any = l4::SocketAddress {
            address: l4::IpAddress::V4((0, 0, 0, 0)),
            port: 0,
        };
        if let Ok(listener) = l4::listen(&root, any).await {
            let _ = l4::accept(&listener).await;
        }
    }
}

/// First-use bring-up: listen on the configured port, accept exactly one connection,
/// drop the listener (refusal-by-RST for everyone else), queue the greeting.
async fn bring_up() -> Result<(), TextError> {
    let root = l4::default();
    let local = l4::SocketAddress {
        address: l4::IpAddress::V4((0, 0, 0, 0)),
        port: port(),
    };
    let listener = l4::listen(&root, local)
        .await
        .map_err(|err| io_error("listen", err))?;
    let conn = loop {
        match l4::accept(&listener).await {
            Ok((conn, _peer)) => break conn,
            // The transport's per-operation deadline expired with nobody there yet:
            // keep waiting, exactly like a console waiting for its first keystroke.
            Err(err) if timed_out(&err) => continue,
            Err(err) => return Err(io_error("accept", err)),
        }
    };
    drop(listener);
    with_state(|s| {
        s.conn = Some(conn);
        s.phase = Phase::Live;
        s.queue_out(GREETING);
    });
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Resource representations and the exported surface.
// ------------------------------------------------------------------------------------------

/// The `net.text` provider.
struct Stub;

/// The root-handle resource: a token — the session lives in [`STATE`].
struct NetTextRoot;

impl types::Guest for Stub {
    type TextImpl = NetTextRoot;
}

impl types::GuestTextImpl for NetTextRoot {}

impl net_text_config::Guest for Stub {
    /// Bind the TCP port. Validation happens here, at compose time: port 0 (the
    /// transport's "pick one for me") would make the session unreachable-by-plan,
    /// so it is a configure error, never a trap.
    fn configure(port: u16) -> Result<types::TextImpl, String> {
        if port == 0 {
            return Err(String::from(
                "net.text: port must be 1..=65535 (0 would bind an unpredictable ephemeral port)",
            ));
        }
        PORT.set(port);
        Ok(types::TextImpl::new(NetTextRoot))
    }
}

impl text::Guest for Stub {
    fn default() -> types::TextImpl {
        types::TextImpl::new(NetTextRoot)
    }

    /// Buffer output for delivery at the next `read-line` boundary (`write` is sync in
    /// the WIT; the transport is async — see the crate header). `out` and `err` share
    /// the connection in write order.
    fn write(
        _t: text::TextImplBorrow<'_>,
        _to: OutputStream,
        text: String,
    ) -> Result<(), TextError> {
        with_state(|s| {
            if s.phase == Phase::Closed {
                return Err(TextError::Closed);
            }
            // NVT line discipline: a newline on the wire is CR LF.
            let bytes = text.as_bytes();
            let mut encoded: Vec<u8> = Vec::with_capacity(bytes.len() + 8);
            for &byte in bytes {
                if byte == b'\n' {
                    encoded.extend_from_slice(b"\r\n");
                } else {
                    encoded.push(byte);
                }
            }
            s.queue_out(&encoded);
            Ok(())
        })
    }

    async fn read_line(_t: text::TextImplBorrow<'_>) -> Result<Option<String>, TextError> {
        match with_state(|s| s.phase) {
            Phase::Closed => return Ok(None),
            Phase::Idle => bring_up().await?,
            Phase::Live => {}
        }

        loop {
            // Deliver everything queued (the prompt that was just written, negotiation
            // refusals, the greeting on the first pass). A dead peer is end of input.
            match flush().await {
                Ok(()) => {}
                Err(TextError::Closed) => return Ok(None),
                Err(err) => return Err(err),
            }

            if let Some(line) = with_state(|s| s.lines.pop_front()) {
                // The NVT layer owns connection teardown: `exit` ends the session here
                // (goodbye, FIN), and the consumer above sees a clean end of input —
                // identical observable behavior, no dangling socket (crate header).
                if line.trim() == "exit" {
                    close_session(Some(GOODBYE)).await;
                    return Ok(None);
                }
                return Ok(Some(line));
            }

            let conn = take_conn()?;
            let dst = eo9::io::buffers::Buffer::new(RECV_BUFFER_BYTES);
            let (dst, received) = l4::recv(&conn, dst).await;
            put_conn(conn);
            match received {
                Ok(result) if result.bytes_received == 0 => {
                    // Peer closed. Hand over a final unterminated line if there is one
                    // (the next call answers `none`), then send our own FIN.
                    let last = with_state(|s| {
                        if s.partial.is_empty() {
                            None
                        } else {
                            s.end_line();
                            s.lines.pop_front()
                        }
                    });
                    close_session(None).await;
                    return Ok(last);
                }
                Ok(result) => {
                    let bytes = dst.read(0, result.bytes_received.min(RECV_BUFFER_BYTES));
                    with_state(|s| {
                        for byte in bytes {
                            s.feed(byte);
                        }
                    });
                }
                Err(err) if timed_out(&err) => continue,
                Err(err) if peer_gone(&err) => {
                    close_now();
                    return Ok(None);
                }
                Err(err) => return Err(io_error("recv", err)),
            }
        }
    }
}

export!(Stub);
