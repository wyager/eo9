//! curl — the demo HTTP client (docs/board/usb-boot-demo-plan.md Part B).
//!
//! Targets the `eo9-examples:curl/curl` world (see `wit/world.wit`): parse one
//! `[http://]host[:port][/path]` URL — the scheme is optional for the user's spelling
//! (`curl yager.io`; the default is `http://`, applied BEFORE the hardened parse so
//! every injection refusal still runs), resolve the host over UDP DNS when it is not a
//! literal IPv4 address (the shared `eo9-dns` wire core — the l4check encoder,
//! factored out; the resolver is `--resolver` when given, otherwise the first IPv4
//! server the transport capability reports via l4 `dns-servers` — the DHCP lease's
//! offer on the board, QEMU user-net's forwarder 10.0.2.3 under the unconfigured
//! default, and a typed refusal with a `--resolver` hint when the transport knows
//! none), send a single HTTP/1.1 GET with `Connection: close`, and print the
//! status line, the header count, and the body (capped at [`BODY_CAP`], with an
//! honest truncation note). A `Transfer-Encoding: chunked` body is de-framed before
//! printing/counting (`eo9-curl-core::dechunk`, host-tested; the cap applies to the
//! DECODED body) — real HTTP/1.1 servers answer chunked, as example.com did on the
//! board bench; any other transfer-coding refuses typed. At most ONE 301/302 redirect to another http:// URL is
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

// The user-facing manual, embedded as the `eo9-manual` custom section and rendered by
// `man curl` in eosh; the M3 argument-completion hints derive from it additively
// (docs/design/component-manuals.md).
eo9_guest::manual! {
    name: "curl",
    synopsis: "fetch one http URL: a single GET, then the status line, header count, and body",
    description: [
        "Sends one HTTP/1.1 GET (Connection: close) over the granted transport capability and prints the",
        "status line, the header count, and the body (16 KiB cap, honest truncation note; chunked bodies",
        "are de-framed first). The scheme is optional: `curl yager.io` fetches http://yager.io/ (https://",
        "is refused typed — TLS is a deferred decision). Hosts that are not literal IPv4 addresses resolve",
        "over UDP DNS: --resolver when given, otherwise the first IPv4 server the transport reports via",
        "l4 dns-servers (the DHCP lease's offer; QEMU user-net's 10.0.2.3 unconfigured). A transport that",
        "reports none (configured static addressing) refuses typed with a --resolver hint. At most one",
        "301/302 redirect is followed, and a counts line is printed on every exit.",
    ],
    args: [
        { name: "url", ty: "string", required,
          doc: "the URL to fetch; a missing scheme defaults to http:// (https:// is refused: TLS deferred)",
          kind: "url" },
        { name: "resolver", ty: "string", optional,
          doc: "DNS server, dotted quad (default: the transport's dns-servers — lease DNS, or 10.0.2.3)" },
    ],
    examples: [
        { line: "net.virtio $ net.l4.over-l2 $ curl yager.io",
          doc: "QEMU: scheme and resolver both defaulted (user-net's forwarder answers DNS)" },
        { line: "net.rtl8125 $ (net.l4.over-l2 --address dhcp) $ curl yager.io",
          doc: "the board: the DHCP lease supplies the resolver" },
        { line: "net.rtl8125 $ (net.l4.over-l2 --address 10.20.3.70 --gateway 10.20.3.1) $ curl yager.io --resolver 10.20.3.1",
          doc: "configured static addressing names no DNS: pass the resolver" },
    ],
    see_also: "net.l4.over-l2, l4check, telnetd",
}

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

/// Chunked framing the server got wrong, as this world's failure variant.
fn chunk_failure(err: eo9_curl_core::ChunkError) -> ProgramFailure {
    match err {
        eo9_curl_core::ChunkError::BadSizeLine(line) => ProgramFailure::Http(format!(
            "malformed chunked framing: the chunk-size line {line:?} is not hexadecimal"
        )),
        eo9_curl_core::ChunkError::Oversize(size) => ProgramFailure::Http(format!(
            "refused chunked framing: a chunk claims {size} bytes (limit {})",
            eo9_curl_core::CHUNK_SIZE_LIMIT
        )),
        eo9_curl_core::ChunkError::MissingDataTerminator => ProgramFailure::Http(String::from(
            "malformed chunked framing: a chunk's data is not followed by CRLF",
        )),
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
    /// The stored body bytes, still in wire form (chunk framing included when the
    /// server sent `Transfer-Encoding: chunked`); at most [`STORE_CAP`] minus the
    /// head. Decoding and the print cap happen at the final-response report.
    body: Vec<u8>,
    /// How many raw body bytes actually arrived (wire form).
    body_total: usize,
    /// The `Transfer-Encoding` header value, when present: `chunked` is decoded,
    /// anything else refuses typed at the report (raw framing is never printed).
    transfer_encoding: Option<String>,
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
    let mut transfer_encoding: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        header_count += 1;
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("location") {
                location.get_or_insert_with(|| value.trim().to_string());
            }
            // Carried for the final-response body handling: `chunked` is decoded
            // (HTTP/1.1 servers answer our HTTP/1.1 request this way — example.com
            // did on the board bench); anything else refuses typed rather than
            // printing raw framing.
            if name.eq_ignore_ascii_case("transfer-encoding") {
                transfer_encoding.get_or_insert_with(|| value.trim().to_string());
            }
        }
    }

    let body_start = head_end + 4;
    let body_total = received_total.saturating_sub(body_start);
    let body = stored[body_start.min(stored.len())..].to_vec();

    Ok(Response {
        status_line,
        status,
        header_count,
        location,
        body,
        body_total,
        transfer_encoding,
        stopped_early,
    })
}

