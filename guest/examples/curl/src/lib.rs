//! curl — the demo HTTP client (docs/board/usb-boot-demo-plan.md Part B).
//!
//! Targets the `eo9-examples:curl/curl` world (see `wit/world.wit`): parse one
//! `http://host[:port][/path]` URL, resolve the host over UDP DNS when it is not a
//! literal IPv4 address (the shared `eo9-dns` wire core — the l4check encoder,
//! factored out), send a single HTTP/1.1 GET with `Connection: close`, and print the
//! status line, the header count, and the body (capped at [`BODY_CAP`], with an
//! honest truncation note). At most ONE 301/302 redirect to another http:// URL is
//! followed; `https://` — typed in the URL or arriving as a redirect — is refused
//! with a typed message naming the deferred-TLS decision (certificate validation
//! needs wall-clock time and entropy provenance the hardware does not carry yet; the
//! post-demo TLS lane in the demo plan). Every failure is typed, never a trap, and a
//! counts line (bytes received, redirects followed) is printed on every exit so each
//! bench run advances a round (one-run-one-round).
//!
//! Known caveat, recorded not solved — the l4 graceful-close gap (plan/09 D44,
//! GAPS.md): dropping a `tcp-connection` queues our FIN but nothing pumps it out
//! after the last l4 operation, so the server may see our side of the close
//! handshake late or never. Harmless for a GET client — the server has already sent
//! its FIN (`Connection: close`) and every peer times a half-open close out — but
//! every l4 consumer carries this until the WIT grows `close: async func`.
//!
//! Security note: the host and path feed the request line verbatim, so URLs (and
//! server-supplied `Location` redirect targets — exactly as untrusted) are refused
//! typed when they carry spaces or control bytes. The rule lives, host-tested, in
//! `eo9-curl-core` (the request-injection refusal).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eo9_curl_core::{Url, UrlError};
use eo9_guest::api::net::l4;
use eo9_guest::buffer;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "curl",
    apis: [io, net_l4, text],
});

/// The DNS forwarder QEMU user-mode networking runs for its guest (the default
/// `--resolver`; on the bench LAN pass the router, `--resolver 10.20.3.1`).
const DEFAULT_RESOLVER: (u8, u8, u8, u8) = (10, 0, 2, 3);
/// A fixed DNS query id (the reply must echo it back; the l4check convention).
const QUERY_ID: u16 = 0xe09;
/// How many datagrams to inspect before giving up on the DNS answer.
const DNS_RECEIVE_ATTEMPTS: u32 = 4;
/// How much of the body is printed (and carried) before the honest truncation note.
const BODY_CAP: usize = 16 * 1024;
/// How much response gets stored at all: the header section must fit here together
/// with the printable body prefix (headers beyond this fail typed, not silently).
const STORE_CAP: usize = BODY_CAP + 16 * 1024;
/// The receive loop's total-bytes bound: past this the loop stops without waiting
/// for the server's FIN (noted on the console; the body note stays honest).
const RECEIVE_CAP: usize = 256 * 1024;
/// One receive buffer.
const RECV_CHUNK: u64 = 4096;
/// Consecutive bounded-await timeouts tolerated mid-response before giving up. Each
/// l4 `recv` carries the provider's own deadline (seconds-scale), so this bounds the
/// whole loop's idle time.
const RECV_TIMEOUT_LIMIT: u32 = 8;
/// How many 301/302 hops are followed (the fixed-scope decision: one).
const MAX_REDIRECTS: u32 = 1;

/// The l4 API's own error, rendered into the world's failure variant.
fn net_failure(err: l4::L4Error) -> ProgramFailure {
    match err {
        l4::L4Error::Denied => ProgramFailure::Denied,
        other => ProgramFailure::Net(format!("{other:?}")),
    }
}

/// One console line; a missing/refusing console never fails the fetch.
fn say(line: &str) {
    let _ = text::write_out_line(line);
}

/// The one https refusal message (the deferred-TLS decision, named).
fn https_refused() -> ProgramFailure {
    ProgramFailure::Unsupported(String::from(
        "https:// is refused: TLS is deferred by decision (certificate validation \
         needs wall-clock time and entropy provenance the hardware does not carry \
         yet; docs/board/usb-boot-demo-plan.md) — use an http:// URL",
    ))
}

/// A user-supplied URL the core refused, as this world's failure variant.
fn url_failure(err: UrlError, url: &str) -> ProgramFailure {
    match err {
        UrlError::Https => https_refused(),
        UrlError::NotHttp => ProgramFailure::BadArguments(format!("not an http:// URL: {url:?}")),
        UrlError::ForbiddenByte => ProgramFailure::BadArguments(format!(
            "spaces and control bytes in a URL are refused (they would reach the \
             request line — percent-encode them): {url:?}"
        )),
        UrlError::Userinfo => ProgramFailure::BadArguments(String::from(
            "userinfo (user@host) in the URL is not supported",
        )),
        UrlError::Ipv6Literal => {
            ProgramFailure::BadArguments(String::from("IPv6 literals in the URL are not supported"))
        }
        UrlError::BadPort(port) => ProgramFailure::BadArguments(format!("not a port: {port:?}")),
        UrlError::NoHost => ProgramFailure::BadArguments(format!("the URL has no host: {url:?}")),
    }
}

/// A server-supplied `Location` the core refused: https stays the typed TLS
/// refusal; everything else is the server's fault (`http`), not the caller's.
fn redirect_failure(err: UrlError, location: &str) -> ProgramFailure {
    match err {
        UrlError::Https => https_refused(),
        UrlError::ForbiddenByte => ProgramFailure::Http(format!(
            "the redirect target carries spaces or control bytes (refused — \
             request injection): {location:?}"
        )),
        other => ProgramFailure::Http(format!("unusable redirect target {location:?}: {other:?}")),
    }
}

/// `a.b.c.d`.
fn format_ip(ip: (u8, u8, u8, u8)) -> String {
    format!("{}.{}.{}.{}", ip.0, ip.1, ip.2, ip.3)
}

/// Resolve `host` to its first A record via one UDP question to `resolver`:53
/// (the l4check round-trip shape, over the shared `eo9-dns` wire core).
async fn resolve(
    root: &l4::L4Impl,
    host: &str,
    resolver: (u8, u8, u8, u8),
) -> Result<(u8, u8, u8, u8), ProgramFailure> {
    let query = eo9_dns::query(QUERY_ID, host.split('.')).map_err(|err| {
        ProgramFailure::BadArguments(format!("{host:?} is not a resolvable name: {err:?}"))
    })?;

    let socket = l4::bind_udp(
        root,
        l4::SocketAddress {
            address: l4::IpAddress::V4((0, 0, 0, 0)),
            port: 0,
        },
    )
    .await
    .map_err(net_failure)?;

    let remote = l4::SocketAddress {
        address: l4::IpAddress::V4(resolver),
        port: 53,
    };
    let (_query, sent) = l4::send_to(&socket, remote, buffer::from_bytes(&query)).await;
    sent.map_err(net_failure)?;

    let mut last_problem = String::from("no datagram came back");
    for _ in 0..DNS_RECEIVE_ATTEMPTS {
        let dst = buffer::with_capacity(1536);
        let (dst, received) = l4::recv_from(&socket, dst).await;
        match received {
            Ok((result, _from)) => {
                let datagram = buffer::prefix_to_vec(&dst, result.bytes_received);
                match eo9_dns::parse_reply(&datagram, QUERY_ID) {
                    Some(Ok(eo9_dns::Reply::A(a, b, c, d))) => return Ok((a, b, c, d)),
                    Some(Ok(eo9_dns::Reply::Answered(records))) => {
                        last_problem = format!(
                            "the resolver answered ({records} record(s)) but none was an A record"
                        );
                        break;
                    }
                    Some(Err(eo9_dns::ReplyError::Rcode(rcode))) => {
                        last_problem = format!("the resolver answered with rcode {rcode}");
                        break;
                    }
                    Some(Err(eo9_dns::ReplyError::NoRecords)) => {
                        last_problem = String::from("the resolver answered with no records");
                        break;
                    }
                    None => {
                        last_problem = format!(
                            "received {} byte(s) that were not our answer",
                            result.bytes_received
                        );
                    }
                }
            }
            Err(l4::L4Error::Denied) => return Err(ProgramFailure::Denied),
            Err(l4::L4Error::TimedOut) => {
                last_problem = String::from("timed out waiting for the resolver");
                break;
            }
            Err(other) => {
                last_problem = format!("{other:?}");
                break;
            }
        }
    }
    Err(ProgramFailure::Dns(format!(
        "resolving {host:?} via {}: {last_problem}",
        format_ip(resolver)
    )))
}