/// The transfer counters behind the exit counts line.
struct Counts {
    received: usize,
    redirects: u32,
}

/// The resolver the transport capability itself knows: the first IPv4 server from the
/// l4 `dns-servers` introspection (the DHCP lease's offer on the board, QEMU
/// user-net's forwarder under the unconfigured default). Asked only when a host
/// actually needs resolving — literal-IP fetches never depend on it — and a transport
/// that reports none refuses typed, with the `--resolver` hint.
async fn transport_resolver(root: &l4::L4Impl) -> Result<(u8, u8, u8, u8), ProgramFailure> {
    let servers = l4::dns_servers(root).await.map_err(net_failure)?;
    let mut saw_non_v4 = false;
    for server in &servers {
        match server {
            l4::IpAddress::V4(v4) => return Ok(*v4),
            l4::IpAddress::V6(_) => saw_non_v4 = true,
        }
    }
    Err(ProgramFailure::Dns(String::from(if saw_non_v4 {
        "no usable resolver: the transport reports only non-IPv4 DNS servers — \
         pass --resolver <dotted quad>"
    } else {
        "no resolver: the transport reports no DNS servers (configured static \
         addressing names none) — pass --resolver <dotted quad>"
    })))
}

/// The whole fetch: resolve, GET, follow at most one redirect, print the report.
async fn run(
    url_text: &str,
    resolver_text: Option<&str>,
    counts: &mut Counts,
) -> Result<ProgramSuccess, ProgramFailure> {
    // An explicit `--resolver` always wins and is validated up front (a bad flag is a
    // bad flag even when the URL turns out to be a literal address).
    let explicit =
        match resolver_text {
            Some(text) => Some(eo9_curl_core::parse_ipv4(text).ok_or_else(|| {
                ProgramFailure::BadArguments(format!("not a dotted quad: {text:?}"))
            })?),
            None => None,
        };
    let root = l4::default();
    // The user's spelling may omit the scheme (`curl yager.io`): the http:// default
    // is applied BEFORE the hardened parse, so every injection refusal still runs.
    let mut url = eo9_curl_core::parse_with_default_scheme(url_text)
        .map_err(|err| url_failure(err, url_text))?;
    // The transport-reported resolver, asked at most once (memoized across redirects).
    let mut from_transport: Option<(u8, u8, u8, u8)> = None;

    loop {
        let address = match eo9_curl_core::parse_ipv4(&url.host) {
            Some(literal) => literal,
            None => {
                let resolver = match (explicit, from_transport) {
                    (Some(flagged), _) => flagged,
                    (None, Some(known)) => known,
                    (None, None) => {
                        let learned = transport_resolver(&root).await?;
                        say(&format!(
                            "curl: resolver {} (the transport's dns-servers)",
                            format_ip(learned)
                        ));
                        from_transport = Some(learned);
                        learned
                    }
                };
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

        // The final response: status line, header count, body (decoded if chunked,
        // capped, honestly).
        say(&response.status_line);
        say(&format!("curl: {} header(s)", response.header_count));

        // `Transfer-Encoding: chunked` is de-framed before printing/counting (raw
        // chunk-size lines never reach the console — the board-round nit); any
        // other coding refuses typed rather than printing wire framing.
        let raw_kept = response.body.len();
        let chunked = response
            .transfer_encoding
            .as_deref()
            .is_some_and(|te| te.eq_ignore_ascii_case("chunked"));
        let (body, body_count, framing_incomplete) = match &response.transfer_encoding {
            Some(te) if te.eq_ignore_ascii_case("chunked") => {
                let decoded = eo9_curl_core::dechunk(&response.body).map_err(chunk_failure)?;
                let count = decoded.body.len();
                // "The server closed mid-chunk" is only honest when WE did not cut
                // the receive (no early stop, nothing discarded past the store cap).
                let incomplete = !decoded.complete
                    && !(response.stopped_early || response.body_total > raw_kept);
                (decoded.body, count, incomplete)
            }
            Some(te) => {
                return Err(ProgramFailure::Http(format!(
                    "unsupported transfer-encoding {te:?} (only chunked is decoded)"
                )));
            }
            None => {
                let count = response.body_total;
                (response.body, count, false)
            }
        };

        let shown = &body[..body.len().min(BODY_CAP)];
        if !shown.is_empty() {
            let text_shown = String::from_utf8_lossy(shown);
            let _ = text::write_out(&text_shown);
            if !text_shown.ends_with('\n') {
                let _ = text::write_out("\n");
            }
        }
        if body_count > shown.len() {
            say(&format!(
                "curl: body truncated: {} of {} byte(s) shown",
                shown.len(),
                body_count
            ));
        }
        if framing_incomplete {
            say("curl: the server closed mid-chunk (the chunked framing never completed)");
        }
        if chunked && response.body_total > raw_kept {
            // Chunked counting runs over what was stored: say so when raw bytes
            // beyond the store bound were discarded (the decoded count is a floor).
            say(&format!(
                "curl: decoded from the first {raw_kept} of {} raw body byte(s)",
                response.body_total
            ));
        }
        if response.stopped_early {
            say("curl: stopped receiving before the server closed (receive bound hit)");
        }
        return Ok(ProgramSuccess::Fetched(format!(
            "{}; {} header(s); {} body byte(s); {} redirect(s)",
            response.status_line, response.header_count, body_count, counts.redirects
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