/// What one GET brought back.
struct Response {
    status_line: String,
    status: u16,
    header_count: usize,
    location: Option<String>,
    /// The body prefix that gets printed (at most [`BODY_CAP`] bytes).
    body: Vec<u8>,
    /// How many body bytes actually arrived.
    body_total: usize,
    /// The receive loop stopped before the server's FIN (cap or idle limit).
    stopped_early: bool,
}

/// First position of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// One GET against `address`:`url.port`: send the whole request, receive to FIN
/// (bounded), parse the head. `counts` accrues every byte that arrived.
async fn fetch(
    root: &l4::L4Impl,
    address: (u8, u8, u8, u8),
    url: &Url,
    counts: &mut Counts,
) -> Result<Response, ProgramFailure> {
    let remote = l4::SocketAddress {
        address: l4::IpAddress::V4(address),
        port: url.port,
    };
    let connection = l4::connect(root, remote).await.map_err(net_failure)?;

    // The one request this program ever sends.
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: */*\r\n\r\n",
        url.path,
        url.host_header()
    );
    let bytes = request.as_bytes();
    let mut sent_total = 0usize;
    while sent_total < bytes.len() {
        let (_chunk, sent) = l4::send(&connection, buffer::from_bytes(&bytes[sent_total..])).await;
        let sent = sent.map_err(net_failure)?;
        let advanced = sent.bytes_sent as usize;
        if advanced == 0 {
            return Err(ProgramFailure::Net(String::from(
                "send accepted 0 bytes; no progress",
            )));
        }
        sent_total += advanced;
    }

    // Receive to FIN, bounded three ways: total bytes ([`RECEIVE_CAP`]), consecutive
    // provider-deadline timeouts ([`RECV_TIMEOUT_LIMIT`]), and what gets *stored*
    // ([`STORE_CAP`] — everything past it is counted, not kept).
    let mut stored: Vec<u8> = Vec::new();
    let mut received_total = 0usize;
    let mut consecutive_timeouts = 0u32;
    let mut stopped_early = false;
    loop {
        if received_total >= RECEIVE_CAP {
            stopped_early = true;
            break;
        }
        let dst = buffer::with_capacity(RECV_CHUNK);
        let (dst, received) = l4::recv(&connection, dst).await;
        match received {
            // FIN: the server is done (it promised `Connection: close`).
            Ok(result) if result.bytes_received == 0 => break,
            Ok(result) => {
                consecutive_timeouts = 0;
                let chunk = buffer::prefix_to_vec(&dst, result.bytes_received);
                received_total += chunk.len();
                let room = STORE_CAP.saturating_sub(stored.len());
                stored.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Err(l4::L4Error::Denied) => return Err(ProgramFailure::Denied),
            Err(l4::L4Error::TimedOut) => {
                consecutive_timeouts += 1;
                if consecutive_timeouts >= RECV_TIMEOUT_LIMIT {
                    if received_total == 0 {
                        return Err(ProgramFailure::Http(String::from(
                            "the server sent nothing before the receive window closed",
                        )));
                    }
                    stopped_early = true;
                    break;
                }
            }
            // A reset (or torn-down connection) after data arrived: some servers cut
            // the connection instead of lingering through the close handshake — what
            // arrived is the response.
            Err(l4::L4Error::ConnectionReset) | Err(l4::L4Error::NotConnected)
                if received_total > 0 =>
            {
                break;
            }
            Err(other) => return Err(ProgramFailure::Net(format!("{other:?}"))),
        }
    }
    counts.received += received_total;

    // Parse the head out of what was stored.
    let Some(head_end) = find(&stored, b"\r\n\r\n") else {
        return Err(ProgramFailure::Http(if stored.len() >= STORE_CAP {
            format!("the header section exceeded {STORE_CAP} bytes")
        } else {
            format!(
                "the response ended before the header section completed \
                 ({received_total} byte(s) received)"
            )
        }));
    };
    let head_text = String::from_utf8_lossy(&stored[..head_end]).into_owned();
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().unwrap_or("").to_string();
    let mut parts = status_line.split_whitespace();
    let version_ok = parts
        .next()
        .is_some_and(|version| version.starts_with("HTTP/"));
    let status = parts.next().and_then(|code| code.parse::<u16>().ok());
    let (true, Some(status)) = (version_ok, status) else {
        return Err(ProgramFailure::Http(format!(
            "malformed status line: {status_line:?}"
        )));
    };

    let mut header_count = 0usize;
    let mut location: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        header_count += 1;
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("location")
        {
            location.get_or_insert_with(|| value.trim().to_string());
        }
    }

    let body_start = head_end + 4;
    let body_total = received_total.saturating_sub(body_start);
    let stored_body = &stored[body_start.min(stored.len())..];
    let body = stored_body[..stored_body.len().min(BODY_CAP)].to_vec();

    Ok(Response {
        status_line,
        status,
        header_count,
        location,
        body,
        body_total,
        stopped_early,
    })
}

/// The transfer counters behind the exit counts line.
struct Counts {
    received: usize,
    redirects: u32,
}

/// The whole fetch: resolve, GET, follow at most one redirect, print the report.
async fn run(
    url_text: &str,
    resolver_text: Option<&str>,
    counts: &mut Counts,
) -> Result<ProgramSuccess, ProgramFailure> {
    let resolver = match resolver_text {
        Some(text) => eo9_curl_core::parse_ipv4(text)
            .ok_or_else(|| ProgramFailure::BadArguments(format!("not a dotted quad: {text:?}")))?,
        None => DEFAULT_RESOLVER,
    };
    let root = l4::default();
    let mut url = eo9_curl_core::parse(url_text).map_err(|err| url_failure(err, url_text))?;

    loop {
        let address = match eo9_curl_core::parse_ipv4(&url.host) {
            Some(literal) => literal,
            None => {
                let address = resolve(&root, &url.host, resolver).await?;
                say(&format!(
                    "curl: resolved {} -> {}",
                    url.host,
                    format_ip(address)
                ));
                address
            }
        };

        let response = fetch(&root, address, &url, counts).await?;

        if response.status == 301 || response.status == 302 {
            let Some(location) = response.location else {
                return Err(ProgramFailure::Http(format!(
                    "a {} redirect without a Location header",
                    response.status
                )));
            };
            if counts.redirects >= MAX_REDIRECTS {
                return Err(ProgramFailure::Http(format!(
                    "a second redirect ({} -> {location:?}); curl follows at most one",
                    response.status
                )));
            }
            // The Location value is server-supplied: same refusals as a user URL
            // (https typed, control bytes/spaces refused) before any connection.
            url = eo9_curl_core::redirect_target(&url, &location)
                .map_err(|err| redirect_failure(err, &location))?;
            counts.redirects += 1;
            say(&format!(
                "curl: {} redirect; following {}",
                response.status,
                url.display()
            ));
            continue;
        }

        // The final response: status line, header count, body (capped, honestly).
        say(&response.status_line);
        say(&format!("curl: {} header(s)", response.header_count));
        if !response.body.is_empty() {
            let shown = String::from_utf8_lossy(&response.body);
            let _ = text::write_out(&shown);
            if !shown.ends_with('\n') {
                let _ = text::write_out("\n");
            }
        }
        if response.body_total > response.body.len() {
            say(&format!(
                "curl: body truncated: {} of {} byte(s) shown",
                response.body.len(),
                response.body_total
            ));
        }
        if response.stopped_early {
            say("curl: stopped receiving before the server closed (receive bound hit)");
        }
        return Ok(ProgramSuccess::Fetched(format!(
            "{}; {} header(s); {} body byte(s); {} redirect(s)",
            response.status_line, response.header_count, response.body_total, counts.redirects
        )));
    }
}

eo9_guest::main! {
    async fn main(
        url: String,
        resolver: Option<String>,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        let mut counts = Counts { received: 0, redirects: 0 };
        let outcome = run(&url, resolver.as_deref(), &mut counts).await;
        // The counts line, success or failure: every run reports what moved
        // (one-run-one-round).
        say(&format!(
            "curl: {} byte(s) received, {} redirect(s) followed",
            counts.received, counts.redirects
        ));
        outcome
    }
}
